#!/usr/bin/env bash
# P4.5.2 并发原语调度检查（本地纪律项，不进 CI）。
# EN: P4.5.2 concurrency primitive scheduling check (local discipline, not CI).
#
# 两道互补的网，都只打 `houyi-persistence` 的 `replication_leader.rs`：
# EN: Two complementary nets, both targeting only `replication_leader.rs`:
# 1. shuttle（随机调度，PCT）——主力。把 ShipHub 扇出/踢除、AckWatermarks 水位线
# 推进/分段隔离、SyncWaiterRegistry 注册与唤醒放进随机调度器穷举交错。
# shuttle 把所有原子按 SeqCst 建模，故查不出弱内存序缺陷。
# EN: 1. shuttle (random scheduling, PCT) — the workhorse. Puts ShipHub
# EN: fan-out/eviction, AckWatermarks advancement/segment isolation, and
# EN: SyncWaiterRegistry registration/wakeup through exhaustive interleavings.
# EN: shuttle models all atomics as SeqCst, so it cannot find weak-ordering bugs.
# 2. loom（穷举交错，`--cfg loom`）——补充。只覆盖 2~3 线程小场景的内存序：
# 丢唤醒（wait 必须在水位线满足后返回）与跨阶段 acked 标志的发布顺序
# （false durability 方向）。crossbeam 本体与 houyi-core::bus 薄包装层不重复验证。
# EN: 2. loom (exhaustive interleavings, `--cfg loom`) — the supplement. Covers
# EN: memory ordering only in small 2~3 thread scenarios: lost wakeups (wait
# EN: must return after the watermark is satisfied) and publication order of
# EN: cross-stage acked flags (the false-durability direction). crossbeam itself
# EN: and the thin houyi-core::bus wrapper are not re-verified.
#
# 纪律（重要）：shuttle 换型只作用于 `replication_leader.rs` 一个文件，本 crate 其余
# 单测仍用 std 原语——两者同进程混跑会在运行期 panic（"Are you accessing a Shuttle
# primitive outside of a Shuttle test?"）。因此**必须**带 `shuttle_` 名字过滤；loom
# 同理带 `loom_`。这也是本脚本存在的原因（别用裸 `cargo test -p houyi-persistence
# --features shuttle`）。同理，houyi-server 不得与本 feature 同构建；
# `--all-features` 与 workspace 级 `--features shuttle` 一律禁止（`replication_leader.rs`
# 顶部有 compile_error! 护栏，会把误用挡在编译期）。
# EN: Discipline (important): the shuttle swap applies only to
# EN: `replication_leader.rs`; other unit tests in the crate still use std
# EN: primitives — running both in one process panics at runtime.
# EN: Therefore the `shuttle_` name filter is **required**; same for loom with
# EN: `loom_`. This is why this script exists (do not use bare `cargo test -p
# EN: houyi-persistence --features shuttle`). Likewise, houyi-server must not be
# EN: built with this feature; `--all-features` and workspace-level
# EN: `--features shuttle` are forbidden (the compile_error! guard at the top of
# EN: `replication_leader.rs` blocks misuse at compile time).
#
# 用法 / Usage:
# EN: Usage:
# ./scripts/concurrency_check.sh              # shuttle + loom 全跑
# ONLY=shuttle ./scripts/concurrency_check.sh # 只跑一道
# SHUTTLE_RANDOM_SEED=12345 ./scripts/concurrency_check.sh   # 固定种子复现
# LOOM_MAX_PREEMPTIONS=3 ./scripts/concurrency_check.sh      # 加深 loom 搜索
# LOOM_TARGET_DIR=/path/to/dir ./scripts/concurrency_check.sh # 换 loom 构建目录
#
# 复现纪律 / Reproduction: shuttle 失败会打印一段编码调度串与 seed——用
# `shuttle::replay` 或设 SHUTTLE_RANDOM_SEED 复现；loom 失败会打印 deadlock/断言
# 现场，用 LOOM_MAX_PREEMPTIONS 逐级加深确认。
# EN: Reproduction: a shuttle failure prints an encoded schedule and seed — replay
# EN: it with `shuttle::replay` or SHUTTLE_RANDOM_SEED; a loom failure prints the
# EN: deadlock/assertion scene; deepen the search step by step with
# EN: LOOM_MAX_PREEMPTIONS.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
CRATE="houyi-persistence"
ONLY="${ONLY:-}"

# ONLY 白名单：拼错（如 `ONLY=shutle`）会让两个分支都不匹配、静默跑全套，
# 既违背调用意图又白付一轮时间。
# EN: ONLY whitelist: a typo (e.g. `ONLY=shutle`) matches neither branch and silently
# EN: runs everything, violating the caller's intent and wasting a full round.
case "$ONLY" in
    "" | shuttle | loom) ;;
    *)
        echo "ONLY 只接受 shuttle 或 loom（当前：${ONLY}）" >&2
        exit 2
        ;;
