//! P4.2.1 pgwire 认证集成测试：真实 loopback TCP + 生产握手函数。
//! - 启用 token：SSLRequest 照常回 'N' → AuthenticationCleartextPassword 质询 →
//!   错密码 FATAL 28P01 断连；对密码 AuthenticationOk + ParameterStatus + ReadyForQuery。
//! - 未启用 token（None）：维持旧行为，startup 后直接 AuthenticationOk。
//! - 边界钉（OCR 359d85d）：非 'p' 应答 28000、密码消息长度上下限、
//!   FATAL 后必须关连接（超时兜底防套件挂死）、auth_failures 与 decode_errors 分账。
//!
//! EN: P4.2.1 pgwire auth integration test: real loopback TCP + production handshake.
//! EN: - Token enabled: SSLRequest still answers 'N' -> AuthenticationCleartextPassword
//! EN:   challenge -> wrong password FATAL 28P01 disconnect; correct password gets
//! EN:   AuthenticationOk + ParameterStatus + ReadyForQuery.
//! EN: - Token disabled (None): legacy behavior, AuthenticationOk right after startup.
//! EN: - Boundary pins (OCR 359d85d): non-'p' reply 28000, password message length bounds,
//! EN:   connection must close after FATAL (timeout guard against suite hangs),
//! EN:   auth_failures vs decode_errors accounting.

use houyi_server::pgwire_handler::handle_pgwire_handshake;
use houyi_server::types::PgWireMetrics;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 测试 IO 超时（OCR 359d85d[4][5]）：任何一步读阻塞超过此值即失败——
/// 服务端回归成"FATAL 后不关连接"或"截断 body 死等"时，测试以自解释
/// 的 panic 失败，而不是把整个套件挂到死。
/// EN: Test IO timeout (OCR 359d85d[4][5]): any blocked read beyond this value fails
/// EN: the test — if the server regresses to "keep connection open after FATAL" or
/// EN: "truncated body waits forever", the test panics with a self-explanatory message
/// EN: instead of hanging the whole suite.
const TEST_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn startup_packet() -> Vec<u8> {
    let params = b"user\0houyi\0database\0test\0\0";
    let len = (8 + params.len()) as i32;
    let mut out = Vec::new();
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&196608_i32.to_be_bytes()); // protocol 3.0
    out.extend_from_slice(params);
    out
}

fn password_message(password: &str) -> Vec<u8> {
    let payload_len = password.len() + 1;
    let len = (payload_len + 4) as i32;
    let mut out = Vec::new();
    out.push(b'p');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(password.as_bytes());
    out.push(0);
    out
}

