//! OpenAI 兼容网关上游熔断器（P4.3.3）。
//! Circuit breaker for the OpenAI-compatible gateway upstream (P4.3.3).
//!
//! 状态机：Closed →(连续 `BREAKER_FAILURE_THRESHOLD` 次相关失败)→ Open
//! （请求快速失败 503，不调上游，给上游恢复窗口）→（cooldown 过后首个请求
//! CAS 抢到迁移名额）→ HalfOpen（同时只放一个探测请求，其余仍快速失败）
//! →探测成功回 Closed（计数清零）/ 探测失败回 Open（重新锚定 cooldown）。
//!
//! State machine: Closed → Open after N consecutive relevant failures (requests
//! fast-fail 503 without touching the upstream), HalfOpen after the cooldown
//! (a single probe is admitted, the rest still fast-fail), back to Closed on
//! probe success or Open on probe failure (cooldown re-anchored).
//!
//! 失败口径（与 HTTP 语义对齐）：
//! - 传输错误（连接失败/超时）、非 JSON 响应体（上游违约）、上游 5xx 计失败
//!   ——上游病了；
//! - 上游 4xx 是请求形状问题（客户端错误）：Closed 下不计失败也不清零——
//!   上游还活着，病的是这条请求；HalfOpen 探测收到 4xx 视为探测通过
//!   （上游答了 = 活着），否则探测名额永不释放、网关永久 503 卡死；
//! - 2xx 成功清零。
//!
//! Epoch（报告归属）：`admit` 返回准入时的 epoch 令牌，回报必须带它。
//! 每次开路 epoch +1：Closed 时代准入的在途请求若在 Open 之后才回报，
//! 其令牌已过期，回报被丢弃——迟到成功不能绕过 cooldown 直接合闸，
//! 迟到失败也不能顶替在途探测的结果。状态与 epoch 打包在同一个
//! AtomicU64（高 32 位 epoch、低 8 位状态），单次 load 即得一致快照，
//! Open→HalfOpen 的 CAS 期望携带 epoch，任何并发重锚都会令其失败——
//! 「cooldown 重新锚定」是可执行语义，不只是文档承诺。
//!
//! EN: Reports carry the epoch token handed out by `admit`; every trip bumps
//! the epoch, so late reports from requests admitted before the trip are
//! dropped — a stale success cannot bypass the cooldown, a stale failure
//! cannot impersonate the in-flight probe. State and epoch share one packed
//! AtomicU64 so a single load is a consistent snapshot and the
//! Open→HalfOpen CAS detects any concurrent re-anchor.
//!
//! 参数为常量（5 次 / 30s，行业通用默认）：暂无按部署调参的真实运维诉求，
//! 待反馈再提为配置项（YAGNI）。全原子操作，无锁——网关在请求热路径上。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 连续相关失败达到该次数即开路。
/// Consecutive relevant failures that trip the breaker open.
pub const BREAKER_FAILURE_THRESHOLD: u64 = 5;
/// 开路冷却毫秒数：过后放行单个半开探测。
/// Open-state cooldown in ms before a single half-open probe is admitted.
pub const BREAKER_COOLDOWN_MS: u64 = 30_000;

// 状态编码导出 crate 内共享：admit 的 Err 载荷与 houyidb_openai_breaker_state
// 指标、503 文案必须用同一份编码——各写一份裸字面量会在状态重排时静默错标。
// EN: State codes are shared crate-wide: admit's Err payload, the
// breaker-state metric, and the 503 wording must use one encoding — private
// raw literals at each site silently mislabel if the states are renumbered.
pub(crate) const STATE_CLOSED: u8 = 0;
pub(crate) const STATE_OPEN: u8 = 1;
pub(crate) const STATE_HALF_OPEN: u8 = 2;
// 模块内短名别名（同一份编码，单一定义点）。
// EN: Short aliases within the module (one encoding, single definition site).
const CLOSED: u8 = STATE_CLOSED;
const OPEN: u8 = STATE_OPEN;
const HALF_OPEN: u8 = STATE_HALF_OPEN;

