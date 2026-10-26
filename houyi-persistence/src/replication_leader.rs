//! 复制 leader：ShipHub 扇出 + follower session（writer / ACK reader）。
//! Replication leader: ship-hub fan-out plus per-follower sessions.
//!
//! ShipHub 设计 / Design:
//! - per-follower 有界队列（tokio mpsc）：慢 follower 队列满即踢出，不拖累
//!   其他 follower；publish（wal worker 热路径）永远 `try_send` 不阻塞。
//! - `sessions` RwLock 读侧仅 wal worker 单发布者：publish 不与其他 publish
//!   并发，读锁只是与 session 注册/注销互斥，持锁时长 = 数次 try_send，
//!   不构成「全局 Mutex」铁律的争用点（注释备查）。
//! - 一次 memcpy：FileWal 必须 own payload 进 WalEntry，publish 无法共享
//!   entry 内 buffer，故 hub 内 `Arc::new(payload.to_vec())`；fan-out 到
//!   N 个 follower 共享同一 Arc，不再逐连接拷贝。
//!
//! P4.5.5 tokio 化 / tokio migration: 会话 IO 从 std 线程 + 裸 `TcpStream`
//! 迁到 tokio task。收益有二：(1) 关停可 `abort` 取消，wedged writer（对端
//! 不读导致 `write_all` 永久阻塞）不再泄漏线程；(2) 网络类型经 `net` 换型层
//! 可切到 turmoil 的仿真网络，做确定性 DST。wal worker 仍是 std 线程——它只
//! 经 `try_send` 与 hub 交互，不进入 async 世界。
//! EN: Session IO moved from std threads + bare `TcpStream` to tokio tasks, so
//! shutdown can abort a wedged writer (peer not reading) instead of leaking the
//! thread, and the network types can be swapped for turmoil's simulated net.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::wal_replication as proto;
use crate::wal_tail::WalTailReader;

#[cfg(not(loom))]
use crate::net_runtime as runtime;
#[cfg(not(loom))]
use crate::net_runtime::{TcpListener, TcpStream};

// ── 并发原语换型层（P4.5.2 shuttle / loom）───────────────────────
// WHY 单独一层 / Why a swap layer: shuttle 的确定性调度**要求**被测代码的同步
// 原语全部来自 `shuttle::sync`——std 原语对调度器不可见，会让 shuttle 线程在真实
// 内核上阻塞，整个 execution 卡死且拿不到 deadlock 诊断。这里把本文件用到的原语
// 集中换型一次，使 `ShipHub` / `AckWatermarks` 的**同一份生产代码**分别跑在
// std（默认）/ shuttle（`--features shuttle`）/ loom（`--cfg loom`）三种原语之上。
// EN: Shuttle requires the code under test to use `shuttle::sync` primitives; a
// std primitive blocks a real OS thread and hangs the whole execution with no
// deadlock diagnostic. This layer swaps them once so the *same* production code
// runs on std (default) / shuttle / loom.
//
// 边界 / Boundary: 只有本文件换型。`FileWal` / `wal_async` / `replication_follower`
// 保持 std（不参与模型）；`ReplicationLeader` 的 accept/session 线程走真实 socket
// IO，其线程创建与 socket 同样保持 std。故启用 `shuttle` feature 时本文件只有
// **通道/锁类型**随之改变（如 `ShipHub::register` 的 Receiver）；`Arc` 不变（见
// 下方取舍 1）。该 feature 只服务过滤测试 `--lib shuttle_`（见 Cargo.toml）。
//
// loom 臂：loom 无 `sync_channel` / `try_send` / `recv_timeout`，队列原语不换型。
// loom 靶区是 acked_flag 的**内存序**（shuttle 一律按 SeqCst 建模，查不出弱内存序
// 缺陷）；队列语义由 shuttle 覆盖。故 loom 下 `wait()` 不可用（std mpsc 在 loom
// 模型内会真阻塞），loom 测试只驱动 `on_ack` / `covers`。
// 两处刻意的设计取舍 / Two deliberate choices:
//
// 1) `Arc` **不换型**（始终 `std::sync::Arc`）。Arc 是引用计数而非阻塞原语——
//    shuttle/loom 需要拦截的是「会让线程阻塞」的原语（Mutex / RwLock / 通道 /
//    线程创建）；把 Arc 一起换型只会让本文件里**不参与模型**的 `ReplicationLeader`
//    （真实 socket + std 线程）连带换型，并在 loom 臂撞上 `Arc<dyn Fn…>` 的
//    Sized 约束。保持 std 让换型边界严格收敛到「调度相关原语」。
// EN: Arc stays std — it is a refcount, not a blocking primitive. Swapping it would
// drag the socket-based ReplicationLeader into the swap for no modelling benefit.
//
// 2) 有界通道与等待通道分开别名。loom 的 mpsc **没有** `sync_channel` /
//    `recv_timeout`：`SyncReceiver`（ShipHub 的有界队列）在 loom 臂退回 std，
//    而 `Receiver`（waiter 唤醒通道）用 loom 的，才能被 loom 建模。取件动作做成
//    `sc::recv_timeout` 自由函数同理——loom 臂退回 `recv`，因为 loom 靶区本就是
//    「无超时的唤醒协议」，超时语义由 std 单测与 shuttle 覆盖。

// ── 编译期护栏：换型层只允许进入**测试**构建 ───────────────────────
// 两条现实泄漏路径都必须 fail closed：
// 1) `--all-features` / workspace 级 feature 统一化 → shuttle 原语进了生产路径，
//    真实 OS 线程上的第一次访问就 panic「Shuttle primitive outside of a Shuttle
//    test」，WAL 发布与复制会话直接崩溃；
// 2) `--cfg loom` 经环境 RUSTFLAGS 泄漏进非测试构建 → `sc::recv_timeout` 退化为
//    无超时的 `recv`，durable 等待的降级语义被静默剥掉，可能永久挂起。
// 两者都是「编译能过、运行期才炸且难归因」的类型，故用 compile_error 在编译期拦住。
// EN: Compile-time guards. `--all-features` (feature unification) would put shuttle
// primitives on the production path, and a leaked `--cfg loom` would silently drop
// the timeout from `recv_timeout`. Both fail at runtime in hard-to-diagnose ways,
// so they fail closed here instead.
#[cfg(all(feature = "shuttle", not(test)))]
compile_error!(
    "houyi-persistence 的 `shuttle` feature 只允许用于测试构建：\
     请用 `cargo test -p houyi-persistence --features shuttle --lib shuttle_`\
     （或 scripts/concurrency_check.sh）；禁止 --all-features / workspace 级传递。"
);
#[cfg(all(feature = "shuttle", loom))]
compile_error!(
    "shuttle 与 loom 两套换型互斥：一次只启用一个（loom 穷举内存序，shuttle 随机调度）。"
);
#[cfg(all(loom, not(test)))]
compile_error!(
    "houyi-persistence 的 `loom` feature 只允许用于测试构建：\
     请用 `cargo test -p houyi-persistence --features loom --lib loom_`\
     （或 scripts/concurrency_check.sh），不得泄漏进生产构建。"
);

#[cfg(all(not(loom), not(feature = "shuttle")))]
mod sc {
    pub use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub use std::sync::mpsc::{
        channel, Receiver as WakerReceiver, RecvTimeoutError, Sender as WakerSender,
    };
    pub use std::sync::{Mutex, RwLock};

    // ── waiter 唤醒通道：保持 std ──────────────────────────────────
    // `AckWatermarks::wait` 的调用方在 `spawn_blocking` 里阻塞等待（P4.6 再
    // 议信号量预算），tokio 通道没有阻塞版 `recv_timeout`，故这一条链不换型。
    // EN: Waiter wake channel stays on std: `AckWatermarks::wait` callers block
    // inside `spawn_blocking` (semaphore budget deferred to P4.6), and tokio
    // channels have no blocking `recv_timeout`, so this chain keeps its type.
    pub fn recv_timeout<T>(
        rx: &WakerReceiver<T>,
        timeout: std::time::Duration,
    ) -> Result<T, RecvTimeoutError> {
        rx.recv_timeout(timeout)
    }

    // ── ShipHub 有界队列：tokio 通道（P4.5.5）──────────────────────
    // 为什么可以用 tokio mpsc 而不必自写 hybrid 通道（WHY）：`Sender::try_send`
    // 是同步、无运行时依赖的，std 侧 wal worker 照旧非阻塞发布；会话侧
    // `recv().await` 直接可被 tokio/turmoil 调度。容量满 = 慢 follower，
    // 语义与旧 `sync_channel` 一致（踢出）。省掉一套自写 waker 协议 = 省掉
    // 一类丢唤醒缺陷面。EN: tokio's `try_send` is sync and runtime-free, so
    // the std wal worker keeps publishing without blocking and the session can
    // `.await`; capacity-full keeps the old kick semantics. No bespoke waker
    // protocol, no new missed-wakeup surface.
    pub use tokio::sync::mpsc::{
        channel as hub_channel, Receiver as HubReceiver, Sender as HubSender,
    };

    /// 有界通道发送结果（统一 std/tokio 两套错误类型）。
    /// EN: Bounded-channel send result (unifies the std/tokio error types).
    #[derive(Debug, PartialEq, Eq)]
    pub enum SendOutcome {
        Delivered,
        Full,
        Closed,
    }

