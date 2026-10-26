//! P4.2.2 TLS 集成测试：自签证书 + 真实 loopback TCP/TLS。
//! - pgwire：SSLRequest → 'S' → TLS 握手 → startup → AuthenticationOk；
//!   明文 startup 直通（sslmode=prefer）；GSSENC 显式回 'N' 后可继续 SSL 协商。
//! - HTTP：TlsHttpListener + 手写 HTTP/1.1 over TLS，验证 200 响应。
//!
//! 客户端用 NoVerify 校验器（自签证书），仅测试通道加密，不测试 PKI。
//!
//! EN: TLS integration tests over loopback with a self-signed cert and a
//! NoVerify client verifier — exercises the encrypted channel, not PKI.

use std::sync::Arc;
use std::time::Duration;

use houyi_server::pgwire_handler::pgwire_accept_session;
use houyi_server::tls::{load_tls_acceptor, TlsHttpListener};
use houyi_server::types::PgWireMetrics;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 测试专用证书校验器：全量放行（自签证书无 CA 链可验）。
/// EN: Test-only certificate verifier: accept everything (self-signed has no CA chain).
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn no_verify_connector() -> tokio_rustls::TlsConnector {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// 生成自签证书到临时目录，返回 (acceptor, 清理句柄)。
/// 目录名带进程内原子序号：Apple Silicon 时钟粒度粗于 1ns，并行测试同时钟周期
/// 撞名会互相覆盖 cert/key（KeyMismatch），与 d605c33 同根因。
/// EN: Generate a self-signed cert into a temp dir; returns (acceptor, cleanup handle).
/// EN: The dir name carries an in-process atomic sequence: Apple Silicon clock granularity
/// EN: is coarser than 1ns, so parallel tests in the same tick overwrite cert/key
/// EN: (KeyMismatch), same root cause as d605c33.
fn gen_acceptor() -> (tokio_rustls::TlsAcceptor, std::path::PathBuf) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "houyidb-pgwire-tls-it-{}-{}-{seq}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    let acceptor = load_tls_acceptor(&cert_path, &key_path).expect("load test acceptor");
    (acceptor, dir)
}

fn startup_packet() -> Vec<u8> {
    let params = b"user\0houyi\0database\0test\0\0";
    let len = (8 + params.len()) as i32;
    let mut out = Vec::new();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&196608_i32.to_be_bytes()); // protocol 3.0
    out.extend_from_slice(params);
    out
}

fn ssl_request_packet() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&8_i32.to_be_bytes());
    out.extend_from_slice(&80877103_i32.to_be_bytes());
    out
}

fn gssenc_request_packet() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&8_i32.to_be_bytes());
    out.extend_from_slice(&80877104_i32.to_be_bytes());
    out
}

/// 读一条后端消息：`(type, payload)`，泛型流（明文/TLS 复用）。
/// EN: Read one backend message: `(type, payload)`, generic over the stream (plain/TLS).
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

/// 断言握手成功尾部：AuthenticationOk('R',0) → ParameterStatus ×2 → ReadyForQuery。
/// EN: Assert the handshake success tail: AuthenticationOk('R',0) -> ParameterStatus x2
/// EN: -> ReadyForQuery.
async fn expect_auth_ok_sequence<S>(stream: &mut S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (tag, payload) = read_backend_message(stream).await;
    assert_eq!(tag, b'R', "expect AuthenticationOk");
    assert_eq!(i32::from_be_bytes(payload.try_into().unwrap()), 0);
    let (t1, _) = read_backend_message(stream).await;
    let (t2, _) = read_backend_message(stream).await;
    let (t3, _) = read_backend_message(stream).await;
    assert_eq!([t1, t2, t3], [b'S', b'S', b'Z']);
}

/// 起生产入口（TLS acceptor 启用）的 pgwire 监听；返回地址。
/// EN: Start a pgwire listener via the production entry (TLS acceptor enabled); returns the
/// EN: address.
async fn spawn_tls_pgwire_server(acceptor: tokio_rustls::TlsAcceptor) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let metrics = PgWireMetrics::default();
                let _ = pgwire_accept_session(stream, &metrics, None, Some(&acceptor)).await;
            });
        }
    });
    addr
}

