#!/usr/bin/env bash
# P4.5.1 cargo-fuzz 解析面夜间窗口（本地纪律项，不进 CI）。
#
# 覆盖 4 个 target：wal_v2_decode / graph_snapshot_restore / repl_frame_decode /
# pgwire_message_decode。每个 target 跑一个定时窗口，随后 cmin 最小化语料，最后
# 检查 fuzz/artifacts/<target>/ 有无**本次新增**的崩溃/超时/泄漏样本——有则打印并
# **以非零退出**（崩溃样本应入库并转成回归测试）。历史样本（已入库）不再报错。
#
# 用法 / Usage:
#   ./scripts/fuzz_nightly.sh                        # 每 target 600s
#   FUZZ_SECS=60 ./scripts/fuzz_nightly.sh           # 冒烟
#   TARGETS="repl_frame_decode" ./scripts/fuzz_nightly.sh
#   FUZZ_SECS=1800 SEED_CORPUS=0 ./scripts/fuzz_nightly.sh   # 跳过语料生成
#
# 前提 / Prereq: nightly 工具链 + cargo-fuzz（`cargo install cargo-fuzz`）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
FUZZ_DIR="$ROOT/fuzz"

FUZZ_SECS="${FUZZ_SECS:-600}"
# FUZZ_SECS 直接进 libFuzzer 的 -max_total_time：非数字只会在 nightly 构建 + 语料
# 生成（可能几分钟）之后才以晦涩的参数错误暴露，故先校验、快速失败。
# EN: FUZZ_SECS goes straight into libFuzzer's -max_total_time: a non-numeric value
# EN: would only surface as an obscure argument error after the nightly build + corpus
# EN: generation (possibly minutes), so validate first and fail fast.
case "$FUZZ_SECS" in
    ''|*[!0-9]*)
        echo "错误：FUZZ_SECS 必须为非负整数（当前：${FUZZ_SECS}）。" >&2
        exit 1
        ;;
esac
SEED_CORPUS="${SEED_CORPUS:-1}"
TARGETS="${TARGETS:-wal_v2_decode graph_snapshot_restore repl_frame_decode pgwire_message_decode}"

if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "错误：未找到 cargo-fuzz。安装：cargo install cargo-fuzz" >&2
    exit 1
fi
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    echo "错误：未安装 nightly 工具链（libFuzzer 需要 -Zsanitizer）。" >&2
    echo "安装：rustup toolchain install nightly" >&2
    exit 1
fi
# 不校验 nightly 的 rust-src 组件：本机 nightly（aarch64-apple-darwin）没有装
# rust-src，cargo-fuzz 仍能正常构建并运行 4 个 target（预编译 std + sanitizer 旗标
# 足够），加这道门只会把可用的环境挡在门外。缺 rust-src 若真成为前提，会在 cargo
# fuzz 构建阶段以明确错误暴露。
# EN: Do not validate nightly's rust-src component: this machine's nightly
# EN: (aarch64-apple-darwin) lacks rust-src, yet cargo-fuzz still builds and runs the
# EN: 4 targets (precompiled std + sanitizer flags suffice); this gate would only lock
# EN: out working environments. If missing rust-src really becomes a prerequisite, the
# EN: cargo fuzz build stage will expose it with a clear error.

# macOS 上 cargo-fuzz 必须显式给 --target（宿主三元组），否则拒绝运行。
# EN: On macOS, cargo-fuzz requires an explicit --target (host triple), otherwise it
# EN: refuses to run.
TARGET_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"

cd "$FUZZ_DIR"

if [ "$SEED_CORPUS" = "1" ]; then
    echo "── 生成语料种子 ────────────────────────────────"
    cargo +nightly run --quiet --example seed_corpus
fi