    pub fn hub_try_send<T>(tx: &HubSender<T>, item: T) -> SendOutcome {
        use tokio::sync::mpsc::error::TrySendError;
        match tx.try_send(item) {
            Ok(()) => SendOutcome::Delivered,
            Err(TrySendError::Full(_)) => SendOutcome::Full,
            Err(TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    /// 有界通道接收结果。
    /// EN: Bounded-channel receive result.
    #[derive(Debug)]
    pub enum RecvOutcome<T> {
        Item(T),
        Empty,
        Closed,
    }

    pub fn hub_try_recv<T>(rx: &mut HubReceiver<T>) -> RecvOutcome<T> {
        use tokio::sync::mpsc::error::TryRecvError;
        match rx.try_recv() {
            Ok(v) => RecvOutcome::Item(v),
            Err(TryRecvError::Empty) => RecvOutcome::Empty,
            Err(TryRecvError::Disconnected) => RecvOutcome::Closed,
        }
    }

    /// 会话侧等待下一批；通道关闭返回 `None`。
    /// EN: Session side waits for the next batch; returns `None` on channel closure.
    pub async fn hub_recv_async<T>(rx: &mut HubReceiver<T>) -> Option<T> {
        rx.recv().await
    }

    /// 测试用阻塞取件（同步 `#[test]` 里取 tokio 通道）。**不得**在 async
    /// 上下文中调用——tokio 的 `blocking_recv` 会 panic。
    /// EN: Blocking receive for tests (takes a tokio channel in a sync `#[test]`).
    /// **Must not** be called inside an async context — tokio's `blocking_recv` panics.
    #[cfg(test)]
    pub fn hub_recv_blocking<T>(rx: &mut HubReceiver<T>) -> Option<T> {
        rx.blocking_recv()
    }

    /// 测试用带超时取件：`blocking_recv` 没有超时，用 1ms 轮询实现。
    /// 仅测试路径使用，性能不敏感。
    /// EN: Timed receive for tests: `blocking_recv` has no timeout, so this polls
    /// every 1ms. Test-only and performance-insensitive.
    #[cfg(test)]
    pub fn hub_recv_timeout<T>(rx: &mut HubReceiver<T>, dur: std::time::Duration) -> Option<T> {
        let deadline = std::time::Instant::now() + dur;
        loop {
            match rx.try_recv() {
                Ok(v) => return Some(v),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(all(not(loom), feature = "shuttle"))]
mod sc {
    pub use shuttle::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub use shuttle::sync::mpsc::{
        channel, Receiver as WakerReceiver, RecvTimeoutError, Sender as WakerSender,
    };
    pub use shuttle::sync::{Mutex, RwLock};

    /// shuttle 的 `recv_timeout` 忽略超时（上游 TODO）：行为等价于 `recv`，
    /// 由 shuttle 的 deadlock 检测兜住「真丢唤醒」（永久阻塞即报错）。
    /// EN: shuttle's `recv_timeout` ignores the timeout (upstream TODO): it behaves
    /// like `recv`, and shuttle's deadlock detector catches real missed wakeups
    /// (permanent blocking errors out).
    pub fn recv_timeout<T>(
        rx: &WakerReceiver<T>,
        timeout: std::time::Duration,
    ) -> Result<T, RecvTimeoutError> {
        rx.recv_timeout(timeout)
    }

    // ShipHub 在 shuttle 臂用 shuttle 自己的有界通道（**不能**退回 std）：
    // std 的 `recv` 是内核阻塞，shuttle 调度器看不见，会把整个 execution 钉死
    // 在单个 OS 线程上（P4.5.5 实测：退回 std 后 fanout 用例直接挂住）。
    // 本臂只服务同步测试（`hub_recv_async` 永不调用）。
    // EN: shuttle's own bounded channel — std's blocking `recv` is invisible to
    // the scheduler and hangs the execution (observed when this arm used std).
    pub use shuttle::sync::mpsc::{
        sync_channel as hub_channel, Receiver as HubReceiver, SyncSender as HubSender,
    };

    #[derive(Debug, PartialEq, Eq)]
    pub enum SendOutcome {
        Delivered,
        Full,
        Closed,
    }

    pub fn hub_try_send<T>(tx: &HubSender<T>, item: T) -> SendOutcome {
        use std::sync::mpsc::TrySendError;
        match tx.try_send(item) {
            Ok(()) => SendOutcome::Delivered,
            Err(TrySendError::Full(_)) => SendOutcome::Full,
            Err(TrySendError::Disconnected(_)) => SendOutcome::Closed,
        }
    }

    #[derive(Debug)]
    pub enum RecvOutcome<T> {
        Item(T),
        Empty,
        Closed,
    }

    pub fn hub_try_recv<T>(rx: &mut HubReceiver<T>) -> RecvOutcome<T> {
        use std::sync::mpsc::TryRecvError;
        match rx.try_recv() {
            Ok(v) => RecvOutcome::Item(v),
            Err(TryRecvError::Empty) => RecvOutcome::Empty,
            Err(TryRecvError::Disconnected) => RecvOutcome::Closed,
        }
    }

    /// shuttle 臂的会话代码只编译不运行（测试直接驱动 ShipHub 的同步 API）。
    /// EN: The shuttle-arm session code compiles but never runs (tests drive
    /// ShipHub's sync API directly).
    pub async fn hub_recv_async<T>(_rx: &mut HubReceiver<T>) -> Option<T> {
        unreachable!("async replication session must not run under the shuttle feature")
    }

    #[cfg(test)]
    pub fn hub_recv_blocking<T>(rx: &mut HubReceiver<T>) -> Option<T> {
        rx.recv().ok()
    }

    #[cfg(test)]
    pub fn hub_recv_timeout<T>(rx: &mut HubReceiver<T>, dur: std::time::Duration) -> Option<T> {
        rx.recv_timeout(dur).ok()
    }
}

#[cfg(loom)]
mod sc {
    pub use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    // loom 的 mpsc 没有 `sync_channel`/`recv_timeout`：waiter 唤醒用 loom 通道
    // （需要被 loom 建模），ShipHub 队列退回 std（ShipHub 不参与 loom 模型）。
    // EN: loom's mpsc lacks `sync_channel`/`recv_timeout`: waiter wakeups use loom
    // channels (they need modeling), while the ShipHub queue falls back to std
    // (ShipHub is outside the loom model).
    pub use loom::sync::mpsc::{channel, Receiver as WakerReceiver, Sender as WakerSender};
    pub use loom::sync::{Mutex, RwLock};
    pub use std::sync::mpsc::RecvTimeoutError;
    pub use std::sync::mpsc::{
        sync_channel as hub_channel, Receiver as HubReceiver, SyncSender as HubSender,
    };

    pub fn recv_timeout<T>(
        rx: &WakerReceiver<T>,
        _timeout: std::time::Duration,
    ) -> Result<T, RecvTimeoutError> {
        rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum SendOutcome {
        Delivered,
        Full,
        Closed,
    }

    pub fn hub_try_send<T>(tx: &HubSender<T>, item: T) -> SendOutcome {
        use std::sync::mpsc::TrySendError;
        match tx.try_send(item) {
            Ok(()) => SendOutcome::Delivered,
            Err(TrySendError::Full(_)) => SendOutcome::Full,
            Err(TrySendError::Disconnected(_)) => SendOutcome::Closed,
        }
    }

    #[derive(Debug)]
    pub enum RecvOutcome<T> {
        Item(T),
        Empty,
        Closed,
    }

    pub fn hub_try_recv<T>(rx: &mut HubReceiver<T>) -> RecvOutcome<T> {
        use std::sync::mpsc::TryRecvError;
        match rx.try_recv() {
            Ok(v) => RecvOutcome::Item(v),
            Err(TryRecvError::Empty) => RecvOutcome::Empty,
            Err(TryRecvError::Disconnected) => RecvOutcome::Closed,
        }
    }

    pub async fn hub_recv_async<T>(_rx: &mut HubReceiver<T>) -> Option<T> {
        unreachable!("async replication session must not run under loom")
    }

    #[cfg(test)]
    pub fn hub_recv_blocking<T>(rx: &mut HubReceiver<T>) -> Option<T> {
        rx.recv().ok()
    }

    #[cfg(test)]
    pub fn hub_recv_timeout<T>(rx: &mut HubReceiver<T>, dur: std::time::Duration) -> Option<T> {
        rx.recv_timeout(dur).ok()
    }
}

#[cfg(test)]
use sc::hub_recv_blocking;
use sc::{
    hub_try_recv, hub_try_send, recv_timeout, AtomicBool, AtomicU64, HubReceiver, HubSender, Mutex,
    Ordering, RecvOutcome, RecvTimeoutError, RwLock, SendOutcome, WakerSender,
};

/// 测试辅助：从 hub 接收端带超时取一批（同步测试里用；生产路径走
/// `hub_recv_async`）。
/// EN: Test helper: receive one batch from a hub receiver with a timeout (for sync
/// tests; production uses `hub_recv_async`).
#[cfg(test)]
pub(crate) fn hub_recv_timeout(rx: &mut ShipReceiver, dur: Duration) -> Option<Arc<ShipBatch>> {
    sc::hub_recv_timeout(rx, dur)
}

/// 一批待推送记录：LSN + 共享 payload（fan-out 零拷贝单元）。
/// EN: One batch to ship: LSN + shared payload (the zero-copy fan-out unit).
pub struct ShipBatch {
    pub lsn: u64,
    pub payload: Arc<Vec<u8>>,
}

struct SessionSlot {
    id: u64,
    tx: HubSender<Arc<ShipBatch>>,
}

/// `ShipHub::register` 返回的接收端（有界队列侧；shuttle/loom 臂退回 std）。
/// EN: Receiver returned by `ShipHub::register` (bounded-queue side; shuttle/loom
/// arms fall back to std).
pub type ShipReceiver = HubReceiver<Arc<ShipBatch>>;

/// leader 侧扇出枢纽：wal worker flush 成功后 publish，各 follower session
/// 从自有队列取批推送。
/// EN: Leader-side fan-out hub: the wal worker publishes after a successful flush
/// and each follower session drains its own queue.
pub struct ShipHub {
    sessions: RwLock<Vec<SessionSlot>>,
    next_id: AtomicU64,
    queue_batches: usize,
    // ── 指标（Relaxed，role!=disabled 才输出）──
    // EN: Metrics (Relaxed; exported only when role != disabled).
    slow_follower_kicks_total: AtomicU64,
    shipped_batches_total: AtomicU64,
    shipped_bytes_total: AtomicU64,
}

impl ShipHub {
    pub fn new(queue_batches: usize) -> Self {
        Self {
            sessions: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
            queue_batches,
            slow_follower_kicks_total: AtomicU64::new(0),
            shipped_batches_total: AtomicU64::new(0),
            shipped_bytes_total: AtomicU64::new(0),
        }
    }

    /// 注册一个 follower session，返回 (session_id, 接收端)。
    /// EN: Registers a follower session and returns (session_id, receiver).
    pub fn register(&self) -> (u64, ShipReceiver) {
        let (tx, rx) = sc::hub_channel(self.queue_batches.max(1));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sessions.write().unwrap().push(SessionSlot { id, tx });
        (id, rx)
    }

    /// 注销 session（session 退出路径幂等调用）。
    /// EN: Unregisters a session (called idempotently on the session exit path).
    pub fn unregister(&self, id: u64) {
        self.sessions.write().unwrap().retain(|s| s.id != id);
    }

    /// 发布一批（wal worker 单发布者，flush 成功后、reply 前调用）。
    /// 队列满/已断开的 follower 立即踢出——慢 follower 不拖累 wal 写路径。
    /// EN: Publishes one batch (single publisher: the wal worker, after flush and
    /// before the reply). Followers whose queue is full or closed are kicked
    /// immediately — a slow follower must not stall the wal write path.
    pub fn publish(&self, lsn: u64, payload: &[u8]) {
        let batch = Arc::new(ShipBatch {
            lsn,
            payload: Arc::new(payload.to_vec()),
        });
        let mut kicks = Vec::new();
        let mut delivered = false;
        {
            let sessions = self.sessions.read().unwrap();
            for s in sessions.iter() {
                match hub_try_send(&s.tx, Arc::clone(&batch)) {
                    SendOutcome::Delivered => delivered = true,
                    SendOutcome::Full | SendOutcome::Closed => kicks.push(s.id),
                }
            }
        }
        // 指标按「批」计（无 session 时也算已生产——follower  catch-up 走文件）。
        // EN: Metrics count batches (even with no session a batch counts as produced —
        // followers catch up from the file).
        self.shipped_batches_total.fetch_add(1, Ordering::Relaxed);
        self.shipped_bytes_total
            .fetch_add(batch.payload.len() as u64, Ordering::Relaxed);
        let _ = delivered;
        if !kicks.is_empty() {
            let mut sessions = self.sessions.write().unwrap();
            for id in kicks {
                // drop sender → session recv 端得 Err → session 线程退出。
                // EN: Dropping the sender → the session's recv gets Err → the session exits.
                sessions.retain(|s| s.id != id);
                self.slow_follower_kicks_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    session_id = id,
                    "replication leader: slow follower kicked (queue full)"
                );
            }
        }
    }

    pub fn follower_count(&self) -> usize {
        self.sessions.read().unwrap().len()
    }

    pub fn slow_follower_kicks_total(&self) -> u64 {
        self.slow_follower_kicks_total.load(Ordering::Relaxed)
    }

    pub fn shipped_batches_total(&self) -> u64 {
        self.shipped_batches_total.load(Ordering::Relaxed)
    }

    pub fn shipped_bytes_total(&self) -> u64 {
        self.shipped_bytes_total.load(Ordering::Relaxed)
    }
}

// ── follower session（follower 会话）──────────────────────────
// EN: follower session section.

/// session 配置。
/// EN: Session configuration.
pub struct LeaderSessionConfig {
    /// 单 follower 队列上限（批），默认 4096。
    /// EN: Per-follower queue cap in batches (default 4096).
    pub queue_batches: usize,
    /// HEARTBEAT 间隔（live 模式空闲时）。
    /// EN: HEARTBEAT interval (when idle in live mode).
    pub heartbeat_interval: Duration,
    /// 连续无 ACK 超过此时长断连。
    /// EN: Disconnect after this long without any ACK.
    pub ack_timeout: Duration,
    /// catch-up poll 间隔（文件尾等待新写时）。
    /// EN: Catch-up poll interval (waiting for new writes at the file tail).
    pub catchup_poll_interval: Duration,
    /// 复制认证共享秘密（P4.6.3）：None = 不认证（可信网络/单机），
    /// Some = 握手 token 必须常量时间相等。follower 必须配置同一值。
    /// EN: shared-secret auth; None disables auth, Some requires an exact
    /// (constant-time) token match. The follower must configure the same value.
    pub auth_token: Option<String>,
    /// 本地 WAL 静态加密句柄（P4.7.3）：仅用于读本机段（catch-up 与指纹），
    /// 不影响线上传输格式。
    /// EN: local WAL encryption handle, used only for reading this node's
    /// segments (catch-up and fingerprints); the wire format is unchanged.
    pub encryption: Option<Arc<crate::encryption::Encryption>>,
}

/// **不要**用默认值重建生产配置：安全字段（如 `auth_token`）会静默丢失。
/// 生产装配统一走 [`LeaderSessionConfig::for_replication`]。
/// EN: never rebuild a production config from Default — security fields would
/// be silently dropped; use `for_replication`.
impl Default for LeaderSessionConfig {
    fn default() -> Self {
        Self {
            queue_batches: 4096,
            heartbeat_interval: Duration::from_secs(1),
            ack_timeout: Duration::from_secs(5),
            catchup_poll_interval: Duration::from_millis(20),
            auth_token: None,
            encryption: None,
        }
    }
}

impl LeaderSessionConfig {
    /// 生产装配的唯一入口（启动 leader 与 promote 共用）。
    ///
    /// WHY 不用 `..Default::default()` 字面量（OCR 批次二十九）：认证这类
    /// 安全字段一旦以默认值重建就会静默丢失（批次二十八的真实 HIGH）。
    /// 把「队列深度 + 认证 + 本地加密句柄」收进构造函数后，未来新增安全
    /// 字段只需改这里，两个装配点结构上不可能再分叉。
    /// EN: single production constructor shared by startup and promote — a
    /// future security field only needs wiring here, so the two sites cannot
    /// silently diverge again. Encryption (P4.7.3) was wired through here.
    ///
    /// **纪律（OCR 批次三十）**：生产装配点（houyi-server 的启动与 promote）
    /// 只允许调用本函数；结构体字面量 + `..Default::default()` 仅限测试里
    /// 覆盖计时参数。字段暂不私有化是因为跨 crate 的 turmoil 测试需要构造
    /// 自定义计时配置；若将来出现第三个生产装配点，应先把它并入本入口，
    /// 而不是复制字面量。
    /// EN: production sites must use this constructor; struct literals with
    /// `..Default::default()` are for tests overriding timing knobs only. A
    /// third production site must extend this entry point, not copy a literal.
    pub fn for_replication(
        queue_batches: usize,
        auth_token: Option<String>,
        encryption: Option<Arc<crate::encryption::Encryption>>,
    ) -> Self {
        Self {
            queue_batches,
            auth_token,
            encryption,
            ..Self::default()
        }
    }
}

/// ACK 水位线 + sync 等待注册表（P4.4.2）。
/// ACK watermarks plus the sync-wait registry.
///
/// 为什么把 waiter 段与水位线放进同一结构体：推进水位线（`on_ack`）与唤醒等待者
/// 必须是同一段逻辑——若拆成两个对象，session 要按序调两个入口，漏调/乱序即
/// 「水位线已满足但 waiter 睡到超时」。同构后 wait 的快路径读与 on_ack 的 CAS
/// 作用于同一组原子量，不存在分裂视角。
/// EN: Waiter segments live in the same struct as the watermarks because advancing
/// a watermark and waking its waiters must be one logic unit — split objects would
/// require the session to call two entry points in order, and a missed/reordered
/// call strands waiters until timeout.
///
/// 等待语义 / Wait semantics:
/// - 快路径：水位线已 ≥ lsn → 零注册立即 Satisfied。
/// - 无健康 follower（`has_followers=false`，由调用方读 ShipHub 现值）→ 立即
///   NoFollower 降级，不等待（裁决：无 follower 时 sync 写不得阻塞）。
/// - 注册后复检水位线，关闭「检查后、注册前 ACK 到达」的错失唤醒窗口
///   （on_ack 先 fetch_max 再扫段；复检在注册后，故必见其一）。
/// - 超时摘除并计数，语义降级为 async（写已本地 durable，由 server 侧告警）。
///
/// group commit 效应在 ACK 侧：follower 一次 fsync + ACK(DURABLE, max_lsn)
/// 推进水位线，on_ack 一次扫段唤醒全部 ≤ 水位线的 waiter，无需逐笔往返。
/// EN: Group commit materializes on the ACK side: one follower fsync + ACK
/// advances the watermark and wakes every waiter at or below it.
///
/// 分段：16 段按 lsn 低位散列，避免全局 waiter 锁（铁律）；on_ack 扫全部 16 段
/// （ACK 是批级事件，非每记录一次，16 次短锁开销可忽略）。
pub struct AckWatermarks {
    /// 所有活跃 session 的 RECEIVED 水位（max 聚合——v1 任一 ACK 即满足）。
    /// EN: RECEIVED watermark across all active sessions (max-aggregated — v1
    /// satisfies on any ACK).
    pub recv_lsn: AtomicU64,
    pub durable_lsn: AtomicU64,
    /// 各阶段是否收过至少一条 ACK（打破 LSN 0 的原点二义性，按阶段独立——
    /// 收过 RECEIVED 不得让 DURABLE 水位线的 0 显得"已确认"，OCR f942e45[1]）。
    /// EN: Whether each stage has seen at least one ACK (breaks the LSN 0 origin
    /// ambiguity, per stage — a RECEIVED ack must not make DURABLE's 0 look
    /// "confirmed", OCR f942e45[1]).
    recv_acked: AtomicBool,
    durable_acked: AtomicBool,
    waiters: [Mutex<Vec<WaiterEntry>>; WAITER_SEGMENTS],
    next_waiter_id: AtomicU64,
    // ── sync 等待指标（Relaxed，role=leader 才输出）──
    // EN: Sync-wait metrics (Relaxed; exported only when role=leader).
    sync_waits_total: AtomicU64,
    sync_timeouts_total: AtomicU64,
    sync_no_follower_total: AtomicU64,
    sync_wait_nanos_total: AtomicU64,
}

/// sync 等待结果 / Outcome of a sync wait.
/// EN: sync wait outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncWaitOutcome {
    /// 水位线已满足（含注册前即满足的快路径）/ Watermark reached.
    /// EN: Watermark reached (including the fast path where it held before registration).
    Satisfied,
    /// 无健康 follower，立即降级（不计超时）/ No follower — degraded instantly.
    /// EN: No healthy follower; degrade immediately (not counted as a timeout).
    NoFollower,
    /// 等待超时，语义降级为 async / Timed out — degraded to async.
    /// EN: Wait timed out; semantics degrade to async.
    Timeout,
}

const WAITER_SEGMENTS: usize = 16;

struct WaiterEntry {
    id: u64,
    lsn: u64,
    stage: u8,
    tx: WakerSender<()>,
}

impl AckWatermarks {
    pub fn new() -> Self {
        Self {
            recv_lsn: AtomicU64::new(0),
            durable_lsn: AtomicU64::new(0),
            recv_acked: AtomicBool::new(false),
            durable_acked: AtomicBool::new(false),
            waiters: std::array::from_fn(|_| Mutex::new(Vec::new())),
            next_waiter_id: AtomicU64::new(1),
            sync_waits_total: AtomicU64::new(0),
            sync_timeouts_total: AtomicU64::new(0),
            sync_no_follower_total: AtomicU64::new(0),
            sync_wait_nanos_total: AtomicU64::new(0),
        }
    }

    fn watermark_for(&self, stage: u8) -> &AtomicU64 {
        if stage == proto::ACK_STAGE_DURABLE {
            &self.durable_lsn
        } else {
            &self.recv_lsn
        }
    }

    /// stage → acked 标志的唯一派发点（OCR 6bfb210[1]）：covers/on_ack 共用，
    /// 未来新增第三阶段时改这里一处，杜绝「常量加了、分支漏改」让新阶段
    /// 静默落入 RECEIVED 侧——重新引入本变更消灭的跨阶段虚满足类。
    /// EN: Single dispatch point for stage → acked flag (OCR 6bfb210[1]): shared
    /// by covers/on_ack. Adding a third stage later means editing only this spot,
    /// preventing "constant added, branch missed" from silently dropping the new
    /// stage into the RECEIVED side — reintroducing the cross-stage false
    /// satisfaction class this change eliminated.
    fn acked_flag_for(&self, stage: u8) -> &AtomicBool {
        debug_assert!(
            stage == proto::ACK_STAGE_RECEIVED || stage == proto::ACK_STAGE_DURABLE,
            "unknown ACK stage {stage}"
        );
        if stage == proto::ACK_STAGE_DURABLE {
            &self.durable_acked
        } else {
            &self.recv_acked
        }
    }

    /// 水位线是否真正覆盖 `lsn`。
    ///
    /// 原点二义性：LSN 从 0 开始，而「最大已确认 LSN」约定下初始值 0 同时是
    /// 「什么都没确认过」——`0 >= 0` 会把首条记录的等待虚满足。用**本阶段**
    /// 是否收过 ACK 打破二义（OCR f942e45[1]：单全局标志会让 RECEIVED ACK
    /// 虚满足 DURABLE 的 lsn 0——false durability，杀主即丢数据）。
    /// EN: LSNs start at 0, so an initial watermark of 0 is ambiguous with
    /// "nothing acked yet". The disambiguator must be per-stage: a RECEIVED
    /// ack must never make DURABLE lsn 0 look satisfied.
    fn covers(&self, stage: u8, lsn: u64) -> bool {
        self.acked_flag_for(stage).load(Ordering::Relaxed)
            && self.watermark_for(stage).load(Ordering::Relaxed) >= lsn
    }

    /// 阻塞等待 `stage` 水位线达到 `lsn`（std 线程世界；async 调用方须经
    /// spawn_blocking 桥接，不得直接占住 tokio worker）。
    /// EN: Blocking wait — async callers must bridge via spawn_blocking.
    ///
    /// `has_followers` 由调用方在读侧取 ShipHub 现值传入：水位线类型不反向
    /// 依赖 ShipHub（保持 persistence 内类型单向依赖）。
    pub fn wait(
        &self,
        lsn: u64,
        stage: u8,
        has_followers: bool,
        timeout: Duration,
    ) -> SyncWaitOutcome {
        let start = Instant::now();
        self.sync_waits_total.fetch_add(1, Ordering::Relaxed);
        let outcome = self.wait_inner(lsn, stage, has_followers, timeout);
        self.sync_wait_nanos_total
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        outcome
    }

    fn wait_inner(
        &self,
        lsn: u64,
        stage: u8,
        has_followers: bool,
        timeout: Duration,
    ) -> SyncWaitOutcome {
        if self.covers(stage, lsn) {
            return SyncWaitOutcome::Satisfied;
        }
        if !has_followers {
            self.sync_no_follower_total.fetch_add(1, Ordering::Relaxed);
            return SyncWaitOutcome::NoFollower;
        }
        let (tx, rx) = sc::channel::<()>();
        let id = self.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let seg = &self.waiters[(lsn as usize) & (WAITER_SEGMENTS - 1)];
        seg.lock().unwrap().push(WaiterEntry { id, lsn, stage, tx });
        // 注册后复检：on_ack 的顺序是 fetch_max → 扫段。若复检读到旧水位，
        // 则 fetch_max 必发生在注册之后，其扫段必见本 entry——不会错失唤醒。
        // EN: Re-check after registration closes the missed-wakeup window.
        if self.covers(stage, lsn) {
            seg.lock().unwrap().retain(|w| w.id != id);
            return SyncWaitOutcome::Satisfied;
        }
        match recv_timeout(&rx, timeout) {
            Ok(()) => SyncWaitOutcome::Satisfied,
            Err(RecvTimeoutError::Timeout) => {
                seg.lock().unwrap().retain(|w| w.id != id);
                self.sync_timeouts_total.fetch_add(1, Ordering::Relaxed);
                SyncWaitOutcome::Timeout
            }
            // sender 侧移除 entry 前必然先 send（on_ack）或随本线程超时摘除
            // （此后 rx 不再使用）——Disconnected 当前不可达；防御分支按失败
            // 安全处理（OCR f942e45[2]）：无 ACK 证据绝不虚报 durable，
            // 降级 async + 计超时（同步摘除 entry 保持计数语义一致）。
            // EN: The sender always sends (on_ack) before removing the entry, or this
            // thread removes it on timeout (rx is unused afterward) — Disconnected is
            // currently unreachable; the defensive branch fails safe (OCR f942e45[2]):
            // no ACK evidence means never report durable; degrade to async and count a
            // timeout (remove the entry synchronously to keep counting consistent).
            Err(RecvTimeoutError::Disconnected) => {
                seg.lock().unwrap().retain(|w| w.id != id);
                self.sync_timeouts_total.fetch_add(1, Ordering::Relaxed);
                SyncWaitOutcome::Timeout
            }
        }
    }

    /// 推进水位线并唤醒覆盖的 waiter。**仅 session ACK reader 与测试可调用**——
    /// 任何其他调用方推进水位线都是在伪造 follower 的持久性确认。
    /// EN: Advance a watermark and wake covered waiters. Callers: the session
    /// ACK reader and tests only — anything else would forge follower acks.
    pub fn on_ack(&self, stage: u8, lsn: u64) {
        let wm = self.watermark_for(stage);
        wm.fetch_max(lsn, Ordering::Relaxed);
        self.acked_flag_for(stage).store(true, Ordering::Relaxed);
        // 扫段唤醒：本 stage 水位线覆盖的全部 waiter 一次唤醒（group commit）。
        // EN: Wake every waiter covered by this stage's watermark.
        let now = wm.load(Ordering::Relaxed);
        for seg in &self.waiters {
            let mut guard = seg.lock().unwrap();
            guard.retain(|w| {
                if w.stage == stage && w.lsn <= now {
                    // unbounded channel：send 永不阻塞，锁内投递安全。
                    // EN: unbounded channel: send never blocks, so delivery under the lock is safe.
                    let _ = w.tx.send(());
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn sync_waits_total(&self) -> u64 {
        self.sync_waits_total.load(Ordering::Relaxed)
    }

    pub fn sync_timeouts_total(&self) -> u64 {
        self.sync_timeouts_total.load(Ordering::Relaxed)
    }

    pub fn sync_no_follower_total(&self) -> u64 {
        self.sync_no_follower_total.load(Ordering::Relaxed)
    }

    pub fn sync_wait_nanos_total(&self) -> u64 {
        self.sync_wait_nanos_total.load(Ordering::Relaxed)
    }

    /// 测试/诊断观测点：当前注册中的 waiter 数。
    /// EN: Test/diagnostic observation point: number of currently registered waiters.
    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.waiters.iter().map(|s| s.lock().unwrap().len()).sum()
    }
}

impl Default for AckWatermarks {
    fn default() -> Self {
        Self::new()
    }
}

/// leader 复制服务：监听 follower 连接，每连接一条 session（tokio task）。
/// EN: Leader replication service: accepts follower connections, one session per
/// connection (tokio task).
#[cfg(not(loom))]
pub struct ReplicationLeader {
    shutdown: Arc<AtomicBool>,
    acceptor: Mutex<Option<tokio::task::JoinHandle<()>>>,
    sessions: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    local_addr: std::net::SocketAddr,
}

#[cfg(not(loom))]
impl ReplicationLeader {
    /// 绑定监听并启动 accept 循环。`hub` 与 `wal_path` 为 stream 0
    /// （collections.wal）的扇出枢纽与尾部数据源；`leader_next_lsn` 由
    /// 调用方提供闭包（读 WalAsyncHandle::next_lsn），握手时取现值。
    ///
    /// 必须在 tokio 运行时上下文中调用（turmoil 仿真时为其模拟运行时）。
    /// EN: must run inside a tokio runtime (or turmoil's simulated one).
    pub async fn start(
        listen_addr: &str,
        hub: Arc<ShipHub>,
        wal_path: std::path::PathBuf,
        leader_next_lsn: Arc<dyn Fn() -> u64 + Send + Sync>,
        watermarks: Arc<AckWatermarks>,
        config: LeaderSessionConfig,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        tracing::info!(%local_addr, "replication leader: listening");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let acceptor = {
            let shutdown = Arc::clone(&shutdown);
            let sessions = Arc::clone(&sessions);
            tokio::spawn(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept().await {
                        Ok((stream, addr)) => {
                            tracing::info!(%addr, "replication leader: follower connected");
                            let hub = Arc::clone(&hub);
                            let wal_path = wal_path.clone();
                            let next = Arc::clone(&leader_next_lsn);
                            let wm = Arc::clone(&watermarks);
                            let cfg = LeaderSessionConfig {
                                queue_batches: config.queue_batches,
                                heartbeat_interval: config.heartbeat_interval,
                                ack_timeout: config.ack_timeout,
                                catchup_poll_interval: config.catchup_poll_interval,
                                auth_token: config.auth_token.clone(),
                                encryption: config.encryption.clone(),
                            };
                            let handle = tokio::spawn(async move {
                                if let Err(e) =
                                    serve_session(stream, hub, &wal_path, next, wm, &cfg).await
                                {
                                    tracing::warn!(%addr, error = %e,
                                        "replication leader: session ended with error");
                                }
                            });
                            sessions.lock().unwrap().push(handle);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "replication leader: accept error");
                            runtime::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            })
        };
        Ok(Self {
            shutdown,
            acceptor: Mutex::new(Some(acceptor)),
            sessions,
            local_addr,
        })
    }

    /// 实际绑定地址（`:0` 自动分配端口后的观测点）。
    /// EN: Actual bound address (observation point after `:0` auto-allocation).
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        Ok(self.local_addr)
    }

    /// 关停：abort accept 与全部 session task。
    ///
    /// 为什么 abort 而不是设标志等自然退出（WHY）：writer 可能永久阻塞在
    /// `write_all`（对端不读，发送缓冲满）——旧 std 线程版会把它泄漏成一个
    /// 永不回收的线程。task abort 在下一个 await 点取消整个 future，`SessionGuard`
    /// 的 Drop 随之执行（hub 注销）。每个任务带 5s 收尾预算，超时只 warn，
    /// 不阻塞进程关停。EN: abort cancels a writer wedged in `write_all`; the
    /// guard drops (hub unregister) and each task gets a 5s join budget.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // take 出局部变量后立即释放 std 锁（guard 跨 await 会让 future 非 Send）。
        // EN: Move the handle into a local and drop the std lock immediately (a guard
        // held across await makes the future non-Send).
        let acceptor = self.acceptor.lock().unwrap().take();
        if let Some(h) = acceptor {
            h.abort();
            let _ = runtime::timeout(Duration::from_secs(5), h).await;
        }
        let handles: Vec<tokio::task::JoinHandle<()>> =
            std::mem::take(&mut *self.sessions.lock().unwrap());
        for h in &handles {
            h.abort();
        }
        for h in handles {
            if runtime::timeout(Duration::from_secs(5), h).await.is_err() {
                tracing::warn!(
                    "replication leader: session task did not finish within 5s after abort"
                );
            }
        }
    }
}

/// session 单次 IO 的硬超时：对端不读/不写时任务必须能自己退出。
/// EN: per-IO hard timeout so a silent peer cannot pin the task.
#[cfg(not(loom))]
const SESSION_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 单 follower session：握手 → catch-up → live 推送；ACK reader 并行 task。
/// EN: One follower session: handshake → catch-up → live push; a parallel ACK
/// reader task.
#[cfg(not(loom))]
async fn serve_session(
    stream: TcpStream,
    hub: Arc<ShipHub>,
    wal_path: &std::path::Path,
    leader_next_lsn: Arc<dyn Fn() -> u64 + Send + Sync>,
    watermarks: Arc<AckWatermarks>,
    config: &LeaderSessionConfig,
) -> io::Result<()> {
    let _ = stream.set_nodelay(true);

    // 1) HANDSHAKE（读写都在超时内完成）。
    //    注意先拿所有权再 split：握手需要双向。
    // EN: 1) HANDSHAKE (both directions complete within the timeout). Note: take
    // ownership before splitting — the handshake needs both halves.
    let mut stream = stream;
    let mut hbuf = [0u8; proto::HANDSHAKE_PREFIX_LEN];
    runtime::timeout(SESSION_IO_TIMEOUT, stream.read_exact(&mut hbuf))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake read timeout"))??;
    let prefix = match proto::decode_handshake_prefix(&hbuf) {
        Ok(v) => v,
        Err(e) => {
            let _ = runtime::timeout(
                SESSION_IO_TIMEOUT,
                stream.write_all(&proto::encode_error(
                    proto::ERROR_KIND_GENERIC,
                    &format!("handshake: {e}"),
                )),
            )
            .await;
            return Err(e);
        }
    };
    // 1a) 认证（P4.6.3）：token 本体读满后常量时间比较；失败发结构化 AUTH
    //     kind（follower 据此走长退避而不是普通重连风暴）。
    //     EN: read the token body and compare constant-time; on failure emit the
    //     structured AUTH kind so the follower backs off for manual repair.
    let mut tbuf = vec![0u8; usize::from(prefix.token_len)];
    match runtime::timeout(SESSION_IO_TIMEOUT, stream.read_exact(&mut tbuf)).await {
        Ok(Ok(_)) => {}
        read_err => {
            let e = match read_err {
                Ok(Err(io_err)) => io_err,
                _ => io::Error::new(io::ErrorKind::TimedOut, "handshake token read timeout"),
            };
            let _ = runtime::timeout(
                SESSION_IO_TIMEOUT,
                stream.write_all(&proto::encode_error(
                    proto::ERROR_KIND_GENERIC,
                    &format!("handshake token: {e}"),
                )),
            )
            .await;
            return Err(e);
        }
    }
    let presented_token = match proto::decode_handshake_token(&tbuf) {
        Ok(t) => t,
        Err(e) => {
            let _ = runtime::timeout(
                SESSION_IO_TIMEOUT,
                stream.write_all(&proto::encode_error(
                    proto::ERROR_KIND_GENERIC,
                    &format!("handshake token: {e}"),
                )),
            )
            .await;
            return Err(e);
        }
    };
    if !proto::tokens_match(config.auth_token.as_deref(), &presented_token) {
        let msg = "replication auth failed: token mismatch or missing";
        let _ = runtime::timeout(
            SESSION_IO_TIMEOUT,
            stream.write_all(&proto::encode_error(proto::ERROR_KIND_AUTH, msg)),
        )
        .await;
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
    }
    let proto::HandshakePrefix {
        resume_lsn,
        stream_mask,
        log_hash: follower_hash,
        ..
    } = prefix;
    if stream_mask & proto::STREAM_MASK_COLLECTIONS == 0 {
        let e = io::Error::new(
            io::ErrorKind::InvalidData,
            "stream_mask missing collections bit",
        );
        let _ = runtime::timeout(
            SESSION_IO_TIMEOUT,
            stream.write_all(&proto::encode_error(
                proto::ERROR_KIND_GENERIC,
                "stream_mask must include stream 0",
            )),
        )
        .await;
        return Err(e);
    }
    let leader_next = leader_next_lsn();
    if resume_lsn > leader_next {
        // follower_ahead = 脑裂信号：拒绝并显式报错，人工处置。
        // EN: follower_ahead = split-brain signal: reject with an explicit error for
        // manual action.
        let msg =
            format!("follower ahead: resume_lsn {resume_lsn} > leader_next_lsn {leader_next}");
        let _ = runtime::timeout(
            SESSION_IO_TIMEOUT,
            stream.write_all(&proto::encode_error(proto::ERROR_KIND_FOLLOWER_AHEAD, &msg)),
        )
        .await;
        return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
    }

    // 1.5) 日志指纹：长度相同必须内容相同。
    //
    // 为什么放在 ahead 检查之后（WHY）：resume_lsn 超前时 leader 没有对应的
    // 历史可哈希（prefix_hash 会因「历史短于 resume」报错），先按既有语义
    // 返回 follower_ahead。长度不超前但内容不同 = 分叉，必须拒绝——否则
    // follower 的 `lsn < expected` 幂等跳过会把同 LSN 的两条不同记录永久
    // 固化（P4.5.6 模型反例）。
    // EN: run after the ahead check (which handles resume > leader_next); an
    // equal-length but different history is divergence and must be rejected,
    // or the follower's idempotent skip freezes two different records at one
    // LSN (the P4.5.6 counterexample).
    //
    // 失败必须先回结构化 ERROR 再断开（OCR 批次二十七）：裸 EOF 会让 follower
    // 把「本地日志损坏/格式不符」误分类成网络抖动，陷入通用重连风暴。
    // EN: send a structured ERROR before closing (batch 27) — a bare EOF makes
    // the follower misclassify a local-log fault as a network flap.
    let leader_hash = match crate::log_fingerprint::prefix_hash_blocking_with_encryption(
        wal_path.to_path_buf(),
        resume_lsn,
        config.encryption.clone(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            let msg = format!("log fingerprint failed at resume_lsn {resume_lsn}: {e}");
            let _ = runtime::timeout(
                SESSION_IO_TIMEOUT,
                stream.write_all(&proto::encode_error(proto::ERROR_KIND_GENERIC, &msg)),
            )
            .await;
            return Err(e);
        }
    };
    if leader_hash != follower_hash {
        let msg = format!(
            "log diverged at resume_lsn {resume_lsn}: follower_hash={follower_hash:#018x} \
             leader_hash={leader_hash:#018x}"
        );
        let _ = runtime::timeout(
            SESSION_IO_TIMEOUT,
            stream.write_all(&proto::encode_error(proto::ERROR_KIND_DIVERGED, &msg)),
        )
        .await;
        return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
    }

    // 2) WELCOME
    runtime::timeout(
        SESSION_IO_TIMEOUT,
        stream.write_all(&proto::encode_welcome(leader_next)),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "welcome write timeout"))??;

    // 3) 先注册后 catch-up：衔接缝隙（注册后到达的 live 批与文件 catch-up
    //    重叠）由 follower 幂等吸收（lsn < expected 跳过仍 ACK）。
    // EN: 3) Register before catch-up: the seam (live batches arriving after
    // registration overlap with file catch-up) is absorbed idempotently by the
    // follower (lsn < expected is skipped but still ACKed).
    let (session_id, mut rx) = hub.register();
    let _guard = SessionGuard {
        hub: Arc::clone(&hub),
        id: session_id,
    };

    // 4) 双向拆分：ACK reader 用读半，writer 用写半。
    // EN: 4) Bidirectional split: the ACK reader uses the read half, the writer
    // the write half.
    let (mut reader, mut writer) = tokio::io::split(stream);

    // 5) ACK reader task（超时/断连 → ack_dead 置位 → session 终止）。
    // EN: 5) ACK reader task (timeout/disconnect → ack_dead set → session terminates).
    let ack_dead = Arc::new(AtomicBool::new(false));
    let ack_timeout = config.ack_timeout;
    let ack_handle = {
        let wm = Arc::clone(&watermarks);
        let dead = Arc::clone(&ack_dead);
        tokio::spawn(async move {
            let mut last_ack = Instant::now();
            loop {
                if last_ack.elapsed() > ack_timeout {
                    dead.store(true, Ordering::Relaxed);
                    return Err::<(), io::Error>(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ack timeout",
                    ));
                }
                // 500ms 轮询**只包 tag 的读取**：超时发生时一个字节都没消费，
                // 可以安全地进入下一轮。tag 一旦读到，帧头剩余字节必须在同一次
                // 会话内读完——否则超时会在字节流中间重启，下一次把帧头当 tag，
                // 链路永久失步（OCR 批次二十三）。
                // EN: the 500ms poll wraps only the tag read (nothing consumed on
                // timeout). Once the tag is read, the rest of the header must be
                // consumed in the same pass or the stream desynchronizes.
                let mut tag = [0u8; 1];
                match runtime::timeout(Duration::from_millis(500), reader.read_exact(&mut tag))
                    .await
                {
                    Err(_) => continue,
                    Ok(Err(e)) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                    Ok(Ok(_)) => {}
                }
                let hlen = match proto::frame_header_len(tag[0]) {
                    Ok(n) => n,
                    Err(e) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                };
                let mut frame = vec![0u8; hlen];
                frame[0] = tag[0];
                match runtime::timeout(SESSION_IO_TIMEOUT, reader.read_exact(&mut frame[1..])).await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                    Err(_) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "ack frame header read timeout",
                        ));
                    }
                }
                match proto::decode_frame_header(&frame) {
                    Ok(proto::FrameHeader::Ack { stage, lsn, .. }) => {
                        wm.on_ack(stage, lsn);
                        last_ack = Instant::now();
                    }
                    Ok(_) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "expected ACK frame from follower",
                        ));
                    }
                    Err(e) => {
                        dead.store(true, Ordering::Relaxed);
                        return Err(e);
                    }
                }
            }
        })
    };

    // 6) catch-up：WalTailReader 从 resume_lsn 推文件，追平后切 live。
    // EN: 6) catch-up: WalTailReader streams the file from resume_lsn, then switches
    // to live.
    let result = session_body(
        &mut writer,
        &mut rx,
        wal_path,
        resume_lsn,
        &leader_next_lsn,
        &ack_dead,
        config,
    )
    .await;

    ack_handle.abort();
    let _ = ack_handle.await;
    result
}