/// 构造任意 declared-len 的 'p' 消息（边界测试用；body 长度 = len - 4）。
/// EN: Build a 'p' message with an arbitrary declared length (for boundary tests;
/// EN: body length = len - 4).
fn raw_password_message(declared_len: i32, body: &[u8]) -> Vec<u8> {
    assert_eq!(
        declared_len as usize,
        body.len() + 4,
        "declared len must match body"
    );
    let mut out = vec![b'p'];
    out.extend_from_slice(&declared_len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// 读一条后端消息：`(type, payload)`。
/// 长度守卫 + 读超时（OCR 359d85d[5]）：declared len < 4 会让 `len - 4`
/// 下溢（debug panic / release 巨分配），截断 body 会让 read_exact 死等——
/// helper 自己挡住，失败信息指向服务端畸形行为。
/// EN: Read one backend message: `(type, payload)`.
/// EN: Length guard + read timeout (OCR 359d85d[5]): declared len < 4 makes `len - 4`
/// EN: underflow (debug panic / huge release allocation); a truncated body makes
/// EN: read_exact wait forever — the helper blocks both, and failures point at
/// EN: malformed server behavior.
async fn read_backend_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    tokio::time::timeout(TEST_IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .expect("read header timed out (server hung?)")
        .expect("read header");
    let len = i32::from_be_bytes(header[1..5].try_into().unwrap());
    assert!(
        len >= 4,
        "server sent malformed message: declared length {len} < 4"
    );
    let mut payload = vec![0u8; len as usize - 4];
    tokio::time::timeout(TEST_IO_TIMEOUT, stream.read_exact(&mut payload))
        .await
        .expect("read payload timed out (truncated body?)")
        .expect("read payload");
    (header[0], payload)
}

/// 4 字节 payload 转 i32，panic 信息带上下文（OCR 359d85d[5]）。
/// EN: Convert a 4-byte payload to i32 with contextual panic info (OCR 359d85d[5]).
fn payload_i32(payload: &[u8], ctx: &str) -> i32 {
    let bytes: [u8; 4] = payload
        .try_into()
        .unwrap_or_else(|_| panic!("{ctx}: expect 4-byte payload, got {} bytes", payload.len()));
    i32::from_be_bytes(bytes)
}

/// FATAL 后服务端必须关闭连接（带超时——OCR 359d85d[4]：裸 read 会把
/// "忘记关连接"的回归变成整个套件挂死）。
/// EN: After FATAL the server must close the connection (with a timeout — OCR
/// EN: 359d85d[4]: a bare read would turn a "forgot to close" regression into a suite hang).
async fn expect_eof(stream: &mut TcpStream) {
    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(TEST_IO_TIMEOUT, stream.read(&mut buf))
        .await
        .expect("connection must be closed after FATAL (timed out)");
    match result {
        Ok(0) => {}
        // 服务端在读 body 前拒绝（非 'p' 分支等）时，接收缓冲残留未读数据，
        // 关 socket 内核发 RST 而非 FIN——关闭契约同样成立，只是关闭方式不同。
        // EN: When the server rejects before reading the body (non-'p' branch etc.), the
        // EN: receive buffer keeps unread data, so closing the socket sends RST instead of
        // EN: FIN — the same close contract holds, only the close style differs.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("connection must be closed after FATAL, got {other:?}"),
    }
}

/// 起监听并跑生产握手；返回地址供客户端连接。
/// EN: Start listening and run the production handshake; returns the address for clients.
async fn spawn_handshake_server(auth_token: Option<&'static str>) -> String {
    let (addr, _) = spawn_handshake_server_with_metrics(auth_token).await;
    addr
}

/// 同上，但返回共享 metrics 句柄（认证失败计数断言用）。
/// EN: Same as above, but returns the shared metrics handle (for auth-failure counter assertions).
async fn spawn_handshake_server_with_metrics(
    auth_token: Option<&'static str>,
) -> (String, Arc<PgWireMetrics>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let metrics = Arc::new(PgWireMetrics::default());
    let server_metrics = metrics.clone();
    // 测试握手：legacy token 作为全权 admin 主体。
    // EN: Test handshake: the legacy token acts as an all-powerful admin principal.
    let policy = auth_token.map(|token| {
        Arc::new(
            houyi_server::rbac::AuthzPolicy::from_config(Some(token), &[]).expect("legacy policy"),
        )
    });
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let m = server_metrics.clone();
            let policy_conn = policy.clone();
            tokio::spawn(async move {
                let _ = handle_pgwire_handshake(&mut stream, &m, policy_conn.as_deref()).await;
            });
        }
    });
    (addr, metrics)
}

/// 连上并完成 startup，断言传回明文密码质询（各认证用例的公共前奏）。
/// EN: Connect and complete startup, asserting a cleartext password challenge (common prologue).
async fn connect_and_expect_challenge(addr: &str) -> TcpStream {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&startup_packet()).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'R', "expect auth challenge");
    assert_eq!(
        payload_i32(&payload, "auth challenge"),
        3,
        "expect cleartext password request"
    );
    c
}

#[tokio::test]
async fn pgwire_auth_wrong_password_rejected_right_accepted() {
    let addr = spawn_handshake_server(Some("s3cret")).await;

    // 错误密码：质询后收 FATAL 28P01，连接随后被服务端关闭。
    // EN: Wrong password: FATAL 28P01 after the challenge; the server then closes the connection.
    let mut c1 = connect_and_expect_challenge(&addr).await;
    c1.write_all(&password_message("wrong")).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c1).await;
    assert_eq!(tag, b'E', "wrong password must yield ErrorResponse");
    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains("28P01"), "expect SQLSTATE 28P01, got {text}");
    expect_eof(&mut c1).await;

    // 正确密码：AuthenticationOk('R',0) → ParameterStatus ×2 → ReadyForQuery。
    // EN: Correct password: AuthenticationOk('R',0) -> ParameterStatus x2 -> ReadyForQuery.
    let mut c2 = connect_and_expect_challenge(&addr).await;
    c2.write_all(&password_message("s3cret")).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c2).await;
    assert_eq!(tag, b'R', "expect AuthenticationOk");
    assert_eq!(payload_i32(&payload, "AuthenticationOk"), 0);
    let (t1, _) = read_backend_message(&mut c2).await;
    let (t2, _) = read_backend_message(&mut c2).await;
    let (t3, _) = read_backend_message(&mut c2).await;
    assert_eq!([t1, t2, t3], [b'S', b'S', b'Z']);
}

