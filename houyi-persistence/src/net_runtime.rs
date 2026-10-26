//! 网络换型层（P4.5.5 turmoil DST）。
//! Network swap layer for turmoil-based deterministic simulation.
//!
//! turmoil 的 `net` 是 tokio 的 **drop-in**（同一套 `AsyncRead`/`AsyncWrite`
//! 契约）：把复制层用到的 socket 类型集中在这里换一次，仿真网络即可接管。
//! 时钟**不换型**：turmoil 的 host 各自跑在真实的 tokio runtime 上，其 timer
//! driver 已把 `tokio::time` 接到模拟时钟——生产代码继续用 `tokio::time` 就
//! 自动被仿真。生产构建走 tokio，turmoil 不进依赖图（可选依赖，仅集成测试
//! 构建启用）。
//! EN: turmoil's `net` is a tokio drop-in, so the socket types are swapped here.
//! The clock is NOT swapped: each turmoil host runs on a real tokio runtime
//! whose timer driver is bound to simulated time, so `tokio::time` is already
//! simulated. Production builds use tokio; turmoil stays out of the graph.
//!
//! 边界 / Boundary: 只换「复制会话的 socket」。文件 IO（WAL、WalTailReader）
//! 与 wal worker 仍是真实世界——DST 覆盖的是网络面，不是存储面（存储面由
//! P4.5.4 的 kill -9 演练覆盖）。

#[cfg(all(not(loom), feature = "turmoil"))]
pub use turmoil::net::{TcpListener, TcpStream};

#[cfg(all(not(loom), not(feature = "turmoil")))]
pub use tokio::net::{TcpListener, TcpStream};

/// 解析 `host:port`（P4.6.3 DNS 超时）。
///
/// 为什么单独暴露解析而不是直接把 hostname 交给 `TcpStream::connect`（WHY）：
/// tokio 的 connect 在 poll 里做阻塞 `getaddrinfo`，外层的 `timeout` 只能截断
/// 随后的 TCP 连接，**截不住 DNS 本身**——防火墙吞掉 DNS 查询时 worker 线程会
/// 被钉在 getaddrinfo 里直到系统解析器超时（分钟级）。这里把解析拆成独立
/// future，让调用方的 timeout 真正覆盖它。turmoil 的 DNS 是仿真注册表（同步、
/// 确定性），同一接口保持两侧行为一致。
/// EN: tokio's connect performs blocking `getaddrinfo` inside poll, so an outer
/// timeout cannot bound DNS resolution itself. Splitting resolution into its own
/// future lets the caller's timeout cover it. turmoil resolves via its simulated
/// registry (synchronous, deterministic) behind the same interface.
#[cfg(all(not(loom), not(feature = "turmoil")))]
pub async fn resolve_host(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

/// 仿真网络版的解析（turmoil 的 `lookup_host`）。
/// Simulated-network resolution (turmoil's lookup_host).
#[cfg(all(not(loom), feature = "turmoil"))]
pub async fn resolve_host(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    Ok(turmoil::net::lookup_host((host, port)).await?.collect())
}

// 时钟统一走 tokio（turmoil 的 host runtime 已把 timer 接到模拟时钟）。
// loom 构建不编译网络/会话代码，此处一并隐藏。
// EN: Timing always goes through tokio (turmoil's host runtime binds its timer
// to the simulated clock); loom builds omit network/session code entirely.
#[cfg(not(loom))]
pub use tokio::time::{sleep, timeout};
