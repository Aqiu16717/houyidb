//! 故障注入点（P4.5.4）——只在 `failpoints` feature 下生效。
//! Failpoint hooks (P4.5.4) — active only with the `failpoints` feature.
//!
//! 为什么自写而不引 `fail-rs` / Why hand-rolled: `fail-rs` 已四年未发版，
//! 而这里需要的只是「按名字查规则 → 抽样 → 动作」三步；自写把依赖面压到零，
//! 也让未启用 feature 时的开销严格归零（`hit` 是内联空函数，调用点被编译器
//! 整段消除）。EN: `fail-rs` is unmaintained; the required behavior is a
//! three-step lookup/sample/act, so rolling it keeps the dependency surface at
//! zero and the disabled-feature overhead at exactly zero.
//!
//! 激活 / Activation: `HOUYI_FAILPOINTS="name=prob:action;name2:action;..."`，
//! 例：`HOUYI_FAILPOINTS="wal_append_pre=0.2:err;graph_snapshot_write_mid=1:panic"`。
//!
//! - `prob`（可选，缺省 1.0，越界自动夹到 [0,1]）：每次命中的触发概率。
//! - `action`：
//!   - `pause[:ms]` —— sleep（缺省 1000ms），用于拉开 kill -9 的落点窗口；
//!   - `err` —— 返回 `io::ErrorKind::Other`（模拟 ENOSPC/EIO 等系统错误）；
//!   - `panic` —— 直接 panic（配合 `panic=abort` 等价一次硬崩）。
//!
//! 抽样不可复现（种子取 pid+时钟）**是刻意的**：演练的可复现性由
//! `scripts/chaos_drill.sh` 的 `CHAOS_SEED` 决定 kill 时刻与故障矩阵，注入点
//! 只负责把「崩溃可能落在哪些窗口」拉开；把抽样也做成可复现会让「换个种子就
//! 撞不到某个窗口」变成隐藏的假通过。EN: Sampling is intentionally
//! non-reproducible; the drill's seed controls kill timing and the matrix.
//!
//! 已知边界 / Known limits: 规则在**首次命中**时解析（`OnceLock`），进程启动后
//! 再改环境变量无效——演练必须在启动子进程前设好 `HOUYI_FAILPOINTS`。

use std::io;

/// 命中一个注入点。
/// Hit one failpoint.
///
/// 未启用 `failpoints` feature 时始终返回 `Ok(())`（内联空操作，零开销）。
/// Without the feature this is an inline no-op returning `Ok(())`.
#[cfg(not(feature = "failpoints"))]
#[inline(always)]
pub fn hit(_name: &'static str) -> io::Result<()> {
    Ok(())
}

/// 命中一个注入点；未配置或本次抽样未命中时返回 `Ok(())`。
/// Hit one failpoint; `Ok(())` when unconfigured or the sample misses.
#[cfg(feature = "failpoints")]
pub fn hit(name: &'static str) -> io::Result<()> {
    use std::sync::atomic::AtomicU64;
    use std::sync::OnceLock;
    use std::time::Duration;

    static RULES: OnceLock<std::collections::HashMap<String, (f64, Action)>> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: OnceLock<u64> = OnceLock::new();

    let rules =
        RULES.get_or_init(|| parse_spec(&std::env::var("HOUYI_FAILPOINTS").unwrap_or_default()));
    let Some(&(prob, action)) = rules.get(name) else {
        return Ok(());
    };
    if !roll(prob, &COUNTER, &SEED) {
        return Ok(());
    }
    match action {
        Action::Pause(ms) => {
            std::thread::sleep(Duration::from_millis(ms));
            Ok(())
        }
        Action::Err => Err(io::Error::other(format!(
            "failpoint '{name}' injected error"
        ))),
        Action::Panic => panic!("failpoint '{name}' injected panic"),
    }
}

/// 注入动作 / Injected action.
/// EN: Injected action kinds.
#[cfg(feature = "failpoints")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Pause(u64),
    Err,
    Panic,
}

/// 按概率抽样。0/1 直接短路，避免浮点误差让边界概率出现「永不中/永远中」。
/// Sample by probability; the 0/1 endpoints short-circuit.
#[cfg(feature = "failpoints")]
fn roll(
    prob: f64,
    counter: &std::sync::atomic::AtomicU64,
    seed: &std::sync::OnceLock<u64>,
) -> bool {
    use std::sync::atomic::Ordering;
    if prob >= 1.0 {
        return true;
    }
    if prob <= 0.0 {
        return false;
    }
    let n = counter.fetch_add(1, Ordering::Relaxed);
    let mut z = n
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(*seed.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            nanos ^ ((std::process::id() as u64) << 32)
        }));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z as f64 / u64::MAX as f64) < prob
}