/// 会话主体：live 队列 + 文件尾部归并推送，空闲时发心跳。
/// EN: Session body: merges the live queue with file-tail pushes; sends heartbeats
/// when idle.
#[allow(clippy::too_many_arguments)]
#[cfg(not(loom))]
async fn session_body<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    rx: &mut ShipReceiver,
    wal_path: &std::path::Path,
    resume_lsn: u64,
    leader_next_lsn: &Arc<dyn Fn() -> u64 + Send + Sync>,
    ack_dead: &Arc<AtomicBool>,
    config: &LeaderSessionConfig,
) -> io::Result<()> {
    // 初次打开要整段读 + （v3）逐条 AES-GCM 解密才能定位 resume_lsn；大 WAL
    // 上秒级，不能钉住 tokio worker（与指纹扫描同一纪律）。后续 poll 只读新增
    // 尾部，量级小得多。
    // EN: the initial open scans (and decrypts) the whole segment to reach
    // resume_lsn — move it off the runtime worker like the fingerprint scan.
    let tail_path = wal_path.to_path_buf();
    let tail_encryption = config.encryption.clone();
    let mut tail = tokio::task::spawn_blocking(move || {
        WalTailReader::open_with_encryption(&tail_path, resume_lsn, tail_encryption)
    })
    .await
    .map_err(|e| io::Error::other(format!("wal tail open join failed: {e}")))??;
    let mut pending: VecDeque<Arc<ShipBatch>> = VecDeque::new();
    loop {
        // ACK reader 死亡（超时/断连）→ session 终止。
        // EN: ACK reader died (timeout/disconnect) → terminate the session.
        if ack_dead.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "ack reader dead",
            ));
        }
        // 优先发 live 队列（注册后已攒的批），再扫文件——两者按 LSN 归并
        // 去重交给 follower 幂等层，这里只需保证「都发到」。
        // EN: Drain live batches first (accumulated after registration), then scan the
        // file — LSN dedup is delegated to the follower's idempotent layer; here we
        // only ensure everything is sent.
        loop {
            match hub_try_recv(rx) {
                RecvOutcome::Item(batch) => pending.push_back(batch),
                RecvOutcome::Empty => break,
                RecvOutcome::Closed => break,
            }
        }
        if let Some(batch) = pending.pop_front() {
            write_data_frame(writer, &batch).await?;
            continue;
        }
        let entries = tail.poll()?;
        if entries.is_empty() {
            // 追平且无 live 批：等 live 批（带心跳间隔超时）。
            // EN: Caught up and no live batch: wait for one (with the heartbeat-interval
            // timeout).
            match runtime::timeout(config.heartbeat_interval, sc::hub_recv_async(rx)).await {
                Ok(Some(batch)) => write_data_frame(writer, &batch).await?,
                Ok(None) => return Ok(()), // hub 关闭/被踢出
                Err(_) => {
                    runtime::timeout(
                        SESSION_IO_TIMEOUT,
                        writer.write_all(&proto::encode_heartbeat(
                            proto::STREAM_COLLECTIONS,
                            leader_next_lsn(),
                        )),
                    )
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "heartbeat write timeout")
                    })??;
                    runtime::timeout(SESSION_IO_TIMEOUT, writer.flush())
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "heartbeat flush timeout")
                        })??;
                }
            }
            continue;
        }
        for e in entries {
            let batch = Arc::new(ShipBatch {
                lsn: e.lsn,
                payload: Arc::new(e.payload),
            });
            write_data_frame(writer, &batch).await?;
        }
    }
}

