//! TLS 支持（P4.2.2）：rustls acceptor 加载 + axum TLS Listener。
//! TLS support: loads a shared rustls `TlsAcceptor` (PEM cert/key) and provides
//! the axum `Listener` adapter; pgwire 侧的 SSLRequest 升级见 `pgwire_handler`。
//!
//! 选型：rustls 纯 Rust 栈（与 ureq 同规，避免 OpenSSL 系统依赖）；只编 ring
//! provider，单证书、不做客户端认证（mTLS 超出 beta 范围）。
//! EN: rustls-only stack (no OpenSSL), ring provider, server-auth only.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// TLS 握手超时：axum 的 `Listener::accept` 是单 future 串行驱动——在 accept
/// 内做 TLS 握手若不设上限，一个只连不握手的慢/恶意客户端会堵住后续全部
/// HTTP accept（队头阻塞 DoS）。超时后拒绝该连接并继续 accept。
/// EN: Bound the in-accept TLS handshake — axum drives `accept` serially, so an
/// unbounded handshake lets one idle client head-of-line-block every later
/// connection. Timed-out handshakes are dropped and accept continues.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn pem_error(what: &str, path: &Path, e: rustls_pki_types::pem::Error) -> io::Error {
    // 保留文件打开错误的 kind（缺失证书应报 NotFound，而不是 InvalidData）。
    // EN: preserve IO error kinds (a missing file is NotFound, not bad data).
    let kind = match &e {
        rustls_pki_types::pem::Error::Io(io) => io.kind(),
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, format!("parse tls {what} {}: {e}", path.display()))
}

/// 从 PEM 文件加载证书链 + 私钥，构建 HTTP/pgwire 共用的 `TlsAcceptor`。
/// Load a PEM cert chain + private key into a shared `TlsAcceptor`.
///
/// 启动期调用，任何一步失败都 fast-fail（配置错误不应拖到首个连接才炸）。
/// EN: Called at startup; failures are fatal (config errors must not surface
/// only on first connection).
pub fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> io::Result<TlsAcceptor> {
    // PEM 解析用 rustls-pki-types（rustls-pemfile 已停止维护，
    // RUSTSEC-2025-0134；功能已迁入 pki-types 的 `pem` 模块）。
    // EN: parse PEM via rustls-pki-types; rustls-pemfile is unmaintained.
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| pem_error("cert", cert_path, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| pem_error("cert", cert_path, e))?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no certificates found in {}", cert_path.display()),
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| pem_error("key", key_path, e))?;
    // 显式钉 ring provider：当前 feature 集下 builder() 会自动选 ring，
    // 但若将来有人加编 aws-lc-rs，隐式选择会变成"谁先 install 谁赢"的
    // 运行时玄学——显式传参把密码学后端变成编译期确定的事实。
    // EN: Pin the ring provider explicitly. Today builder() auto-selects it,
    // but if aws-lc-rs ever gets compiled in too, implicit selection becomes
    // a first-installer-wins runtime lottery — passing the provider makes the
    // crypto backend a compile-time fact.
    let config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tls protocol versions: {e}"),
        )
    })?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tls cert/key mismatch ({} / {}): {e}",
                cert_path.display(),
                key_path.display()
            ),
        )
    })?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// axum 0.8 `Listener` 的 TLS 适配：accept TCP 后在 accept 内完成 TLS 握手
/// （带 [`TLS_HANDSHAKE_TIMEOUT`] 上限，防队头阻塞），失败的连接拒绝后继续。
/// EN: axum `Listener` adapter that upgrades accepted TCP connections to TLS
/// inside `accept` (bounded handshake; failed handshakes are dropped).
pub struct TlsHttpListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
    /// 暂时性 TCP accept 失败的连续计数：fd 耗尽等场景下 accept 会循环失败，
    /// 每 10ms 一条 warn 即日志洪泛——首次失败 warn（孤立瞬态是常态，成功
    /// accept 即清零），每连续 100 次升 error（持续性故障才是告警条件）。
    /// 限流不静默：故障仍然可见，但不会淹没其他日志。
    /// EN: Consecutive transient-accept-failure counter — under fd exhaustion
    /// accept fails in a hot loop and a warn per 10ms floods the log. First
    /// failure logs at warn (isolated transients are routine; the counter
    /// resets on the next success); every 100 consecutive failures escalate
    /// to error (persistence is the alert-worthy condition). Throttled, not
    /// silent.
    accept_failures: u64,
}

impl TlsHttpListener {
    pub fn new(inner: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self {
            inner,
            acceptor,
            accept_failures: 0,
        }
    }
}

impl axum::serve::Listener for TlsHttpListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(tup) => {
                    self.accept_failures = 0;
                    tup
                }
                Err(e) => {
                    // 暂时性 accept 错误记录后重试（对齐 tokio/axum 的容错取向，
                    // 但限流是我们自己的补充——上游并不替你管日志量）。
                    // EN: log-and-retry on transient accept errors (same
                    // fault-tolerant stance as tokio/axum; the throttling is
                    // ours — upstream does not bound your log volume).
                    // 严重度按持续性分级：孤立的暂时失败（bursty 负载下的
                    // ECONNABORTED 是常态）只配 warn——成功 accept 会清零计数，
                    // 每次都打 error 会对良性瞬态误报警；只有连续 100 次
                    // （≈持续故障，如 fd 耗尽）才升 error。
                    // EN: Severity follows persistence — an isolated transient
                    // (routine ECONNABORTED under bursty load) is warn-only
                    // since the counter resets on the next success; only 100
                    // consecutive failures (a real outage like fd exhaustion)
                    // escalate to error.
                    self.accept_failures += 1;
                    if self.accept_failures == 1 {
                        tracing::warn!(error = %e, "tls http listener: tcp accept failed, retrying");
                    } else if self.accept_failures.is_multiple_of(100) {
                        tracing::error!(
                            error = %e,
                            consecutive_failures = self.accept_failures,
                            "tls http listener: tcp accept still failing"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
                Ok(Ok(tls)) => return (tls, addr),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, peer = %addr, "tls http listener: handshake failed");
                }
                Err(_) => {
                    tracing::warn!(peer = %addr, "tls http listener: handshake timed out");
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pem(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn gen_cert(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_path = write_pem(dir, "cert.pem", &cert.cert.pem());
        let key_path = write_pem(dir, "key.pem", &cert.key_pair.serialize_pem());
        (cert_path, key_path)
    }

    #[test]
    fn load_acceptor_roundtrip_ok() {
        let dir = std::env::temp_dir().join(format!(
            "houyidb-tls-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = gen_cert(&dir);
        load_tls_acceptor(&cert, &key).expect("valid PEM pair must load");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_acceptor_missing_file_fails_fast() {
        let missing = Path::new("/nonexistent/houyidb-tls-cert.pem");
        // TlsAcceptor 不实现 Debug，unwrap_err 不可用；转 Option 断言。
        // EN: TlsAcceptor has no Debug, so unwrap_err is unavailable; assert via Option.
        let err = load_tls_acceptor(missing, missing).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("tls cert"));
    }

    #[test]
    fn load_acceptor_garbage_pem_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "houyidb-tls-test-garbage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = write_pem(&dir, "cert.pem", "not a pem at all");
        let key = write_pem(&dir, "key.pem", "also not a pem");
        let err = load_tls_acceptor(&cert, &key).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(&dir).ok();
    }
}