/// 解析 `name=prob:action;...` 规则串（纯函数，便于单测）。
/// Parse the `name=prob:action;...` spec (pure, unit-testable).
///
/// 无法识别的条目只 warn + 跳过：演练环境里一个笔误不应让服务起不来——但
/// **不得静默**，否则「注入了却没生效」会被误读为「系统扛住了」。
/// EN: Unrecognized entries warn and are skipped; a typo must never silently
/// turn an injection into a no-op that looks like the system surviving.
#[cfg(feature = "failpoints")]
fn parse_spec(spec: &str) -> std::collections::HashMap<String, (f64, Action)> {
    use std::collections::HashMap;
    let mut out = HashMap::new();
    for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((name, rest)) = item.split_once('=') else {
            tracing::warn!(entry = item, "failpoint: expected name=prob:action");
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            tracing::warn!(entry = item, "failpoint: empty name");
            continue;
        }
        let (prob, action_text) = match rest.split_once(':') {
            Some((p, a)) => match p.trim().parse::<f64>() {
                // NaN/Inf 必须拒绝：`clamp` 对 NaN 原样返回，而 `roll` 里 NaN
                // 既不 >=1 也不 <=0，会掉进采样分支——行为未定义。
                // EN: Reject NaN/Inf: `clamp` passes NaN through, and in `roll`
                // NaN is neither >=1 nor <=0, so it enters sampling — undefined.
                Ok(v) if v.is_finite() => (v.clamp(0.0, 1.0), a.trim()),
                Ok(_) => {
                    tracing::warn!(entry = item, "failpoint: prob must be finite");
                    continue;
                }
                // `name=pause:50` —— 省略 prob 的写法：首段不是数字就把整段当
                // action（否则 `pause:50` 会被误报为「prob 不是数字」而跳过）。
                // EN: `name=pause:50` omits prob: when the first segment is not a
                // number, treat the whole segment as the action (else it is skipped).
                Err(_) if p.trim() == "pause" => (1.0, rest.trim()),
                Err(_) => {
                    tracing::warn!(entry = item, "failpoint: prob is not a number");
                    continue;
                }
            },
            None => (1.0, rest.trim()),
        };
        let action = if action_text == "err" {
            Action::Err
        } else if action_text == "panic" {
            Action::Panic
        } else if action_text == "pause" {
            Action::Pause(1000)
        } else if let Some(ms) = action_text.strip_prefix("pause:") {
            match ms.trim().parse::<u64>() {
                Ok(v) => Action::Pause(v),
                Err(_) => {
                    tracing::warn!(entry = item, "failpoint: pause duration is not a number");
                    continue;
                }
            }
        } else {
            tracing::warn!(entry = item, "failpoint: unknown action");
            continue;
        };
        if out.contains_key(name) {
            tracing::warn!(entry = item, "failpoint: duplicate name, later rule wins");
        }
        out.insert(name.to_string(), (prob, action));
    }
    // 未知名字只 warn：拼错或改名后注入点永远不触发，但配置本身合法——
    // 静默吞掉会让「注入了却没生效」被误读为「系统扛住了」。
    // EN: an unknown name never fires; warn so a typo is not read as survival.
    for name in out.keys() {
        if !KNOWN_FAILPOINTS.contains(&name.as_str()) {
            tracing::warn!(
                name = %name,
                "failpoint: no such injection point (typo or renamed?)"
            );
        }
    }
    out
}

#[cfg(feature = "failpoints")]
/// 代码里实际存在的注入点（新增注入点时同步登记）。
/// Known injection points — keep this list in sync with the call sites.
const KNOWN_FAILPOINTS: &[&str] = &[
    "wal_append_pre",
    "wal_append_post",
    "wal_fsync_pre",
    "graph_snapshot_write_mid",
    "repl_ack_send_pre",
    "follower_apply_pre",
];

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    use super::*;

    fn spec(s: &str) -> std::collections::HashMap<String, (f64, Action)> {
        parse_spec(s)
    }

    #[test]
    fn parses_probability_and_actions() {
        let p = spec("wal_append_pre=0.2:err;graph_mid=panic;slow=pause:50;plain=pause");
        assert_eq!(p.len(), 4);
        assert_eq!(p["wal_append_pre"], (0.2, Action::Err));
        assert_eq!(p["graph_mid"], (1.0, Action::Panic));
        assert_eq!(p["slow"], (1.0, Action::Pause(50)));
        assert_eq!(p["plain"], (1.0, Action::Pause(1000)));
    }

    #[test]
    fn probability_clamps_and_defaults() {
        let p = spec("a=5:err;b=-1:panic;c=err");
        assert_eq!(p["a"].0, 1.0, "prob > 1 clamps to 1");
        assert_eq!(p["b"].0, 0.0, "prob < 0 clamps to 0");
        assert_eq!(p["c"].0, 1.0, "missing prob defaults to 1.0");
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        for bad in [
            "no_equals_sign",
            "=0.5:err",
            "x=abc:err",
            "x=1:nonsense",
            "x=1:pause:abc",
            ";;",
        ] {
            assert!(spec(bad).is_empty(), "entry {bad:?} must be skipped");
        }
    }

    #[test]
    fn probability_endpoints_are_exact() {
        use std::sync::atomic::AtomicU64;
        use std::sync::OnceLock;
        let counter = AtomicU64::new(0);
        let seed = OnceLock::new();
        for _ in 0..100 {
            assert!(roll(1.0, &counter, &seed));
            assert!(!roll(0.0, &counter, &seed));
        }
        // 中间概率必须两种结果都出现（xorshift 混合没有退化）。
        // EN: A middling probability must produce both outcomes (xorshift mix intact).
        let hits = (0..1000).filter(|_| roll(0.5, &counter, &seed)).count();
        assert!(
            (300..700).contains(&hits),
            "p=0.5 gave {hits}/1000 hits — sampler looks degenerate"
        );
    }
}