#[cfg(not(loom))]
struct SessionGuard {
    hub: Arc<ShipHub>,
    id: u64,
}

#[cfg(not(loom))]
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.hub.unregister(self.id);
    }
}

/// DATA 帧 = 头 + payload 两段写（tokio `write_all` 内部处理短写）。
///
/// 短写纪律（OCR 25b92a4[1]）由 tokio 的 `write_all` 承担：它循环到整段写完，
/// 不像裸 `writev(2)` 那样返回部分长度。每次写仍包一层超时——对端不读时不能让
/// 任务永久挂起（turmoil 下同样需要，否则仿真无法推进）。
/// EN: tokio's `write_all` owns the short-write loop; the timeout keeps a
/// non-reading peer from pinning the task (also required under turmoil).
#[cfg(not(loom))]
async fn write_data_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    batch: &ShipBatch,
) -> io::Result<()> {
    let header = proto::encode_data_header(
        proto::STREAM_COLLECTIONS,
        batch.lsn,
        batch.payload.len() as u32,
    );
    runtime::timeout(SESSION_IO_TIMEOUT, writer.write_all(&header))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "data header write timeout"))??;
    runtime::timeout(
        SESSION_IO_TIMEOUT,
        writer.write_all(batch.payload.as_slice()),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "data payload write timeout"))??;
    runtime::timeout(SESSION_IO_TIMEOUT, writer.flush())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "data flush timeout"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等 waiter 注册：deadline 轮询消 CI 调度竞争（OCR f942e45[8]：固定
    /// yield 预算在满载 runner 上会让 waiter 错过 ACK 窗口，假 Timeout 比
    /// 计数断言失败更难归因）。
    /// EN: Waits for waiter registration: deadline polling removes CI scheduling
    /// races (OCR f942e45[8]: a fixed yield budget under a loaded runner can miss
    /// the ACK window; a false Timeout is harder to attribute than a count failure).
    fn wait_registered(wm: &AckWatermarks, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while wm.waiter_count() < n {
            assert!(
                Instant::now() < deadline,
                "waiters must register before on_ack fires"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn hub_fanout_delivers_to_all_sessions() {
        let hub = ShipHub::new(8);
        let (_id1, mut rx1) = hub.register();
        let (_id2, mut rx2) = hub.register();
        hub.publish(7, b"payload-7");
        let b1 = hub_recv_blocking(&mut rx1).unwrap();
        let b2 = hub_recv_blocking(&mut rx2).unwrap();
        assert_eq!(b1.lsn, 7);
        assert_eq!(&b1.payload[..], b"payload-7");
        assert_eq!(&b2.payload[..], b"payload-7");
        // 同一 Arc 扇出（零拷贝）
        // EN: Same Arc fan-out (zero copy).
        assert!(Arc::ptr_eq(&b1.payload, &b2.payload));
        assert_eq!(hub.shipped_batches_total(), 1);
        assert_eq!(hub.shipped_bytes_total(), 9);
    }

    #[test]
    fn hub_kicks_slow_follower_and_keeps_fast() {
        let hub = ShipHub::new(2);
        let (_slow, _rx_slow) = hub.register(); // 从不取 → 队列积满
        let (_fast, mut rx_fast) = hub.register();
        hub.publish(0, b"a"); // slow: 1/2
        assert_eq!(hub_recv_blocking(&mut rx_fast).unwrap().lsn, 0); // fast 及时取走
        hub.publish(1, b"b"); // slow: 2/2
        assert_eq!(hub_recv_blocking(&mut rx_fast).unwrap().lsn, 1);
        hub.publish(2, b"c"); // slow 满 → 踢
        assert_eq!(hub_recv_blocking(&mut rx_fast).unwrap().lsn, 2);
        assert_eq!(hub.slow_follower_kicks_total(), 1);
        assert_eq!(hub.follower_count(), 1);
        // 被踢后 publish 不再尝试 slow（计数不再涨）
        // EN: After the kick, publish no longer tries slow (the count stops growing).
        hub.publish(3, b"d");
        assert_eq!(hub.slow_follower_kicks_total(), 1);
        assert_eq!(hub_recv_blocking(&mut rx_fast).unwrap().lsn, 3);
    }

    #[test]
    fn hub_kicks_disconnected_session() {
        let hub = ShipHub::new(4);
        let (id, rx) = hub.register();
        drop(rx); // 接收端断开
        hub.publish(0, b"x");
        assert_eq!(hub.slow_follower_kicks_total(), 1);
        assert_eq!(hub.follower_count(), 0);
        let _ = id;
    }

    #[test]
    fn hub_unregister_is_idempotent() {
        let hub = ShipHub::new(4);
        let (id, _rx) = hub.register();
        hub.unregister(id);
        hub.unregister(id);
        assert_eq!(hub.follower_count(), 0);
    }

    #[test]
    fn ack_watermarks_max_cas() {
        let wm = AckWatermarks::new();
        wm.on_ack(proto::ACK_STAGE_RECEIVED, 10);
        wm.on_ack(proto::ACK_STAGE_RECEIVED, 5); // 回退不生效
        wm.on_ack(proto::ACK_STAGE_DURABLE, 8);
        assert_eq!(wm.recv_lsn.load(Ordering::Relaxed), 10);
        assert_eq!(wm.durable_lsn.load(Ordering::Relaxed), 8);
    }

    // ── DATA 帧写入（OCR 25b92a4[1] 回归） ───────────────────
    // EN: DATA frame writes (OCR 25b92a4[1] regression).

    /// 短写 writer：每次 `poll_write` 最多吐 `max` 字节，复刻 socket 发送缓冲
    /// 写满时的短写行为（tokio 的 `write_all` 必须自行重试到整段写完）。
    /// EN: Short-write writer: each `poll_write` emits at most `max` bytes,
    /// reproducing socket send-buffer short writes (tokio's `write_all` must retry
    /// until everything is written).
    struct TrickleWriter {
        written: Vec<u8>,
        max: usize,
    }

    impl tokio::io::AsyncWrite for TrickleWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            let n = buf.len().min(self.max);
            self.written.extend_from_slice(&buf[..n]);
            std::task::Poll::Ready(Ok(n))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// 回归：短写不得截断 DATA 帧。若写出只推进一部分就返回，follower 会把
    /// 下一帧的头字节当 payload 解析，并在「帧头合法」前提下 apply+ACK 腐坏
    /// 数据（重连不可修复：resume_lsn 已越过该批）。tokio 的 `write_all` 负责
    /// 重试，本用例钉死该契约在换型后依然成立。
    /// EN: Regression: short writes must not truncate a DATA frame. If a write
    /// returns after partial progress, the follower parses the next frame's header
    /// bytes as payload and, under a "valid header", applies+ACKs corrupt data
    /// (unrecoverable by reconnect: resume_lsn already passed the batch). tokio's
    /// `write_all` handles the retry; this test pins that contract after the swap.
    #[cfg(not(loom))]
    #[tokio::test]
    async fn data_frame_survives_short_writes() {
        let payload = vec![0xA5u8; 64];
        let batch = ShipBatch {
            lsn: 7,
            payload: Arc::new(payload.clone()),
        };
        let mut w = TrickleWriter {
            written: Vec::new(),
            max: 3,
        };
        write_data_frame(&mut w, &batch)
            .await
            .expect("short writes must be retried, not dropped");

        let header = proto::encode_data_header(proto::STREAM_COLLECTIONS, 7, payload.len() as u32);
        let mut want = header.to_vec();
        want.extend_from_slice(&payload);
        assert_eq!(w.written, want, "DATA frame must land in full");
    }

    // ── P4.4.2 sync 等待 ─────────────────────────────────────
    // EN: P4.4.2 sync waits.

    #[test]
    fn sync_wait_fast_path_when_watermark_already_satisfied() {
        let wm = AckWatermarks::new();
        wm.on_ack(proto::ACK_STAGE_DURABLE, 10);
        let outcome = wm.wait(
            10,
            proto::ACK_STAGE_DURABLE,
            false, // 无 follower 也走快路径——水位线已满足
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::Satisfied);
        assert_eq!(wm.sync_waits_total(), 1);
        assert_eq!(wm.sync_no_follower_total(), 0);
        assert_eq!(wm.waiter_count(), 0, "fast path must not register");
    }

    #[test]
    fn sync_wait_no_follower_degrades_immediately() {
        let wm = AckWatermarks::new();
        let start = Instant::now();
        let outcome = wm.wait(
            5,
            proto::ACK_STAGE_RECEIVED,
            false,
            Duration::from_secs(60), // 不得真正等待
        );
        assert_eq!(outcome, SyncWaitOutcome::NoFollower);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "no-follower must return immediately"
        );
        assert_eq!(wm.sync_no_follower_total(), 1);
        assert_eq!(wm.sync_timeouts_total(), 0);
        assert_eq!(wm.waiter_count(), 0);
    }

    #[test]
    fn sync_wait_timeout_removes_entry_and_counts() {
        let wm = AckWatermarks::new();
        let outcome = wm.wait(
            5,
            proto::ACK_STAGE_RECEIVED,
            true,
            Duration::from_millis(30),
        );
        assert_eq!(outcome, SyncWaitOutcome::Timeout);
        assert_eq!(wm.sync_timeouts_total(), 1);
        assert_eq!(wm.waiter_count(), 0, "timed-out entry must be removed");
        // 摘除后迟到的 ACK 不得唤醒已返回的调用方（无 entry 可投）。
        // EN: A late ACK after removal must not wake an already-returned caller (no
        // entry left to deliver to).
        wm.on_ack(proto::ACK_STAGE_RECEIVED, 100);
        assert_eq!(wm.waiter_count(), 0);
    }

    #[test]
    fn sync_wait_wakes_on_ack() {
        let wm = std::sync::Arc::new(AckWatermarks::new());
        let wm2 = std::sync::Arc::clone(&wm);
        let t = std::thread::spawn(move || {
            wm2.wait(7, proto::ACK_STAGE_DURABLE, true, Duration::from_secs(5))
        });
        // 等 waiter 注册后再推水位线（deadline 轮询消调度竞争）。
        // EN: Push the watermark only after the waiter registers (deadline polling
        // removes scheduling races).
        wait_registered(&wm, 1);
        wm.on_ack(proto::ACK_STAGE_DURABLE, 7);
        assert_eq!(t.join().unwrap(), SyncWaitOutcome::Satisfied);
        assert_eq!(wm.waiter_count(), 0);
        assert_eq!(wm.sync_timeouts_total(), 0);
    }

    #[test]
    fn sync_wait_group_commit_wakes_all_covered_waiters() {
        let wm = std::sync::Arc::new(AckWatermarks::new());
        let mut handles = Vec::new();
        // 三个 durable waiter（lsn 3/5/9）+ 一个 received waiter（lsn 3）——
        // durable ACK 到 8 应唤醒前两个，其余不受本 stage 影响。
        // EN: Three durable waiters (lsn 3/5/9) + one received waiter (lsn 3) — a
        // durable ACK to 8 should wake the first two; the rest are unaffected by
        // this stage.
        for lsn in [3u64, 5, 9] {
            let wm2 = std::sync::Arc::clone(&wm);
            handles.push(std::thread::spawn(move || {
                wm2.wait(
                    lsn,
                    proto::ACK_STAGE_DURABLE,
                    true,
                    Duration::from_millis(300),
                )
            }));
        }
        let wm2 = std::sync::Arc::clone(&wm);
        handles.push(std::thread::spawn(move || {
            wm2.wait(
                3,
                proto::ACK_STAGE_RECEIVED,
                true,
                Duration::from_millis(300),
            )
        }));
        wait_registered(&wm, 4);
        wm.on_ack(proto::ACK_STAGE_DURABLE, 8);
        let outcomes: Vec<SyncWaitOutcome> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(outcomes[0], SyncWaitOutcome::Satisfied, "lsn 3 covered");
        assert_eq!(outcomes[1], SyncWaitOutcome::Satisfied, "lsn 5 covered");
        assert_eq!(outcomes[2], SyncWaitOutcome::Timeout, "lsn 9 not covered");
        assert_eq!(
            outcomes[3],
            SyncWaitOutcome::Timeout,
            "received waiter must not wake on durable ack"
        );
    }

    #[test]
    fn sync_wait_lsn_zero_is_not_vacuously_satisfied() {
        // 原点二义性回归：LSN 从 0 开始，初始水位线 0（=从未收过 ACK）不得
        // 虚满足 lsn=0 的等待——否则首条记录的 sync 语义形同虚设。
        // EN: With LSNs starting at 0, an untouched watermark of 0 must not
        // satisfy wait(0); only a real ACK may.
        let wm = AckWatermarks::new();
        let outcome = wm.wait(
            0,
            proto::ACK_STAGE_DURABLE,
            false,
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::NoFollower);
        // 真 ACK（确认的就是 lsn 0）之后，wait(0) 才合法满足。
        // EN: After a real ACK (confirming lsn 0), wait(0) is legitimately satisfied.
        wm.on_ack(proto::ACK_STAGE_DURABLE, 0);
        let outcome = wm.wait(
            0,
            proto::ACK_STAGE_DURABLE,
            false,
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::Satisfied);
    }

    #[test]
    fn received_ack_must_not_satisfy_durable_lsn_zero() {
        // OCR f942e45[1] 回归：单全局「收过 ACK」标志会让一条 RECEIVED ACK
        // 虚满足 DURABLE 的 lsn=0 等待——false durability，主在此时崩溃即
        // 丢数据。按阶段独立的 acked 标志钉死跨阶段泄漏。
        // EN: A RECEIVED-only ack must never satisfy a DURABLE wait, even at
        // the ambiguous lsn 0 origin.
        let wm = AckWatermarks::new();
        wm.on_ack(proto::ACK_STAGE_RECEIVED, 5);
        // DURABLE 视角：从未收过 DURABLE ACK，lsn 0 也不得满足。
        // EN: DURABLE view: no DURABLE ACK has ever arrived, so lsn 0 must not be
        // satisfied either.
        let outcome = wm.wait(
            0,
            proto::ACK_STAGE_DURABLE,
            false,
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::NoFollower);
        // RECEIVED 视角：真实满足（水位线 5 ≥ 0 且本阶段收过 ACK）。
        // EN: RECEIVED view: genuinely satisfied (watermark 5 >= 0 and this stage has
        // seen an ACK).
        let outcome = wm.wait(
            0,
            proto::ACK_STAGE_RECEIVED,
            false,
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::Satisfied);
    }

    #[test]
    fn sync_wait_recheck_closes_missed_wakeup_window() {
        // 水位线在「检查后、注册前」推进：注册后复检必须命中快路径。
        // 直接构造终态验证不变量（竞态本身不可确定性复现）。
        // EN: The watermark advances between "check" and "register": the
        // post-register recheck must hit the fast path. Constructing the final state
        // directly verifies the invariant (the race is not deterministically
        // reproducible).
        let wm = AckWatermarks::new();
        wm.on_ack(proto::ACK_STAGE_RECEIVED, 1);
        let outcome = wm.wait(
            1,
            proto::ACK_STAGE_RECEIVED,
            true,
            Duration::from_millis(10),
        );
        assert_eq!(outcome, SyncWaitOutcome::Satisfied);
        assert_eq!(wm.waiter_count(), 0);
    }
}

// ── 唤醒路径的跨阶段隔离（白盒，默认构建即跑）─────────────────────
/// Wake-path stage isolation — white-box, runs in the default build.
///
/// 为什么白盒注册 entry（WHY）：走 `wait()` 注册需要另一线程真正阻塞，而
/// shuttle 的 `recv_timeout` 忽略超时（等不到「不得被唤醒」的观测点），真实线程
/// 又会把用例变成时序相关。直接把 entry 放进注册段再调 `on_ack`，断言就精确落在
/// `on_ack` 里的 `w.stage == stage` 比较上——该比较若回归，DURABLE waiter 会被
/// RECEIVED ACK 唤醒，客户端在数据尚未持久化前就收到成功（false durability）。
/// 注意：`shuttle_received_ack_never_satisfies_durable_wait` 传的是
/// `has_followers=false`，走早退路径、waiter 从未注册，覆盖不到这个比较。
/// EN: Registering the entry white-box puts the assertion exactly on the
/// `w.stage == stage` comparison in `on_ack`; a real thread would need a timeout
/// (ignored by shuttle) or become timing-dependent. The existing shuttle test
/// passes `has_followers=false` and therefore never registers a waiter.
#[cfg(all(test, not(loom), not(feature = "shuttle")))]
mod wake_path_stage_isolation_tests {
    use super::*;

    #[test]
    fn received_ack_does_not_wake_registered_durable_waiter() {
        let wm = AckWatermarks::new();
        let (tx, rx) = sc::channel::<()>();
        let id = wm.next_waiter_id.fetch_add(1, Ordering::Relaxed);
        let lsn = 5u64;
        let seg = &wm.waiters[(lsn as usize) & (WAITER_SEGMENTS - 1)];
        seg.lock().unwrap().push(WaiterEntry {
            id,
            lsn,
            stage: proto::ACK_STAGE_DURABLE,
            tx,
        });
        assert_eq!(wm.waiter_count(), 1);

        wm.on_ack(proto::ACK_STAGE_RECEIVED, lsn);
        assert_eq!(wm.waiter_count(), 1, "RECEIVED ACK 不得摘除 DURABLE waiter");
        assert!(
            rx.try_recv().is_err(),
            "RECEIVED ACK 不得唤醒 DURABLE waiter（false durability）"
        );

        // 同 stage 的 ACK 必须唤醒并摘除，确认上面不是「什么都没干」的空断言。
        // EN: Same-stage ACKs must wake and remove the waiter, proving the assertion
        // above is not vacuous.
        wm.on_ack(proto::ACK_STAGE_DURABLE, lsn);
        assert!(rx.try_recv().is_ok(), "DURABLE ACK 必须唤醒 waiter");
        assert_eq!(wm.waiter_count(), 0, "唤醒后不得遗留注册项");
    }
}

// ── P4.5.2 shuttle 确定性调度测试 ───────────────────────────────
/// 用 shuttle 的随机/PCT 调度穷举 `ShipHub` 与 `AckWatermarks` 的交错。
/// Shuttle-based interleaving search for the fan-out hub and the ack watermarks.
///
/// 为什么需要 / Why: 这两处的缺陷形态都是**交错依赖**的——丢唤醒、注册与扇出
/// 竞态、踢除与注销竞态、计数漂移。既有单测用「等注册 → 推水位线」的 deadline
/// 轮询只覆盖调度器恰好给出的那一两种交错；shuttle 在**同一份生产代码**上穷举
/// 交错，失败时打印可复现的调度串（`replay` 可逐条重放）。
///
/// 运行 / Run:
/// ```text
/// cargo test -p houyi-persistence --features shuttle --lib shuttle_
/// ```
/// 注意**必须**带 `shuttle_` 名字过滤：本 crate 其余单测用 std 原语，与本 feature
/// 换型后的类型同进程混跑会在运行期 panic（换型边界见文件头 `mod sc`）。
///
/// 已知边界 / Known limits:
/// - shuttle 的 `recv_timeout` 忽略超时（上游 TODO），故 `wait()` 的 Timeout 分支
///   在本模块内不可达；反向收益是「真丢唤醒」会让 waiter 永久阻塞，shuttle 以
///   **deadlock 诊断**报错——这正是我们要找的信号，且比超时降级更早暴露。
/// - 无 `shuttle::time`（`Instant` 走真实墙钟），故只断言**行为**，不断言耗时。
/// - 原子一律按 SeqCst 建模：弱内存序缺陷由同文件的 `loom_tests` 覆盖。
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    use super::*;
    use shuttle::thread;

    /// 扇出完整性：两个已注册且未断开的 follower 必须各收到**全部**批，且顺序
    /// 与发布顺序一致（队列 8 ≥ 3，不触发踢除）。丢批 = 复制静默失联。
    /// EN: Fan-out completeness: two registered, connected followers must each
    /// receive **all** batches in publish order (queue 8 >= 3, no kicks). A lost
    /// batch means replication silently lost contact.
    #[test]
    fn shuttle_hub_fanout_delivers_every_batch_in_order() {
        shuttle::check_random(
            || {
                let hub = Arc::new(ShipHub::new(8));
                let (_id1, rx1) = hub.register();
                let (_id2, rx2) = hub.register();
                let drain = |rx: ShipReceiver| {
                    thread::spawn(move || {
                        (0..3).map(|_| rx.recv().unwrap().lsn).collect::<Vec<_>>()
                    })
                };
                let r1 = drain(rx1);
                let r2 = drain(rx2);
                let h = Arc::clone(&hub);
                let pubr = thread::spawn(move || {
                    for i in 0..3u64 {
                        h.publish(i, &[i as u8]);
                    }
                });
                pubr.join().unwrap();
                assert_eq!(r1.join().unwrap(), vec![0, 1, 2]);
                assert_eq!(r2.join().unwrap(), vec![0, 1, 2]);
                assert_eq!(hub.slow_follower_kicks_total(), 0, "队列充裕时不得踢除");
                assert_eq!(hub.shipped_batches_total(), 3);
                assert_eq!(hub.shipped_bytes_total(), 3);
                assert_eq!(hub.follower_count(), 2);
            },
            200,
        );
    }

    /// 断开竞态：一个 follower 的接收端在随机时刻被丢弃，另一个必须**不受影响**
    /// 地收全所有批；断开的那个至多被踢一次（`kicks` 不得重复计数），且踢除后
    /// 不再出现在册（`follower_count` 结算为 1）。
    /// EN: Disconnect race: one follower's receiver is dropped at a random moment;
    /// the other must still receive **all** batches unaffected; the disconnected
    /// one is kicked at most once (`kicks` must not double-count) and no longer
    /// appears registered (`follower_count` settles at 1).
    #[test]
    fn shuttle_hub_disconnect_kicks_once_without_harming_others() {
        shuttle::check_random(
            || {
                let hub = Arc::new(ShipHub::new(8));
                let (_keep, rx_keep) = hub.register();
                let (_drop, rx_drop) = hub.register();
                let dropper = thread::spawn(move || drop(rx_drop));
                let h = Arc::clone(&hub);
                let pubr = thread::spawn(move || {
                    for i in 0..3u64 {
                        h.publish(i, b"x");
                    }
                });
                pubr.join().unwrap();
                dropper.join().unwrap();
                // 强制结算：断开者若仍在册，本批必踢。
                // EN: Force settlement: if the disconnected one is still registered,
                // this batch must kick it.
                hub.publish(3, b"y");
                assert_eq!(hub.follower_count(), 1, "断开者必须已出册");
                assert_eq!(hub.slow_follower_kicks_total(), 1, "同一 session 只计一次");
                let got: Vec<u64> = (0..4).map(|_| rx_keep.recv().unwrap().lsn).collect();
                assert_eq!(got, vec![0, 1, 2, 3], "未断开的 follower 不得丢批");
                assert_eq!(hub.shipped_batches_total(), 4);
            },
            200,
        );
    }

    /// 丢唤醒穷举（核心）：waiter 注册 → 复检 → 阻塞，与 ACK 线程的
    /// 「fetch_max → 扫段唤醒」任意交错。任一交错丢掉唤醒 → waiter 永久阻塞 →
    /// shuttle 报 deadlock。
    /// EN: Missed-wakeup exhaustion (core): waiter registration → recheck → block,
    /// interleaved arbitrarily with the ACK thread's "fetch_max → scan segments and
    /// wake". Any interleaving that loses a wakeup → the waiter blocks forever →
    /// shuttle reports a deadlock.
    #[test]
    fn shuttle_sync_wait_is_never_lost() {
        shuttle::check_random(
            || {
                let wm = Arc::new(AckWatermarks::new());
                let acker = {
                    let wm = Arc::clone(&wm);
                    thread::spawn(move || wm.on_ack(proto::ACK_STAGE_DURABLE, 1))
                };
                let waiter = {
                    let wm = Arc::clone(&wm);
                    thread::spawn(move || {
                        wm.wait(1, proto::ACK_STAGE_DURABLE, true, Duration::from_secs(3600))
                    })
                };
                assert_eq!(waiter.join().unwrap(), SyncWaitOutcome::Satisfied);
                acker.join().unwrap();
                assert_eq!(wm.waiter_count(), 0, "等待结束不得遗留注册项");
                assert_eq!(wm.sync_timeouts_total(), 0);
                assert_eq!(wm.sync_waits_total(), 1);
            },
            500,
        );
    }

    /// group commit：一次 DURABLE ACK 覆盖多个 waiter，必须**全部**唤醒
    /// （`on_ack` 扫全部 16 段），且各 waiter 的注册交错不影响覆盖率。
    /// EN: Group commit: one DURABLE ACK covering multiple waiters must wake **all**
    /// of them (`on_ack` scans all 16 segments), and interleaved registrations must
    /// not affect coverage.
    #[test]
    fn shuttle_group_commit_wakes_all_covered_waiters() {
        shuttle::check_random(
            || {
                let wm = Arc::new(AckWatermarks::new());
                let waiters: Vec<_> = [1u64, 2, 3]
                    .into_iter()
                    .map(|lsn| {
                        let wm = Arc::clone(&wm);
                        thread::spawn(move || {
                            wm.wait(
                                lsn,
                                proto::ACK_STAGE_DURABLE,
                                true,
                                Duration::from_secs(3600),
                            )
                        })
                    })
                    .collect();
                let acker = {
                    let wm = Arc::clone(&wm);
                    thread::spawn(move || wm.on_ack(proto::ACK_STAGE_DURABLE, 3))
                };
                let outcomes: Vec<SyncWaitOutcome> =
                    waiters.into_iter().map(|h| h.join().unwrap()).collect();
                acker.join().unwrap();
                assert!(
                    outcomes.iter().all(|o| *o == SyncWaitOutcome::Satisfied),
                    "水位线覆盖的 waiter 必须全部唤醒，实得 {outcomes:?}"
                );
                assert_eq!(wm.waiter_count(), 0);
                assert_eq!(wm.sync_timeouts_total(), 0);
            },
            300,
        );
    }

    /// 水位线单调 + 跨阶段隔离：并发 ACK 的乱序到达不得让水位线倒退，DURABLE
    /// ACK 不得推进 RECEIVED 水位（二者是独立阶段）。
    /// EN: Watermark monotonicity + cross-stage isolation: out-of-order concurrent
    /// ACKs must not regress the watermark, and a DURABLE ACK must not advance the
    /// RECEIVED watermark (they are independent stages).
    #[test]
    fn shuttle_watermarks_monotonic_and_stage_isolated() {
        shuttle::check_random(
            || {
                let wm = Arc::new(AckWatermarks::new());
                let handles: Vec<_> = [5u64, 3, 9, 1]
                    .into_iter()
                    .map(|lsn| {
                        let wm = Arc::clone(&wm);
                        thread::spawn(move || wm.on_ack(proto::ACK_STAGE_DURABLE, lsn))
                    })
                    .collect();
                for h in handles {
                    h.join().unwrap();
                }
                assert_eq!(
                    wm.durable_lsn.load(Ordering::Relaxed),
                    9,
                    "水位线取 max，乱序到达不得倒退"
                );
                assert_eq!(
                    wm.recv_lsn.load(Ordering::Relaxed),
                    0,
                    "DURABLE ACK 不得推进 RECEIVED 水位"
                );
                assert!(wm.covers(proto::ACK_STAGE_DURABLE, 9));
                assert!(!wm.covers(proto::ACK_STAGE_RECEIVED, 0), "跨阶段虚满足");
            },
            200,
        );
    }

    /// 原点二义性在交错下的复检：RECEIVED ACK 与 DURABLE 等待任意交错，
    /// DURABLE 视角必须落 NoFollower（无 DURABLE ACK ⇒ 不得虚满足 lsn 0）。
    /// 用 `has_followers=false` 走立即降级路径，避免在「故意不满足」场景下阻塞。
    /// EN: Origin ambiguity under interleaving: with RECEIVED ACKs and DURABLE waits
    /// in any order, the DURABLE view must land on NoFollower (no DURABLE ACK ⇒
    /// never falsely satisfy lsn 0). With `has_followers=false` it takes the
    /// immediate-degrade path, avoiding blocking in a deliberately unsatisfied case.
    #[test]
    fn shuttle_received_ack_never_satisfies_durable_wait() {
        shuttle::check_random(
            || {
                let wm = Arc::new(AckWatermarks::new());
                let recv_acker = {
                    let wm = Arc::clone(&wm);
                    thread::spawn(move || wm.on_ack(proto::ACK_STAGE_RECEIVED, 0))
                };
                let outcome = wm.wait(
                    0,
                    proto::ACK_STAGE_DURABLE,
                    false,
                    Duration::from_secs(3600),
                );
                recv_acker.join().unwrap();
                assert_eq!(
                    outcome,
                    SyncWaitOutcome::NoFollower,
                    "RECEIVED ACK 不得虚满足 DURABLE 的 lsn 0（false durability）"
                );
                assert_eq!(wm.sync_no_follower_total(), 1);
                assert_eq!(wm.sync_timeouts_total(), 0);
            },
            300,
        );
    }
}

