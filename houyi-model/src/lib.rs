//! 复制 / 手动 promote 协议的模型检查（P4.5.6，Stateright）。
//! Model checking for the replication + manual-promote protocol (P4.5.6).
//!
//! 为什么单独建模（WHY）：端到端测试只能跑少量交错；协议级缺陷往往藏在
//! 「消息在途 + 角色翻转 + 重连」这类组合里。这里把 v1 协议抽象成两节点状态
//! 机，让模型检查器穷举全部交错，对安全性质给出**反例或证明**。
//! EN: End-to-end tests sample few interleavings; protocol bugs hide in
//! "in-flight messages + role flip + reconnect" combinations. This models the
//! v1 protocol as a two-node state machine so the checker exhausts every
//! interleaving and yields a counterexample or a proof per property.
//!
//! 协议镜像 / Protocol mirror（与 `replication_leader.rs` / `replication_follower.rs`
//! 逐条对应）：
//! - 记录以 LSN 标识；`日志` = 本节点持有的记录序列。记录内容抽象为
//!   **写入者 epoch**（同 LSN 不同 epoch = 内容分叉）。
//! - leader 写：`log.push(epoch)`；`next_lsn = log.len()`。
//! - 会话：注册后先 catch-up（从 follower 的 resume 处补文件），再 live 推送；
//!   follower 对 `lsn < expected` 幂等跳过、`lsn > expected` 断连重练。
//! - fsync+apply 后才推进 DURABLE 水位（RPO 的边界）。
//! - promote：follower 的 `wal.next_lsn()` 成为新 leader 的起点（会话退出前的
//!   兜底 drain 保证已追加记录全部 durable）。
//! - 重连：`resume > leader.next` → follower_ahead 拒绝；长度不超前时再比对
//!   **日志前缀指纹**（P4.6.1，协议 v4）：内容不一致 → 分叉拒绝（人工处置），
//!   一致才补后缀继续。
//!
//! 安全性质 / Safety properties:
//! 1. `never_diverged`：任何被接受的重连都不得让同 LSN 上存在两个不同 epoch
//!    的记录（静默分叉）。P4.6.1 之后该性质在整个状态空间成立（指纹拒绝）。
//! 2. `durable_contiguous`：follower 的 durable 与本地日志连续（无洞）。
//! 3. `rpo_after_promote`：promote 后新 leader 的日志必须包含全部已 DURABLE
//!    ACK 的记录（full_sync 的 RPO=0）。
//! 4. `ahead_rejected`：resume 超前时不得应用任何 leader 记录。

use std::collections::BTreeSet;

use stateright::{Model, Property};

/// 日志上限：模型只需暴露协议缺陷，3~4 条记录足够覆盖「翻转前后各写」。
/// EN: Log cap: the model only needs to expose protocol defects; 3-4 records cover writes around the flip.
const MAX_LOG: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Leader,
    Follower,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct State {
    /// 节点 0 = 原 leader，节点 1 = 原 follower。
    /// EN: Node 0 = original leader, node 1 = original follower.
    pub role: [Role; 2],
    /// 各节点日志；元素是写入者 epoch（0 = 原 leader，1 = promote 后的新 leader）。
    /// EN: Per-node logs; elements are writer epochs (0 = original leader, 1 = new leader after promote).
    pub log: [Vec<u8>; 2],
    /// follower 已 fsync+apply 的最大 LSN+1（0 表示无）。
    /// EN: Max LSN+1 fsynced and applied by the follower (0 means none).
    pub durable: [usize; 2],
    /// 已发货但未送达的记录（LSN + 记录内容）。
    /// EN: Records shipped but not delivered (LSN + record content).
    pub inflight: BTreeSet<(usize, u8)>,
    /// 当前 follower 是否在线（已握手完成）。
    /// EN: Whether the follower is online (handshake completed).
    pub connected: bool,
    /// leader 收到的最大 DURABLE ACK（LSN+1）。
    /// EN: Max DURABLE ACK received by the leader (LSN+1).
    pub acked: usize,
    /// 是否已发生 promote。
    /// EN: Whether a promote has occurred.
    pub promoted: bool,
    /// 是否发生过 follower_ahead 拒绝（观测用）。
    /// EN: Whether a follower_ahead rejection occurred (for observation).
    pub ahead_rejected: bool,
    /// 是否发生过**分叉拒绝**（P4.6.1：同长度不同内容被握手拒绝）。
    /// EN: Whether a **divergence rejection** occurred (P4.6.1: same length, different content).
    pub diverged_rejected: bool,
    /// 被接受的重连是否造成同 LSN 不同内容（分叉）。P4.6.1 之后该状态不可达。
    /// EN: Whether an accepted reconnect caused divergence at the same LSN. Unreachable after P4.6.1.
    pub diverged: bool,
}

