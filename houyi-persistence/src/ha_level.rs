//! HA 级别：collection 粒度的复制持久性语义（P4.4.2）。
//! HA level — per-collection replication durability semantics (P4.4.2).
//!
//! 为什么放 persistence 而不是 server：collections.wal 的编解码（本 crate）与
//! server 的 Collection/配置装配都要引用同一套取值与字符串表——两处各写一份
//! 字符串表必然分叉（v1 骨架 LE/BE 分裂的同类教训）。
//! EN: Lives in persistence because both the collections.wal codec (this crate)
//! and the server's Collection/config assembly reference the same values and
//! string table — two copies would inevitably drift.
//!
//! 为什么是枚举而不是 u8 常量（P4.6.3）：旧 API 以裸 u8 码传遍 WAL 编解码、
//! 配置与热路径，任何写错码值的调用点都只能在运行期被 `ha_level_as_str`
//! 的 `None` 抓住（或者漏抓）。枚举让「非法级别」在类型层不可表达；u8 只
//! 保留在原子量存储（std 没有 AtomicHaLevel）与线格式这两个边界处，进出
//! 都必须经 [`HaLevel::code`] / [`HaLevel::from_code`]。
//! EN: A bare u8 code crossed codec/config/hot-path boundaries, leaving wrong
//! values representable until (if ever) caught at runtime. The enum makes
//! invalid levels unrepresentable; u8 survives only at the two boundaries —
//! atomic storage (no AtomicHaLevel in std) and the wire codec — through
//! `code()` / `from_code()`.
//!
//! 语义（v1，多 follower 任一 ACK 即满足）/ Semantics (v1: any single follower
//! ACK satisfies the wait):
//! - `async`：写入本地 durable 即返回，不等 follower（默认）。
//! - `semi_sync`：等任一 follower **收到**帧即 ACK（+1 RTT），不要求落盘。
//! - `full_sync`：等任一 follower **fsync + 进 HotBuffer 可查**后 ACK——与
//!   leader 自身「durable + 可检索」语义同级（不等异步 HNSW 建图）。
//!
//! 超时/无健康 follower 全局降级为 async + 指标告警（MySQL semi-sync 路线）。
//! EN: `async` returns once locally durable (default); `semi_sync` waits for
//! any follower's RECEIVED ack (+1 RTT); `full_sync` waits for any follower's
//! fsync + HotBuffer-visible ack. Timeouts or the absence of a healthy
//! follower degrade to async with metrics (MySQL semi-sync precedent).

/// collection 粒度的复制持久性级别。
/// Per-collection replication durability level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HaLevel {
    /// async：本地 durable 即返回（默认）。
    /// EN: async: return once locally durable (the default).
    Async = 0,
    /// semi_sync：等任一 follower 收帧 ACK。
    /// EN: semi_sync: wait for any follower's frame-received ACK.
    SemiSync = 1,
    /// full_sync：等任一 follower fsync + HotBuffer 可查 ACK。
    /// EN: full_sync: wait for any follower's fsync + HotBuffer-visible ACK.
    FullSync = 2,
}

impl HaLevel {
    /// 字符串 → 级别；trim + 大小写不敏感，连字符/下划线归一。
    /// 笔误返回 Err（配置通道 fast-fail，静默落 async 会让「以为有 HA」的部署
    /// 实际裸奔——与 role 解析同款纪律）。
    /// EN: Parse a level string; typos are fatal — silently landing on async
    /// would leave a deployment that believes it has HA actually unprotected.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
        match norm.as_str() {
            "async" => Ok(Self::Async),
            "semi_sync" => Ok(Self::SemiSync),
            "full_sync" => Ok(Self::FullSync),
            other => Err(format!(
                "invalid ha_level {other:?}: expect async | semi_sync | full_sync"
            )),
        }
    }

    /// 规范字符串（WAL 记录/启动日志/配置回显共用）。
    /// Canonical string shared by WAL records, startup logs and config echoes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Async => "async",
            Self::SemiSync => "semi_sync",
            Self::FullSync => "full_sync",
        }
    }

    /// 线格式 / 原子量存储码。仅允许出现在边界层。
    /// Wire/storage code; boundary layers only.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// 码 → 级别；未知码返回 None——调用侧按腐败处理（fail-fast 或告警），
    /// 不得静默落回 async。EN: unknown codes yield None; callers treat them as
    /// corruption instead of silently falling back to async.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Async),
            1 => Some(Self::SemiSync),
            2 => Some(Self::FullSync),
            _ => None,
        }
    }
}

impl std::fmt::Display for HaLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_all_levels() {
        for level in [HaLevel::Async, HaLevel::SemiSync, HaLevel::FullSync] {
            // 经规范字符串再解析仍是自身（roundtrip 闭环）。
            // EN: Reparsing the canonical string yields the same level (roundtrip).
            assert_eq!(HaLevel::parse(level.as_str()).unwrap(), level);
            // 码 roundtrip（线格式边界）。
            // EN: Code roundtrip (wire-format boundary).
            assert_eq!(HaLevel::from_code(level.code()), Some(level));
        }
    }

    #[test]
    fn parse_accepts_case_hyphen_whitespace_variants() {
        assert_eq!(HaLevel::parse(" Semi-Sync ").unwrap(), HaLevel::SemiSync);
        assert_eq!(HaLevel::parse("FULL_SYNC").unwrap(), HaLevel::FullSync);
        assert_eq!(HaLevel::parse("Async").unwrap(), HaLevel::Async);
    }

    #[test]
    fn parse_typo_is_fatal() {
        let err = HaLevel::parse("asyn").unwrap_err();
        assert!(err.contains("asyn"), "error must echo the bad value: {err}");
        assert!(HaLevel::parse("").is_err());
    }

    #[test]
    fn from_code_rejects_unknown_code() {
        assert_eq!(HaLevel::from_code(3), None);
        assert_eq!(HaLevel::from_code(9), None);
        assert_eq!(HaLevel::from_code(u8::MAX), None);
    }

    #[test]
    fn display_matches_canonical_string() {
        assert_eq!(HaLevel::FullSync.to_string(), "full_sync");
    }
}