// ── P4.5.2 loom 穷举测试 ───────────────────────────────────────
/// 用 loom 穷举 2 线程的**内存序**交错。
/// Exhaustive memory-ordering exploration of the 2-thread wake-up protocol.
///
/// 为什么 shuttle 之外还要 loom / Why loom on top of shuttle: shuttle 把所有原子
/// 一律按 **SeqCst** 建模（官方明确「不忠实建模弱内存序」，建议此类场景改用 loom），
/// 而 `AckWatermarks` 的唤醒协议全部用 `Relaxed`。本模块在 loom 的弱内存序模型下
/// 穷举「waiter 注册 → 复检 → 阻塞」与「`on_ack`: fetch_max → 置标志 → 扫段」的
/// 全部交错：任一交错让 waiter 永久阻塞，loom 即报 deadlock 并给出反例。
///
/// 运行 / Run:
/// ```text
/// RUSTFLAGS="--cfg loom" cargo test -p houyi-persistence --lib loom_
/// ```
///
/// 边界 / Boundary: loom 的 mpsc 无 `recv_timeout`（`sc::recv_timeout` 在 loom 臂
/// 退回 `recv`），故**不覆盖超时**——靶区本就是无超时的唤醒协议，超时语义由
/// shuttle / std 单测覆盖。`ShipHub` 不参与 loom 模型（其有界通道在 loom 臂退回
/// std），队列语义由 shuttle 覆盖。
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    /// 丢唤醒穷举：已注册的 waiter 必须被 ACK 唤醒，且不得遗留注册项。
    /// 若存在反例，`wait` 阻塞在无超时的 `recv` 上 → loom 报 deadlock。
    /// EN: Missed-wakeup exhaustion: a registered waiter must be woken by an ACK and
    /// no registration entry may be left behind. A counterexample leaves `wait`
    /// blocked on a timeout-less `recv` → loom reports a deadlock.
    #[test]
    fn loom_no_lost_wakeup_for_registered_waiter() {
        loom::model(|| {
            let wm = Arc::new(AckWatermarks::new());
            let acker = {
                let wm = Arc::clone(&wm);
                loom::thread::spawn(move || wm.on_ack(proto::ACK_STAGE_DURABLE, 7))
            };
            let outcome = wm.wait(7, proto::ACK_STAGE_DURABLE, true, Duration::from_secs(3600));
            assert_eq!(outcome, SyncWaitOutcome::Satisfied);
            acker.join().unwrap();
            assert_eq!(wm.waiter_count(), 0, "等待结束不得遗留注册项");
            assert_eq!(wm.sync_timeouts_total(), 0);
        });
    }

    /// 跨阶段隔离的内存序穷举：RECEIVED ACK 与 DURABLE 查询任意交错，
    /// `covers(DURABLE, 0)` 不得因 RECEIVED 的 acked 标志而虚真（false durability）。
    /// EN: Memory-ordering exhaustion for cross-stage isolation: with RECEIVED ACKs
    /// and DURABLE queries in any order, `covers(DURABLE, 0)` must not become
    /// vacuously true because of RECEIVED's acked flag (false durability).
    #[test]
    fn loom_received_ack_never_covers_durable_origin() {
        loom::model(|| {
            let wm = Arc::new(AckWatermarks::new());
            let acker = {
                let wm = Arc::clone(&wm);
                loom::thread::spawn(move || wm.on_ack(proto::ACK_STAGE_RECEIVED, 9))
            };
            assert!(
                !wm.covers(proto::ACK_STAGE_DURABLE, 0),
                "RECEIVED ACK 不得让 DURABLE 的 lsn 0 虚满足"
            );
            acker.join().unwrap();
            assert!(wm.covers(proto::ACK_STAGE_RECEIVED, 9));
            assert!(!wm.covers(proto::ACK_STAGE_DURABLE, 0));
        });
    }
}