esac

cd "$ROOT"
mkdir -p target

failed=""
ran=""

# 跑一套并校验**确实跑了测试**。
#
# WHY（重要）：`cargo test` 在名字过滤匹配 0 个测试时仍退出 0。只信退出码的话，
# 将来某次重命名（或新加一个没带前缀的 `mod sc` 测试）会让整个门禁静默空转，
# 而本脚本是这层验证的唯一执行点（不进 CI）。故解析 `test result` 行的通过数，
# `> 0` 才算通过。
# EN: `cargo test` exits 0 when the name filter matches nothing, which would
# silently neuter this gate; require at least one test to actually run.
run_suite() {
    local suite="$1" filter="$2" log="$3"
    shift 3
    if "$@" 2>&1 | tee "$log"; then
        local passed
        passed="$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$log" | tail -1)"
        if [ -z "$passed" ] || [ "$passed" -eq 0 ]; then
            echo "  ✗ ${suite}：过滤 '${filter}' 匹配到 0 个测试——命名纪律（${filter}*）被破坏？" >&2
            failed="${failed} ${suite}(空转)"
            return 1
        fi
        echo "  ✓ ${suite} 全绿（${passed} 个测试）"
        ran="${ran} ${suite}"
    else
        echo "  ✗ ${suite} 失败（完整输出见 ${log}）" >&2
        failed="${failed} ${suite}"
        return 1
    fi
}

ambient_has_loom_cfg() {
    case " ${RUSTFLAGS:-} " in
        *" --cfg loom "* | *" --cfg=loom "*) return 0 ;;
        *) return 1 ;;
    esac
}

if [ "$ONLY" != "loom" ]; then
    if ambient_has_loom_cfg; then
        echo "环境 RUSTFLAGS 已含 --cfg loom，与 shuttle 臂互斥；请先清除再跑。" >&2
        exit 2
    fi
    echo "── shuttle 随机调度（${CRATE}，过滤 shuttle_）──────────"
    # SHUTTLE_RANDOM_SEED 由 shuttle 自己读（未设则每轮随机）；失败时打印的 seed
    # 直接回填即可复现同一交错序列。
    # EN: SHUTTLE_RANDOM_SEED is read by shuttle itself (random each round if unset); a
    # EN: seed printed on failure can be fed back to reproduce the same interleaving.
    run_suite shuttle shuttle_ "$ROOT/target/cx-shuttle.log" \
        cargo test -p "$CRATE" --features shuttle --lib shuttle_ || true
fi

if [ "$ONLY" != "shuttle" ]; then
    # loom 的**穷举**由编译 loom crate 时的 `--cfg loom` 决定（cargo feature
    # 只会换我们自己的类型，模型检查会静默退化为单次执行——P4.5.5 曾踩过）。
    # 全局 cfg 会关掉 tokio 的 `net` 模块（tokio 源码 `#[cfg(not(loom))]`），
    # 故复制层的网络/会话代码全部 `#[cfg(not(loom))]` 门控。
    # EN: loom's exhaustive exploration requires `--cfg loom` when compiling the
    # loom crate itself; the replication network code is cfg-gated away.
    case " ${RUSTFLAGS:-} " in
        *" --cfg loom "* | *" --cfg=loom "*)
            LOOM_RUSTFLAGS="${RUSTFLAGS:-}" ;;
        *)
            LOOM_RUSTFLAGS="${RUSTFLAGS:-} --cfg loom" ;;
    esac
    # RUSTFLAGS 参与构建指纹：与默认/shuttle 共用 target/ 会让整张依赖图反复
    # 全量重编译。独立 target 目录把这份代价一次性付掉。
    # EN: RUSTFLAGS participates in the build fingerprint: sharing target/ with
    # EN: default/shuttle rebuilds the whole dependency graph repeatedly. A dedicated
    # EN: target dir pays that cost once.
    LOOM_TARGET_DIR="${LOOM_TARGET_DIR:-$ROOT/target/loom}"
    echo "── loom 穷举交错（${CRATE}，过滤 loom_，RUSTFLAGS+=--cfg loom）──"
    run_suite loom loom_ "$ROOT/target/cx-loom.log" \
        env RUSTFLAGS="$LOOM_RUSTFLAGS" CARGO_TARGET_DIR="$LOOM_TARGET_DIR" \
        cargo test -p "$CRATE" --lib loom_ || true
fi

if [ -n "$failed" ]; then
    echo "失败：${failed}" >&2
    exit 1
fi
echo "并发原语调度检查全绿（${ran# }）。"