failed=""
for target in $TARGETS; do
    echo "── fuzz ${target}（${FUZZ_SECS}s）─────────────────"
    # 语料目录顺序即 libFuzzer 的写优先级：新单元只落第一个目录。故把**本地累积**
    # 目录放在前面（corpus-gen，gitignore），入库的确定性种子只读作起点——
    # 否则 60s 冒烟就能往入库目录里塞几千个文件，仓库随 nightly 无界膨胀。
    # EN: Corpus directory order is libFuzzer's write priority: new units go only to the
    # EN: first directory. So put the **local accumulation** directory first
    # EN: (corpus-gen, gitignored) and keep checked-in deterministic seeds as a read-only
    # EN: starting point — otherwise a 60s smoke run could stuff thousands of files into
    # EN: the checked-in directory and the repo would grow unbounded with every nightly.
    corpus_gen="$FUZZ_DIR/corpus-gen/$target"
    corpus_seed="$FUZZ_DIR/corpus/$target"
    mkdir -p "$corpus_gen"

    # 运行前快照：`artifacts/` 里**已入库**的历史崩溃样本是回归输入（见 fuzz/.gitignore），
    # 只报告本次新增的——否则一旦有样本入库，之后每轮干净运行都会被它判失败，仓库永远
    # 回不到绿，且报出的「应入库并转回归测试」早已完成。
    # EN: Snapshot before running: historical crash samples **already checked into**
    # EN: `artifacts/` are regression inputs (see fuzz/.gitignore); report only newly
    # EN: added ones — otherwise once a sample is checked in, every later clean run is
    # EN: judged failed and the repo never returns to green, while the reported "should
    # EN: be checked in and turned into a regression test" is long done.
    artifacts="$FUZZ_DIR/artifacts/$target"
    before="$(ls -1 "$artifacts" 2>/dev/null | sort || true)"

    # libFuzzer 命中 crash/OOM/timeout 时子进程非零退出（cargo-fuzz 透传），故此处
    # **不能** continue：否则「列出 artifacts → 入库转回归」这条专为崩溃而写的路径
    # 恰好不可达，且真实崩溃与构建错误无法区分。记下状态，继续走下面的 artifacts 检查。
    # EN: When libFuzzer hits crash/OOM/timeout the child exits non-zero (cargo-fuzz
    # EN: passes it through), so **must not** continue here: otherwise "list artifacts ->
    # EN: check in as regression", the path written specifically for crashes, is
    # EN: unreachable, and real crashes cannot be distinguished from build errors. Record
    # EN: the status and keep going to the artifacts check below.
    run_failed=0
    if ! cargo +nightly fuzz run --target "$TARGET_TRIPLE" "$target" \
        "$corpus_gen" "$corpus_seed" -- -max_total_time="$FUZZ_SECS" -print_final_stats=1; then
        run_failed=1
    fi

    if [ -n "$(ls -A "$corpus_gen" 2>/dev/null)" ]; then
        echo "  · cmin 最小化累积语料"
        # cmin 失败不得吞掉剩余 target：`set -e` 下不守卫会直接中止整轮。
        # EN: A cmin failure must not swallow the remaining targets: without a guard, set -e
        # EN: aborts the whole round.
        if ! cargo +nightly fuzz cmin --target "$TARGET_TRIPLE" "$target" "$corpus_gen" >/dev/null; then
            echo "  ! ${target} cmin 失败（语料未最小化，不影响本轮判定）" >&2
        fi
    fi

    # 只报告**本次新增**样本。libFuzzer 除 crash/oom/timeout 外还会产 `leak-*`
    # （libfuzzer-sys 默认开泄漏检测），一并纳入。
    # EN: Report **new** samples only. Besides crash/oom/timeout, libFuzzer also emits
    # EN: `leak-*` (libfuzzer-sys enables leak detection by default); include those too.
    new_artifacts="$(comm -13 <(printf '%s\n' "$before") \
        <(ls -1 "$artifacts" 2>/dev/null | sort || true) \
        | grep -E '^(crash|oom|timeout|leak)-' || true)"
    if [ -n "$new_artifacts" ]; then
        echo "  ✗ ${target} 产生崩溃/超时/泄漏样本（${artifacts}）——应入库并转回归测试" >&2
        printf '%s\n' "$new_artifacts" >&2
        failed="$failed $target"
    elif [ "$run_failed" -ne 0 ]; then
        echo "  ✗ ${target} 运行失败（见上方输出）" >&2
        failed="$failed $target"
    else
        echo "  ✓ ${target} 零崩溃"
    fi
done

if [ -n "$failed" ]; then
    echo "失败 target：$failed" >&2
    exit 1
fi
echo "全部 target 零崩溃。入库种子：$FUZZ_DIR/corpus/；本地累积：$FUZZ_DIR/corpus-gen/"
