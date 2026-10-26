//! P4.2.3 pgwire 连接水位线集成测试：真实 loopback TCP + 生产入口函数。
//! max=1 时第一条连接握手后占位存活；第二条连接握手照常完成（AuthenticationOk），
//! 随后在裁决点收协议内 FATAL 53300（与 Postgres too_many_connections 同款），
//! 而不是不明 EOF；第一条断开后名额归还，新连接可再入。
//!
//! EN: Post-handshake admission control — the overflow connection completes the
//! handshake, then receives a standard FATAL 53300; freed slots are reusable.

use std::sync::Arc;

use houyi_server::pgwire_handler::{pgwire_accept_session, pgwire_connection_admit};
use houyi_server::types::{InflightWatermark, PgWireMetrics};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn startup_packet() -> Vec<u8> {
    let params = b"user\0houyi\0database\0test\0\0";
    let len = (8 + params.len()) as i32;
    let mut out = Vec::new();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&196608_i32.to_be_bytes()); // protocol 3.0
    out.extend_from_slice(params);
    out
}

async fn read_backend_message<S>(stream: &mut S) -> (u8, Vec<u8>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).await.expect("read header");
    let len = i32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len - 4];
    stream.read_exact(&mut payload).await.expect("read payload");
    (header[0], payload)
}

/// 消费 AuthenticationOk → ParameterStatus ×2 → ReadyForQuery。
/// EN: Consume AuthenticationOk -> ParameterStatus x2 -> ReadyForQuery.
async fn expect_auth_ok_sequence<S>(stream: &mut S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (tag, _) = read_backend_message(stream).await;
    assert_eq!(tag, b'R', "expect AuthenticationOk");
    let (t1, _) = read_backend_message(stream).await;
    let (t2, _) = read_backend_message(stream).await;
    let (t3, _) = read_backend_message(stream).await;
    assert_eq!([t1, t2, t3], [b'S', b'S', b'Z']);
}

/// 起 max=1 水位线的 pgwire 服务（生产入口 + 生产裁决）；返回 (地址, metrics)。
/// EN: Start pgwire with max=1 watermark (production entry + admission); returns (addr, metrics).
async fn spawn_watermark_server() -> (String, Arc<PgWireMetrics>) {
    let metrics = Arc::new(PgWireMetrics {
        connection_watermark: Arc::new(InflightWatermark::new(1)),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let metrics_srv = Arc::clone(&metrics);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let metrics_conn = Arc::clone(&metrics_srv);
            tokio::spawn(async move {
                if let Ok((mut session, _principal)) =
                    pgwire_accept_session(stream, &metrics_conn, None, None).await
                {
                    // 生产裁决：占位成功则持有许可直到客户端断开（读 EOF）。
                    // EN: Production admission: on success the permit is held until the client
                    // EN: disconnects (read EOF).
                    if let Some(_permit) =
                        pgwire_connection_admit(&mut session, &metrics_conn).await
                    {
                        let mut buf = [0u8; 1];
                        let _ = session.read(&mut buf).await;
                    }
                }
            });
        }
    });
    (addr, metrics)
}

#[tokio::test]
async fn pgwire_watermark_overflow_gets_fatal_53300_then_slot_frees() {
    let (addr, metrics) = spawn_watermark_server().await;

    // 连接 1：握手 + 占位成功，保持存活占住唯一名额。
    // EN: Connection 1: handshake + admission succeed; stay alive to hold the only slot.
    let mut c1 = TcpStream::connect(&addr).await.unwrap();
    c1.write_all(&startup_packet()).await.unwrap();
    expect_auth_ok_sequence(&mut c1).await;
    // 等服务端完成裁决占位。
    // EN: Wait for the server to complete admission.
    for _ in 0..100 {
        if metrics.connection_watermark.active() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(metrics.connection_watermark.active(), 1);

    // 连接 2：握手照常完成（AuthenticationOk），随后在裁决点收 FATAL 53300。
    // EN: Connection 2: handshake completes as usual (AuthenticationOk), then FATAL 53300
    // EN: at the admission point.
    let mut c2 = TcpStream::connect(&addr).await.unwrap();
    c2.write_all(&startup_packet()).await.unwrap();
    expect_auth_ok_sequence(&mut c2).await;
    let (tag, payload) = read_backend_message(&mut c2).await;
    assert_eq!(tag, b'E', "overflow must yield ErrorResponse");
    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains("53300"), "expect SQLSTATE 53300, got {text}");
    assert_eq!(metrics.connection_watermark.rejected_total(), 1);

    // 连接 1 断开 → 名额归还；连接 3 握手后不再收 FATAL（读会等查询响应，
    // 这里以 gauge 归零为名额归还证据）。
    // EN: Connection 1 disconnects -> slot returned; connection 3 gets no FATAL after
    // EN: handshake (reading would wait for a query response; gauge returning to zero is
    // EN: the evidence for slot return).
    drop(c1);
    for _ in 0..100 {
        if metrics.connection_watermark.active() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(metrics.connection_watermark.active(), 0);
    let mut c3 = TcpStream::connect(&addr).await.unwrap();
    c3.write_all(&startup_packet()).await.unwrap();
    expect_auth_ok_sequence(&mut c3).await;
    for _ in 0..100 {
        if metrics.connection_watermark.active() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        metrics.connection_watermark.active(),
        1,
        "third connection must be admitted after the slot freed"
    );
    assert_eq!(metrics.connection_watermark.rejected_total(), 1);
}