#[tokio::test]
async fn pgwire_no_token_keeps_legacy_open_handshake() {
    let addr = spawn_handshake_server(None).await;
    let mut c = TcpStream::connect(&addr).await.unwrap();
    c.write_all(&startup_packet()).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'R', "no token → immediate AuthenticationOk");
    assert_eq!(payload_i32(&payload, "AuthenticationOk"), 0);
}

#[tokio::test]
async fn pgwire_auth_ssl_request_still_negotiated_before_auth() {
    let addr = spawn_handshake_server(Some("s3cret")).await;
    let mut c = TcpStream::connect(&addr).await.unwrap();
    // SSLRequest: len=8, code=80877103 → 固定回 'N'（TLS 是 P4.2.2 的范围）。
    // EN: SSLRequest: len=8, code=80877103 -> always 'N' (TLS is P4.2.2 scope).
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&8_i32.to_be_bytes());
    ssl.extend_from_slice(&80877103_i32.to_be_bytes());
    c.write_all(&ssl).await.unwrap();
    let mut one = [0u8; 1];
    c.read_exact(&mut one).await.unwrap();
    assert_eq!(one[0], b'N');
    // SSL 拒绝后继续 startup，仍应收到认证质询。
    // EN: Continue startup after SSL refusal; the auth challenge must still arrive.
    c.write_all(&startup_packet()).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'R');
    assert_eq!(payload_i32(&payload, "auth challenge"), 3);
}

/// 非 'p' 应答（OCR 359d85d[13]）：质询后客户端回 Query/Terminate 等任何
/// 非 PasswordMessage，必须 FATAL 28000 "expected password message" + 关连接——
/// 钉住完整的认证拒绝契约（类型检查被删或 SQLSTATE 写错时此测试红）。
/// EN: Non-'p' reply (OCR 359d85d[13]): after the challenge, any non-PasswordMessage
/// EN: (Query/Terminate etc.) must yield FATAL 28000 "expected password message" and
/// EN: close the connection — pins the full auth rejection contract (red if the type
/// EN: check is removed or the SQLSTATE is wrong).
#[tokio::test]
async fn pgwire_auth_non_password_message_rejected_28000() {
    let addr = spawn_handshake_server(Some("s3cret")).await;
    let mut c = connect_and_expect_challenge(&addr).await;
    // 用 'Q'（Query）应答明文质询。
    // EN: Answer the cleartext challenge with 'Q' (Query).
    let query = b"SELECT 1\0";
    let len = (query.len() + 4) as i32;
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(query);
    c.write_all(&msg).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E', "non-'p' reply must yield ErrorResponse");
    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains("28000"), "expect SQLSTATE 28000, got {text}");
    assert!(
        text.contains("expected password message"),
        "expect rejection reason, got {text}"
    );
    expect_eof(&mut c).await;
}