/// 状态字打包：高 32 位 epoch（每次开路 +1），低 8 位状态。
/// EN: Packed state word — high 32 bits epoch (bumped per trip), low 8 state.
const fn pack(epoch: u64, state: u8) -> u64 {
    (epoch << 32) | state as u64
}
const fn unpack_epoch(packed: u64) -> u64 {
    packed >> 32
}
const fn unpack_state(packed: u64) -> u8 {
    packed as u8
}

pub struct UpstreamBreaker {
    /// 打包状态字（epoch + state），见 [`pack`]。
    /// EN: Packed state word (epoch + state); see [`pack`].
    packed: AtomicU64,
    consecutive_failures: AtomicU64,
    /// 最近一次开路的 epoch 毫秒（cooldown 计时的锚点）。
    /// EN: Epoch milliseconds of the latest trip (anchor for cooldown timing).
    opened_at_ms: AtomicU64,
    /// 因开路/半开被快速拒绝的请求总数（指标）。
    /// EN: Total requests fast-rejected due to Open/HalfOpen (metric).
    rejected_open_total: AtomicU64,
    /// 累计开路次数（指标）。
    /// EN: Cumulative trip count (metric).
    open_total: AtomicU64,
}

impl Default for UpstreamBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamBreaker {
    pub fn new() -> Self {
        Self {
            packed: AtomicU64::new(pack(0, CLOSED)),
            consecutive_failures: AtomicU64::new(0),
            opened_at_ms: AtomicU64::new(0),
            rejected_open_total: AtomicU64::new(0),
            open_total: AtomicU64::new(0),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 请求准入：Ok(epoch) = 放行（Closed，或抢到唯一半开探测名额——调用方
    /// 随后必须带该 epoch 回报 [`Self::on_success`]/[`Self::on_failure`]，
    /// 否则探测名额永久卡死）；Err(state) = 熔断快速失败，调用方不得调上游。
    /// Err 携带**拒绝那一刻**的状态码（1=Open 冷却中 / 2=HalfOpen 探测在途）：
    /// 与准入判定同一份快照，拒绝文案不会与指标脱节（flapping 期间二次读
    /// state_code 可能已迁移，给出误导性诊断）。
    /// EN: Admission. Ok(epoch) = proceed (Closed, or the single half-open
    /// probe slot — the caller MUST then report back with this epoch or the
    /// slot sticks forever); Err(state) = fast-fail, carrying the state at
    /// the instant of rejection from the same snapshot as the decision, so
    /// the error message can't drift from the metrics during flapping.
    pub fn admit(&self) -> Result<u64, u8> {
        let packed = self.packed.load(Ordering::Acquire);
        let epoch = unpack_epoch(packed);
        match unpack_state(packed) {
            CLOSED => Ok(epoch),
            OPEN => {
                let opened = self.opened_at_ms.load(Ordering::Acquire);
                if Self::now_ms().saturating_sub(opened) < BREAKER_COOLDOWN_MS {
                    self.rejected_open_total.fetch_add(1, Ordering::Relaxed);
                    return Err(OPEN);
                }
                // 冷却结束：并发请求里只有一个能赢得 Open→HalfOpen 迁移。
                // 期望值携带我们读到的 epoch——任何并发 trip（含探测失败的
                // 重锚）都会 +1 epoch 令 CAS 失败，杜绝「按旧锚点提前放行」。
                // EN: Cooldown over — only one request wins the transition.
                // The expected value carries our epoch: any concurrent trip
                // (including a probe-failure re-anchor) bumps it and fails
                // the CAS, so no probe slips out early against a stale anchor.
                if self
                    .packed
                    .compare_exchange(
                        pack(epoch, OPEN),
                        pack(epoch, HALF_OPEN),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(epoch);
                }
                // 迁移输了：状态已被别的请求改动——Closed（探测已成功）放行，
                // Open（重锚）/HalfOpen（探测在途）拒绝。
                // EN: Lost the race — admit if the probe already succeeded
                // (Closed), reject on re-anchored Open or in-flight probe.
                let cur = self.packed.load(Ordering::Acquire);
                let cur_state = unpack_state(cur);
                if cur_state == CLOSED {
                    Ok(unpack_epoch(cur))
                } else {
                    self.rejected_open_total.fetch_add(1, Ordering::Relaxed);
                    Err(cur_state)
                }
            }
            // HALF_OPEN：探测在途，其余请求快速失败。
            // EN: HALF_OPEN: a probe is in flight; other requests fail fast.
            state => {
                self.rejected_open_total.fetch_add(1, Ordering::Relaxed);
                Err(state)
            }
        }
    }

    /// 上游调用成功（2xx）：清零失败计数、释放探测名额、回到 Closed。
    /// `epoch` 必须是准入时拿到的令牌；过期令牌（开路前的在途请求）直接丢弃，
    /// 迟到的成功不得绕过 cooldown 合闸。
    /// EN: Upstream success — reset the counter, free the probe slot, close.
    /// Stale epoch tokens (in-flight from before a trip) are dropped: a late
    /// success must not bypass the cooldown.
    pub fn on_success(&self, epoch: u64) {
        let mut cur = self.packed.load(Ordering::Acquire);
        loop {
            if unpack_epoch(cur) != epoch {
                return;
            }
            match self.packed.compare_exchange_weak(
                cur,
                pack(epoch, CLOSED),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // 计数清零放在 CAS 之后：窗口内若有新一轮失败入账，
                    // 最坏是少计一发推迟开路——熔断是启发式保护，可接受。
                    // EN: Reset after the CAS — a failure landing in the gap
                    // is at worst under-counted by one, delaying a trip;
                    // acceptable for a heuristic guard.
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                    return;
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// 上游调用失败。`epoch` 为准入令牌（过期丢弃，语义同 [`Self::on_success`]）。
    /// `breaker_relevant` 区分失败口径：传输错误/5xx/违约响应体为 true（计失败），
    /// 上游 4xx 为 false（Closed 下不计不清零；HalfOpen 下视为探测通过——
    /// 上游答了就是活着，不释放名额会把整个网关卡死在 503）。
    /// EN: Upstream failure with the admission epoch (stale tokens dropped).
    /// `breaker_relevant`: transport/5xx/contract violations count; upstream
    /// 4xx neither counts nor resets while Closed, and resolves the probe as
    /// alive while HalfOpen — otherwise the probe slot would never free and
    /// the gateway would wedge at 503 until restart.
    pub fn on_failure(&self, epoch: u64, breaker_relevant: bool) {
        let cur = self.packed.load(Ordering::Acquire);
        if unpack_epoch(cur) != epoch {
            return;
        }
        let state = unpack_state(cur);
        if !breaker_relevant {
            if state == HALF_OPEN {
                self.on_success(epoch);
            }
            return;
        }
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if state == HALF_OPEN || (state == CLOSED && failures >= BREAKER_FAILURE_THRESHOLD) {
            self.trip_open();
        }
    }

    fn trip_open(&self) {
        let now = Self::now_ms();
        let mut cur = self.packed.load(Ordering::Acquire);
        loop {
            let prev_state = unpack_state(cur);
            // 每次开路 epoch +1：使所有旧令牌一次性失效。
            // EN: Every trip bumps the epoch, invalidating all old tokens.
            let next = pack(unpack_epoch(cur) + 1, OPEN);
            match self
                .packed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.opened_at_ms.store(now, Ordering::Release);
                    // 并发双重 trip（两个请求同时失败越线）只计一次开路；
                    // opened_at 重锚无害（两写几乎同刻，last-wins）。
                    // EN: A concurrent double-trip counts once; re-anchoring
                    // opened_at is harmless (~simultaneous writes, last wins).
                    if prev_state != OPEN {
                        self.open_total.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            open_total = self.open_total(),
                            cooldown_ms = BREAKER_COOLDOWN_MS,
                            "openai upstream circuit breaker tripped open"
                        );
                    }
                    return;
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// 状态码（指标用）：0=Closed 1=Open 2=HalfOpen。
    /// EN: State code (for metrics): 0=Closed 1=Open 2=HalfOpen.
    pub fn state_code(&self) -> u8 {
        unpack_state(self.packed.load(Ordering::Acquire))
    }

    pub fn rejected_open_total(&self) -> u64 {
        self.rejected_open_total.load(Ordering::Relaxed)
    }

    pub fn open_total(&self) -> u64 {
        self.open_total.load(Ordering::Relaxed)
    }

    /// 测试专用：把 cooldown 锚点改到过去，等价于「开路已过去 at_least_ms」。
    /// EN: Test-only — age the cooldown anchor by at least the given ms.
    #[cfg(test)]
    fn age_open_by_ms(&self, at_least_ms: u64) {
        let anchor = Self::now_ms().saturating_sub(at_least_ms);
        self.opened_at_ms.store(anchor, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closed 下 admit 无副作用（不发名额），可反复调用取当前 epoch。
    /// EN: Under Closed, admit is side-effect free (no slot issued); call it
    /// EN: repeatedly to read the current epoch.
    fn current_epoch(b: &UpstreamBreaker) -> u64 {
        b.admit().expect("closed breaker admits")
    }

    fn trip_to_open(b: &UpstreamBreaker) {
        let e = current_epoch(b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            b.on_failure(e, true);
        }
    }

    /// Closed 下未达阈值保持放行；达到阈值开路，请求快速失败并计数。
    /// EN: Under threshold stays Closed/admitting; at threshold opens and
    /// fast-fails with the rejection counted.
    #[test]
    fn closed_trips_open_at_threshold() {
        let b = UpstreamBreaker::new();
        assert_eq!(b.state_code(), CLOSED);
        let e = current_epoch(&b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
            assert!(b.admit().is_ok(), "under threshold must stay closed");
        }
        assert_eq!(b.state_code(), CLOSED);
        b.on_failure(e, true);
        assert_eq!(b.state_code(), OPEN);
        assert!(b.admit().is_err(), "open breaker must fast-fail");
        assert!(b.admit().is_err());
        assert_eq!(b.rejected_open_total(), 2);
        assert_eq!(b.open_total(), 1);
    }

    /// 成功调用清零连续失败计数（失败-成功-再失败重新累计）。
    /// EN: A success resets the consecutive-failure counter.
    #[test]
    fn success_resets_consecutive_failures() {
        let b = UpstreamBreaker::new();
        let e = current_epoch(&b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
        }
        b.on_success(e);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
        }
        assert_eq!(b.state_code(), CLOSED, "reset must delay the trip");
        b.on_failure(e, true);
        assert_eq!(b.state_code(), OPEN);
    }

    /// 4xx 类失败（breaker_relevant=false）不计也不清零。
    /// EN: 4xx-class failures neither count nor reset.
    #[test]
    fn irrelevant_failure_neither_counts_nor_resets() {
        let b = UpstreamBreaker::new();
        let e = current_epoch(&b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
        }
        b.on_failure(e, false);
        assert!(b.admit().is_ok(), "4xx must not trip the breaker");
        // 若 4xx 清零，这一发相关失败不会开路；若计入，上一发就已开路。
        // EN: If 4xx reset the count, this relevant failure would not trip; if
        // EN: counted, the previous one already tripped.
        b.on_failure(e, true);
        assert_eq!(b.state_code(), OPEN);
    }

    /// Open 冷却中拒绝；冷却过后首个请求拿到探测名额（HalfOpen），
    /// 并发第二请求仍快速失败。
    /// EN: Open rejects during cooldown; after it, the first request wins the
    /// single probe slot while a concurrent one still fast-fails.
    #[test]
    fn open_ages_to_half_open_single_probe() {
        let b = UpstreamBreaker::new();
        trip_to_open(&b);
        // Err 载荷钉住「拒绝瞬间的状态」——503 文案靠它区分冷却中与探测
        // 在途，载荷漂移则文案静默错标（OCR a5b5520）。
        // EN: The Err payload pins the state at rejection time — the 503 wording
        // EN: distinguishes cooldown from probe-in-flight by it; payload drift
        // EN: silently mislabels the message (OCR a5b5520).
        assert!(
            matches!(b.admit(), Err(STATE_OPEN)),
            "cooling-down rejection must carry OPEN"
        );
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        assert!(b.admit().is_ok(), "aged open must admit the probe");
        assert_eq!(b.state_code(), HALF_OPEN);
        assert!(
            matches!(b.admit(), Err(STATE_HALF_OPEN)),
            "in-flight-probe rejection must carry HALF_OPEN"
        );
    }

    /// 半开探测成功回 Closed，失败计数清零，恢复放行。
    /// EN: A successful probe closes the breaker and resets the counter.
    #[test]
    fn half_open_probe_success_closes() {
        let b = UpstreamBreaker::new();
        trip_to_open(&b);
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        let e = b.admit().expect("probe admitted");
        b.on_success(e);
        assert_eq!(b.state_code(), CLOSED);
        assert!(b.admit().is_ok());
        // 计数已清零：阈值 - 1 次失败不再开路。
        // EN: Count was reset: threshold - 1 failures no longer trip the breaker.
        let e = current_epoch(&b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
        }
        assert_eq!(b.state_code(), CLOSED);
    }

    /// 半开探测失败回 Open 并重新锚定 cooldown（不老化立即拒绝）。
    /// EN: A failed probe re-opens with the cooldown re-anchored.
    #[test]
    fn half_open_probe_failure_reopens() {
        let b = UpstreamBreaker::new();
        trip_to_open(&b);
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        let e = b.admit().expect("probe admitted");
        b.on_failure(e, true);
        assert_eq!(b.state_code(), OPEN);
        assert_eq!(b.open_total(), 2);
        assert!(b.admit().is_err(), "re-anchored cooldown must reject again");
    }

    /// 半开探测收到 4xx（上游答了但请求形状有问题）视为探测通过：
    /// 名额释放、回合、计数清零——不释放则网关永久 503 卡死。
    /// EN: A 4xx probe response resolves the probe as alive (slot freed,
    /// breaker closed, counter reset) — otherwise the gateway wedges at 503.
    #[test]
    fn half_open_probe_4xx_resolves_as_alive() {
        let b = UpstreamBreaker::new();
        trip_to_open(&b);
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        let e = b.admit().expect("probe admitted");
        b.on_failure(e, false);
        assert_eq!(b.state_code(), CLOSED, "4xx probe must close the breaker");
        assert!(b.admit().is_ok());
        // 探测通过 = 计数清零：阈值 - 1 次失败不再开路。
        // EN: Probe success = count reset: threshold - 1 failures no longer trip.
        let e = current_epoch(&b);
        for _ in 0..BREAKER_FAILURE_THRESHOLD - 1 {
            b.on_failure(e, true);
        }
        assert_eq!(b.state_code(), CLOSED);
    }

    /// 旧 epoch 的迟到回报一律丢弃：开路前准入的在途请求，其成功不得
    /// 绕过 cooldown 合闸，其失败不得顶替在途探测触发重锚。
    /// EN: Stale-epoch reports are dropped — a pre-trip in-flight success
    /// must not close an Open breaker, nor a stale failure re-anchor it.
    #[test]
    fn stale_epoch_reports_are_dropped() {
        let b = UpstreamBreaker::new();
        let stale = current_epoch(&b);
        trip_to_open(&b);
        assert_eq!(b.state_code(), OPEN);
        // 迟到的成功：不得合闸。
        // EN: Late success: must not close the breaker.
        b.on_success(stale);
        assert_eq!(b.state_code(), OPEN, "stale success must not close");
        // 冷却过后真探测上场；此时迟到的旧失败不得重锚。
        // EN: After cooldown the real probe runs; a stale failure must not re-anchor.
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        let probe = b.admit().expect("probe admitted");
        b.on_failure(stale, true);
        assert_eq!(
            b.state_code(),
            HALF_OPEN,
            "stale failure must not re-anchor"
        );
        // 真探测的结果仍然生效。
        // EN: The real probe's outcome still takes effect.
        b.on_success(probe);
        assert_eq!(b.state_code(), CLOSED);
    }

    /// 半开探测成功后，并发被拒期间累计的 rejected 计数不回退（指标只增）。
    /// EN: Rejection counters are monotonic across transitions.
    #[test]
    fn rejected_counter_is_monotonic() {
        let b = UpstreamBreaker::new();
        trip_to_open(&b);
        assert!(b.admit().is_err());
        b.age_open_by_ms(BREAKER_COOLDOWN_MS + 1);
        let e = b.admit().expect("probe admitted");
        assert!(b.admit().is_err());
        b.on_success(e);
        assert_eq!(b.rejected_open_total(), 2);
        assert!(b.admit().is_ok());
        assert_eq!(b.rejected_open_total(), 2);
    }
}
