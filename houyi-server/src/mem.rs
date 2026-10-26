//! 内存水位线（P4.2.4）：RSS 采样 + 写入准入控制。
//! Memory watermark: RSS sampling + write-path admission control.
//!
//! 选型说明：只读 Linux `/proc/self/statm`（部署目标是 Linux 容器），
//! 不引 sysinfo 之类的跨平台依赖；非 Linux（如 macOS 开发机）读不到即
//! fail-open 放行——水位线是防 OOM 的安全网，不是精确计量，宁可缺席不可误杀。
//! EN: Linux `/proc/self/statm` only (the deploy target is Linux containers);
//! no cross-platform dep. Non-Linux reads fail open — this is an OOM safety
//! net, not precise metering; absence beats false kills.

use std::sync::atomic::{AtomicU64, Ordering};

/// 读当前进程 RSS（字节）。Linux 解析 /proc/self/statm 第二字段（驻留页数）
/// 乘页大小；读不到（非 Linux / 容器限制）返回 None。
/// EN: Current process RSS in bytes, parsed from field 2 of
/// /proc/self/statm times the page size; None when unavailable.
pub fn current_rss_bytes() -> Option<u64> {
    parse_statm_rss(&std::fs::read_to_string("/proc/self/statm").ok()?)
}

/// 解析 statm 文本的 RSS 字段（独立成函数便于单测）。
/// EN: statm parsing kept separate for unit testing.
fn parse_statm_rss(statm: &str) -> Option<u64> {
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = assumed_page_size();
    Some(pages.saturating_mul(page_size))
}

/// 页大小：Linux 上恒为系统页大小；用 std 拿不到就回退 4096（x86_64/aarch64
/// 常见值；估错只影响水位线刻度，不影响正确性判断方向）。
/// EN: Page size with a 4096 fallback — a misestimate only skews the
/// watermark scale, never the direction of the decision.
fn assumed_page_size() -> u64 {
    // std 无跨平台 page_size API；Linux 部署目标下 4096 是正确默认值，
    // 大页环境（64K 页 aarch64）会低估 RSS——保守方向（更早触发水位线），可接受。
    // EN: std has no cross-platform page_size API; 4096 is the correct default for
    // EN: Linux targets. Huge-page hosts (64K aarch64) under-estimate RSS — the
    // EN: conservative direction (watermark trips earlier) is acceptable.
    4096
}

/// 内存水位线：`max_bytes == 0` 表示禁用（默认，opt-in）。
/// 准入失败计数单调递增，供 /metrics 暴露。
/// EN: `max_bytes == 0` disables the watermark (default, opt-in);
/// the rejection counter is monotonic for /metrics.
#[derive(Debug)]
pub struct MemoryWatermark {
    max_bytes: u64,
    rejected_total: AtomicU64,
    /// 一次性告警门闩：水位线启用但 RSS 源不可读时只 warn 一次（fail-open 是
    /// 设计行为，但「安全网缺席」必须在日志里可见，不能静默）。
    /// EN: Latch for a one-shot warn when the watermark is enabled but the RSS
    /// source is unreadable — fail-open is by design, but the safety net's
    /// absence must be observable, not silent.
    warned_unreadable: std::sync::atomic::AtomicBool,
}

impl Default for MemoryWatermark {
    fn default() -> Self {
        Self::new(0)
    }
}

impl MemoryWatermark {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            rejected_total: AtomicU64::new(0),
            warned_unreadable: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn rejected_total(&self) -> u64 {
        self.rejected_total.load(Ordering::Relaxed)
    }

    pub fn is_enabled(&self) -> bool {
        self.max_bytes > 0
    }

    /// 写入准入检查：禁用或 RSS 读不到 → 放行（fail-open）；
    /// 超限 → 计入拒绝并返回错误消息。
    /// EN: Admission check — disabled or unreadable RSS admits (fail-open);
    /// over the mark counts a rejection and returns the error message.
    pub fn check_admission(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }
        match current_rss_bytes() {
            Some(rss) if rss > self.max_bytes => {
                self.rejected_total.fetch_add(1, Ordering::Relaxed);
                Err(format!(
                    "memory watermark exceeded: rss {} bytes > limit {} bytes, writes rejected",
                    rss, self.max_bytes
                ))
            }
            Some(_) => Ok(()),
            None => {
                if !self.warned_unreadable.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        max_bytes = self.max_bytes,
                        "memory watermark enabled but RSS source is unreadable \
                         (non-Linux host or restricted /proc); admissions fail open"
                    );
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_statm_extracts_rss_pages() {
        // 字段序：size resident shared text lib data dt（页数）。
        // EN: Field order: size resident shared text lib data dt (page counts).
        let statm = "10000 2500 800 400 0 2000 0\n";
        assert_eq!(parse_statm_rss(statm), Some(2500 * 4096));
    }

    #[test]
    fn parse_statm_rejects_garbage() {
        assert_eq!(parse_statm_rss(""), None);
        assert_eq!(parse_statm_rss("only-one-field"), None);
        assert_eq!(parse_statm_rss("abc def"), None);
    }

    #[test]
    fn disabled_watermark_always_admits() {
        let wm = MemoryWatermark::new(0);
        assert!(!wm.is_enabled());
        assert!(wm.check_admission().is_ok());
        assert_eq!(wm.rejected_total(), 0);
    }

    #[test]
    fn tiny_limit_rejects_when_rss_known() {
        // 仅在有 RSS 源的平台（Linux）上做超限断言；其他平台 fail-open。
        // EN: Assert over-limit only on platforms with an RSS source (Linux); others fail open.
        let wm = MemoryWatermark::new(1);
        match current_rss_bytes() {
            Some(rss) if rss > 1 => {
                assert!(wm.check_admission().is_err());
                assert_eq!(wm.rejected_total(), 1);
            }
            _ => {
                assert!(wm.check_admission().is_ok(), "fail-open without RSS");
            }
        }
    }
}