/// 密码消息长度边界（OCR 359d85d[3]）：生产在分配前检查 declared len
/// ∈ [5, 4104]——区间外 FATAL 28000 且不读 body（无界分配攻击面）；
/// 区间内进入密码比较。边界值钉住既定上限（4104 = 4 头 + 4099 密码 + 1  NUL，
/// 比"4 KiB 密码"的名义上限宽 3 字节——这是被钉住的决定，不是巧合）。
/// EN: Password message length bounds (OCR 359d85d[3]): production checks declared len
/// EN: in [5, 4104] before allocating — outside the range FATAL 28000 without reading
/// EN: the body (unbounded allocation surface); inside it proceeds to comparison.
/// EN: Boundary values pin the established upper limit (4104 = 4 header + 4099 password
/// EN: + 1 NUL, 3 bytes wider than the nominal "4 KiB password" limit — a pinned
/// EN: decision, not a coincidence).
#[tokio::test]
async fn pgwire_auth_password_length_boundaries() {
    let addr = spawn_handshake_server(Some("s3cret")).await;

    // 区间外：len=4（无 body 空间）与 len=4105（超上限）→ 28000 invalid length。
    // 服务端在读 body 前拒绝，所以只发 5 字节头（body 永远不到也无所谓）。
    // EN: Outside the range: len=4 (no body space) and len=4105 (above limit) -> 28000
    // EN: invalid length. The server rejects before reading the body, so send only the
    // EN: 5-byte header (the body never arriving is fine).
    for declared in [4_i32, 4105] {
        let mut c = connect_and_expect_challenge(&addr).await;
        let mut msg = vec![b'p'];
        msg.extend_from_slice(&declared.to_be_bytes());
        c.write_all(&msg).await.unwrap();
        let (tag, payload) = read_backend_message(&mut c).await;
        assert_eq!(
            tag, b'E',
            "declared len {declared} must yield ErrorResponse"
        );
        let text = String::from_utf8_lossy(&payload);
        assert!(
            text.contains("28000") && text.contains("invalid password message length"),
            "declared len {declared}: expect 28000 invalid length, got {text}"
        );
        expect_eof(&mut c).await;
    }

    // 区间内下限 len=5：空密码（仅 NUL）→ 进入比较 → 28P01。
    // EN: Lower bound len=5: empty password (NUL only) -> enters comparison -> 28P01.
    let mut c = connect_and_expect_challenge(&addr).await;
    c.write_all(&raw_password_message(5, &[0])).await.unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E');
    let text = String::from_utf8_lossy(&payload);
    assert!(
        text.contains("28P01"),
        "len=5 must reach password compare (28P01), got {text}"
    );
    expect_eof(&mut c).await;

    // 区间内上限 len=4104：4099 字节密码 + NUL → 进入比较 → 28P01。
    // EN: Upper bound len=4104: 4099-byte password + NUL -> enters comparison -> 28P01.
    let mut c = connect_and_expect_challenge(&addr).await;
    let mut body = vec![b'a'; 4099];
    body.push(0);
    c.write_all(&raw_password_message(4104, &body))
        .await
        .unwrap();
    let (tag, payload) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E');
    let text = String::from_utf8_lossy(&payload);
    assert!(
        text.contains("28P01"),
        "len=4104 must reach password compare (28P01), got {text}"
    );
    expect_eof(&mut c).await;
}

/// 认证失败分账（OCR 359d85d[12]）：三条拒绝路径（非 'p' / 长度畸形 /
/// 密码错误）全部计 auth_failures，decode_errors 不被污染——dashboard
///  keyed 在 decode_errors 上时，爆破尝试不能被误读成客户端协议 bug。
/// EN: Auth failure accounting (OCR 359d85d[12]): all three rejection paths (non-'p' /
/// EN: malformed length / wrong password) count as auth_failures and do not pollute
/// EN: decode_errors — dashboards keyed on decode_errors must not read brute-force
/// EN: attempts as client protocol bugs.
#[tokio::test]
async fn pgwire_auth_failures_counted_separately_from_decode_errors() {
    let (addr, metrics) = spawn_handshake_server_with_metrics(Some("s3cret")).await;

    // 路径 1：非 'p'。
    // EN: Path 1: non-'p'.
    let mut c = connect_and_expect_challenge(&addr).await;
    let query = b"\0";
    let len = (query.len() + 4) as i32;
    let mut msg = vec![b'X'];
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(query);
    c.write_all(&msg).await.unwrap();
    let (tag, _) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E');
    expect_eof(&mut c).await;

    // 路径 2：长度畸形。
    // EN: Path 2: malformed length.
    let mut c = connect_and_expect_challenge(&addr).await;
    let mut msg = vec![b'p'];
    msg.extend_from_slice(&4_i32.to_be_bytes());
    c.write_all(&msg).await.unwrap();
    let (tag, _) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E');
    expect_eof(&mut c).await;

    // 路径 3：密码错误。
    // EN: Path 3: wrong password.
    let mut c = connect_and_expect_challenge(&addr).await;
    c.write_all(&password_message("wrong")).await.unwrap();
    let (tag, _) = read_backend_message(&mut c).await;
    assert_eq!(tag, b'E');
    expect_eof(&mut c).await;

    assert_eq!(
        metrics
            .auth_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        3,
        "all three auth rejection paths must count auth_failures"
    );
    assert_eq!(
        metrics
            .decode_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "auth rejections must not pollute decode_errors"
    );
}