/// SSLRequest → 'S' → TLS 握手 → startup → AuthenticationOk 全链路。
/// EN: SSLRequest -> 'S' -> TLS handshake -> startup -> AuthenticationOk full chain.
#[tokio::test]
async fn pgwire_tls_ssl_request_upgrades_and_handshakes() {
    let (acceptor, dir) = gen_acceptor();
    let addr = spawn_tls_pgwire_server(acceptor).await;

    let mut plain = TcpStream::connect(&addr).await.unwrap();
    plain.write_all(&ssl_request_packet()).await.unwrap();
    let mut one = [0u8; 1];
    plain.read_exact(&mut one).await.unwrap();
    assert_eq!(one[0], b'S', "server must agree to TLS");

    let connector = no_verify_connector();
    let server_name = ServerName::try_from("localhost").unwrap().to_owned();
    let mut tls = tokio::time::timeout(TEST_TIMEOUT, connector.connect(server_name, plain))
        .await
        .expect("tls handshake timeout")
        .expect("tls handshake");

    tls.write_all(&startup_packet()).await.unwrap();
    tokio::time::timeout(TEST_TIMEOUT, expect_auth_ok_sequence(&mut tls))
        .await
        .expect("auth sequence timeout");

    std::fs::remove_dir_all(&dir).ok();
}

/// prefer 语义：TLS 已启用时，客户端直接发明文 startup 也应明文直通。
/// EN: prefer semantics: when TLS is enabled, a client sending plaintext startup directly
/// EN: still passes through as plaintext.
#[tokio::test]
async fn pgwire_tls_plaintext_startup_passes_through() {
    let (acceptor, dir) = gen_acceptor();
    let addr = spawn_tls_pgwire_server(acceptor).await;

    let mut c = TcpStream::connect(&addr).await.unwrap();
    c.write_all(&startup_packet()).await.unwrap();
    tokio::time::timeout(TEST_TIMEOUT, expect_auth_ok_sequence(&mut c))
        .await
        .expect("auth sequence timeout");

    std::fs::remove_dir_all(&dir).ok();
}

/// GSSENC 显式回 'N' 后，同一连接上仍可继续 SSLRequest 协商（psql 探测顺序）。
/// EN: After GSSENC explicitly answers 'N', SSLRequest negotiation can still proceed on
/// EN: the same connection (psql probe order).
#[tokio::test]
async fn pgwire_tls_gssenc_rejected_then_ssl_accepted() {
    let (acceptor, dir) = gen_acceptor();
    let addr = spawn_tls_pgwire_server(acceptor).await;

    let mut plain = TcpStream::connect(&addr).await.unwrap();
    plain.write_all(&gssenc_request_packet()).await.unwrap();
    let mut one = [0u8; 1];
    plain.read_exact(&mut one).await.unwrap();
    assert_eq!(one[0], b'N', "GSSENC must be refused");

    plain.write_all(&ssl_request_packet()).await.unwrap();
    plain.read_exact(&mut one).await.unwrap();
    assert_eq!(
        one[0], b'S',
        "SSLRequest after GSSENC must still be accepted"
    );

    let connector = no_verify_connector();
    let server_name = ServerName::try_from("localhost").unwrap().to_owned();
    let mut tls = tokio::time::timeout(TEST_TIMEOUT, connector.connect(server_name, plain))
        .await
        .expect("tls handshake timeout")
        .expect("tls handshake");
    tls.write_all(&startup_packet()).await.unwrap();
    tokio::time::timeout(TEST_TIMEOUT, expect_auth_ok_sequence(&mut tls))
        .await
        .expect("auth sequence timeout");

    std::fs::remove_dir_all(&dir).ok();
}

/// HTTP 侧：TlsHttpListener 上跑最小 axum 应用，手写 HTTP/1.1 over TLS 验证 200。
/// EN: HTTP side: run a minimal axum app on TlsHttpListener and hand-write HTTP/1.1 over
/// EN: TLS, expecting 200.
#[tokio::test]
async fn http_tls_listener_serves_requests() {
    use axum::{routing::get, Router};

    let (acceptor, dir) = gen_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/health", get(|| async { "ok" }));
    tokio::spawn(async move {
        axum::serve(TlsHttpListener::new(listener, acceptor), app)
            .await
            .unwrap();
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    let connector = no_verify_connector();
    let server_name = ServerName::try_from("localhost").unwrap().to_owned();
    let mut tls = tokio::time::timeout(TEST_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .expect("tls handshake timeout")
        .expect("tls handshake");

    tls.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tokio::time::timeout(TEST_TIMEOUT, tls.read_to_end(&mut resp))
        .await
        .expect("read timeout")
        .unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "expect 200 over TLS, got: {text}"
    );
    assert!(text.ends_with("ok"), "expect body, got: {text}");

    std::fs::remove_dir_all(&dir).ok();
}