impl State {
    fn leader(&self) -> Option<usize> {
        // 若同时存在两个 leader（promote 后旧主未处置），以「当前被建模的
        // 活跃链路」为准：节点 1 promote 后，节点 0 的写不参与复制。
        // EN: If two leaders coexist (old leader not handled after promote), use the
        // EN: currently modeled active link: after node 1 promotes, node 0's writes do not replicate.
        match (self.role[0], self.role[1]) {
            (Role::Leader, Role::Follower) => Some(0),
            (_, Role::Leader) => Some(1),
            _ => None,
        }
    }

    fn follower(&self) -> Option<usize> {
        match (self.role[0], self.role[1]) {
            (Role::Leader, Role::Follower) => Some(1),
            (Role::Follower, Role::Leader) => Some(0),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// 节点 i 发起握手/重连（带 resume = 本地日志长度）。
    /// EN: Node i initiates handshake/reconnect (with resume = local log length).
    Connect(usize),
    /// 节点 i 作为 leader 追加一条记录。
    /// EN: Node i, as leader, appends one record.
    Write(usize),
    /// leader 把 LSN l 推给在线 follower。
    /// EN: Leader ships LSN l to the online follower.
    Ship(usize),
    /// follower 收到 LSN l。
    /// EN: Follower receives LSN l.
    Deliver(usize),
    /// follower fsync + apply 到当前已接收位置（推进 durable）。
    /// EN: Follower fsync+applies up to the currently received position (advances durable).
    Fsync,
    /// follower 提升为 leader（手动 failover）。
    /// EN: Follower is promoted to leader (manual failover).
    Promote(usize),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReplicationModel;

impl ReplicationModel {
    /// 握手：resume = 节点日志长度。
    ///
    /// 与实现同口径（P4.6.1 起是 v4/v5 的握手日志指纹）：`resume > leader
    /// 日志长度` → follower_ahead 拒绝；否则对 `[0, resume)` 做**内容比较**，
    /// 不一致即 `diverged_rejected` 并拒绝重连（不补后缀、不采纳）——
    /// 与旧模型「直接接受并假定前缀相同」的待查点相对应，该缺口已修复。
    /// EN: matches the v4/v5 fingerprint handshake — equal length is no longer
    /// sufficient; a differing prefix is rejected (the old model's open point).
    fn connect(state: &State, node: usize) -> State {
        let mut next = state.clone();
        let Some(leader) = state.leader() else {
            return next;
        };
        if node == leader || state.connected {
            return next;
        }
        let resume = state.log[node].len();
        let leader_len = state.log[leader].len();
        if resume > leader_len {
            next.ahead_rejected = true;
            return next;
        }
        // 前缀内容必须一致（P4.6.1 握手日志指纹）：不一致即拒绝重连，
        // 与 follower_ahead 同属人工处置信号——不得补后缀、不得采纳。
        // EN: prefix content must match (the v4 fingerprint); a mismatch is
        // rejected like follower_ahead — no backfill, no adoption.
        if state.log[node][..resume] != state.log[leader][..resume] {
            next.diverged_rejected = true;
            return next;
        }
        // catch-up：只补后缀 [resume, leader_len)——与实现一致（前缀保留，
        // 是否一致由上面的比较负责记录）。重连会丢弃旧会话的在途消息。
        // EN: backfill only the suffix, matching the implementation; the old
        // session's in-flight messages are dropped by the reconnect.
        next.log[node].truncate(resume);
        next.log[node].extend_from_slice(&state.log[leader][resume..leader_len]);
        next.inflight.clear();
        next.connected = true;
        next
    }
}

impl Model for ReplicationModel {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State {
            role: [Role::Leader, Role::Follower],
            log: [Vec::new(), Vec::new()],
            durable: [0, 0],
            inflight: BTreeSet::new(),
            connected: false,
            acked: 0,
            promoted: false,
            ahead_rejected: false,
            diverged_rejected: false,
            diverged: false,
        }]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Self::Action>) {
        for node in 0..2 {
            if state.role[node] == Role::Leader && state.log[node].len() < MAX_LOG {
                actions.push(Action::Write(node));
            }
            if state.role[node] == Role::Follower {
                actions.push(Action::Promote(node));
            }
            // 任何节点都可以发起重连（旧主 rejoin / follower 重连）。
            // EN: Any node can initiate a reconnect (old leader rejoin / follower reconnect).
            actions.push(Action::Connect(node));
        }
        if let Some(f) = state.follower() {
            if state.connected {
                for l in 0..state.log[state.leader().unwrap()].len() {
                    if !state.inflight.iter().any(|(lsn, _)| *lsn == l) {
                        actions.push(Action::Ship(l));
                    }
                }
                for (l, _) in state.inflight.iter() {
                    actions.push(Action::Deliver(*l));
                }
                if state.durable[f] < state.log[f].len() {
                    actions.push(Action::Fsync);
                }
            }
        }
    }

    fn next_state(&self, last: &State, action: Self::Action) -> Option<Self::State> {
        let mut s = last.clone();
        match action {
            Action::Write(node) => {
                if s.role[node] != Role::Leader || s.log[node].len() >= MAX_LOG {
                    return None;
                }
                let epoch = if s.promoted && node == 1 { 1 } else { 0 };
                s.log[node].push(epoch);
            }
            Action::Ship(l) => {
                let leader = s.leader()?;
                if !s.connected || l >= s.log[leader].len() {
                    return None;
                }
                let rec = s.log[leader][l];
                if !s.inflight.insert((l, rec)) {
                    return None;
                }
            }
            Action::Deliver(l) => {
                let f = s.follower()?;
                let rec = s
                    .inflight
                    .iter()
                    .find(|(lsn, _)| *lsn == l)
                    .map(|(_, r)| *r)?;
                s.inflight.remove(&(l, rec));
                let expected = s.log[f].len();
                if l < expected {
                    // 重叠推送：幂等跳过。实现不校验内容——若本地同一 LSN 内容
                    // 不同，这里就是静默分叉的第二入口。
                    // EN: Overlapping ship: idempotent skip. The implementation does not verify
                    // EN: content — same-LSN differing local content is a second silent-divergence entry.
                    if s.log[f][l] != rec {
                        s.diverged = true;
                    }
                } else if l == expected {
                    s.log[f].push(rec);
                } else {
                    // 空洞：断连重练（实现同口径）。
                    // EN: Gap: disconnect and retry (same as the implementation).
                    s.connected = false;
                }
            }
            Action::Fsync => {
                let f = s.follower()?;
                s.durable[f] = s.log[f].len();
                // DURABLE ACK 回到 leader：水位只进不退。
                // EN: DURABLE ACK returns to the leader: the watermark only advances.
                s.acked = s.acked.max(s.durable[f]);
            }
            Action::Promote(node) => {
                if s.role[node] != Role::Follower || s.promoted {
                    return None;
                }
                // 会话退出前兜底 drain：已追加记录全部 fsync+apply。
                // EN: Fallback drain before session exit: all appended records are fsynced and applied.
                s.durable[node] = s.log[node].len();
                s.role[node] = Role::Leader;
                s.promoted = true;
                s.connected = false;
                s.inflight.clear();
            }
            Action::Connect(node) => {
                s = ReplicationModel::connect(&s, node);
                if s == *last {
                    return None;
                }
            }
        }
        Some(s)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("never_diverged", |_, s: &State| !s.diverged),
            Property::always("durable_contiguous", |_, s: &State| {
                (0..2).all(|i| s.durable[i] <= s.log[i].len())
            }),
            Property::always("rpo_after_promote", |_, s: &State| {
                if !s.promoted {
                    return true;
                }
                // 已 DURABLE ACK 的记录必须在新 leader 的日志里（acked 是
                // 记录数，日志长度必须覆盖它）。promoted 状态下 leader 必须
                // 存在——缺失是建模回归，直接 panic 暴露（不静默兜底）。
                // EN: no silent fallback — a missing leader on a promoted state
                // is a modeling regression and should fail loudly.
                let leader = s.leader().expect("promoted state must have a leader");
                s.log[leader].len() >= s.acked
            }),
            // 「ahead 拒绝可达」本身是可观测的活性/可达性断言：协议必须
            // 能识别 resume 超前（否则脑裂信号静默失效）。
            // EN: reachability of the ahead-rejection signal.
            Property::sometimes("ahead_rejection_reachable", |_, s: &State| s.ahead_rejected),
            // P4.6.1：分叉拒绝必须可达（同长度不同内容的旧主重连要被拦住）。
            // EN: P4.6.1: divergence rejection must be reachable (same length, different content).
            Property::sometimes("diverged_rejection_reachable", |_, s: &State| {
                s.diverged_rejected
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::Checker;

    /// 模型检查结论（P4.6.1 之后）：
    ///
    /// - `never_diverged` **无反例**：握手比对日志指纹后，同长度不同内容的
    ///   重连被拒绝（P4.5.6 的反例路径已封闭）。
    /// - `durable_contiguous` / `rpo_after_promote` 无反例。
    /// - `ahead_rejection_reachable` / `diverged_rejection_reachable` 存在示例。
    ///
    /// EN: after P4.6.1 the fingerprint rejects same-length divergent reconnects;
    /// `never_diverged` now holds over the whole state space.
    #[test]
    fn model_check_outcome_is_recorded() {
        let checker = ReplicationModel.checker().spawn_dfs().join();
        assert!(
            checker.is_done(),
            "model checking must complete (state space exhausted)"
        );

        for prop in ["never_diverged", "durable_contiguous", "rpo_after_promote"] {
            let found = checker.discovery(prop);
            assert!(
                found.is_none(),
                "unexpected counterexample for property {prop}: {found:?}"
            );
        }
        for prop in ["ahead_rejection_reachable", "diverged_rejection_reachable"] {
            assert!(
                checker.discovery(prop).is_some(),
                "expected a reachability example for {prop}"
            );
        }
    }

    /// 分叉拒绝的示例路径必须包含 promote 与旧主在 promote 后的写——
    /// 钉死「等长日志重连」这一反例形态，防止模型退化后仍「碰巧」可达。
    /// EN: The divergence-rejection example must include promote and an old-leader write after it —
    /// EN: pinning the same-length reconnect counterexample so model decay cannot pass by accident.
    #[test]
    fn diverged_rejection_counterexample_shape() {
        let checker = ReplicationModel.checker().spawn_dfs().join();
        let path = checker
            .discovery("diverged_rejection_reachable")
            .expect("divergence-rejection example missing");

        // 分叉有两种同构形态：旧主在 promote 后写（其未复制的记录与新主
        // 重用同一 LSN），或新主在 promote 后写（与旧主的未复制记录撞号）。
        // 共同点是：恰好一次 promote + 之后发生重连。
        // EN: divergence can arise from either a post-promote write on the old
        // leader or a post-promote write on the new one reusing an LSN; the
        // invariant is one promote followed by a reconnect attempt.
        let mut promotes = 0usize;
        let mut writes_after_promote = 0usize;
        let mut reconnect_after_promote = 0usize;
        for (_state, action) in path.into_vec() {
            let Some(action) = action else { continue };
            match action {
                Action::Promote(_) => promotes += 1,
                Action::Write(_) if promotes > 0 => writes_after_promote += 1,
                Action::Connect(_) if promotes > 0 => reconnect_after_promote += 1,
                _ => {}
            }
        }
        assert_eq!(promotes, 1, "example must involve exactly one promote");
        assert!(
            writes_after_promote >= 1,
            "example must include a post-promote write"
        );
        assert!(
            reconnect_after_promote >= 1,
            "example must include a reconnect attempt after promote"
        );
    }
}
