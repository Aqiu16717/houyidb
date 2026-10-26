#!/usr/bin/env bash
# 社区仓阶段性发布（方案 A：私有开发仓 → 干净公共快照仓）。
#
# 作用：从私有仓的某个 ref（建议用发布 tag）导出一棵**只含已跟踪文件**的白名单
# 快照，做密钥/隐私/开源边界检查，然后在目标目录里生成一个「阶段提交」并打 tag。
# **绝不 push**——远端创建/推送由人确认后执行。
#
# 用法：
#   ./scripts/publish_community.sh --ref v0.2.0-beta.5 --out /path/to/houyidb-community
#   ./scripts/publish_community.sh --ref v0.2.0-beta.5 --dry-run
#   ./scripts/publish_community.sh --ref <sha> --out ... --exclude docs/verification
#
# 首次运行：--out 目录为空时自动 git init；后续阶段：--out 指向已有公共仓 clone，
# 脚本会用快照**整体替换**其工作树（保留 .git 历史），再生成阶段提交。
# 注意：公共仓里直接改的文件会被覆盖；公共 PR 应先合入私有仓，再随阶段快照发布。
#
# 可配置：
#   PUBLISH_EXCLUDES="path1 path2"   额外排除（空格分隔，相对仓库根）
#   --exclude <path>                 同上，可重复
#   --skip-boundary                  跳过 community_boundary_check（不推荐）
#   --message-file <file>            用自定义文字替换 CHANGELOG 生成的提交正文
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

REF=""
OUT=""
DRY_RUN=0
SKIP_BOUNDARY=0
MESSAGE_FILE=""
EXTRA_EXCLUDES=()

# 默认不进入公共仓的文件（内部协作/规划/商业边界文档）。未跟踪文件永远
# 不会被 git archive 导出；新增内部文档时同步补到这里。
# EN: internal planning/tracking docs excluded from the public snapshot.
DEFAULT_EXCLUDES=(
  "CLAUDE.md"
  "WHITEPAPER_TRACKING.md"
  "ROADMAP.md"
  "TODO.md"
  "bench-baseline.md"
  "dev-log.md"
  "houyidb.md"
  "INCREMENTAL_WORKLOAD.md"
  "PARALLEL_IMPLEMENTATION_PLAN.md"
  "docs/OPEN_SOURCE_PLAN.md"
  "docs/ENTERPRISE_READINESS.md"
  "docs/PROJECT_TIMELINE.md"
  "docs/ROADMAP.md"
  "docs/ADBC_INTERFACE_PLAN.md"
  "docs/PGWIRE_BINARY_ROADMAP.md"
  "docs/VECTOR_WAL_REPLAY_CONTRACT.md"
  "docs/verification"
  "docs/HII_LSM_ARCHITECTURE.md"
  "bench-results"
  "houyi-vector/HNSW_ENGINE_ROADMAP.md"
  "deploy/macos"
  "deploy/python"
)

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ref) REF="${2:?--ref needs a value}"; shift 2 ;;
    --out) OUT="${2:?--out needs a value}"; shift 2 ;;
    --exclude) EXTRA_EXCLUDES+=("${2:?--exclude needs a value}"); shift 2 ;;
    --message-file) MESSAGE_FILE="${2:?--message-file needs a value}"; shift 2 ;;
    --skip-boundary) SKIP_BOUNDARY=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h | --help) usage 0 ;;
    *) echo "!! unknown argument: $1" >&2; usage 1 ;;
  esac
done

[[ -n "$REF" ]] || { echo "!! --ref is required (e.g. v0.2.0-beta.5)" >&2; exit 1; }
COMMIT="$(git rev-parse --verify "$REF^{commit}" 2>/dev/null)" || {
  echo "!! cannot resolve ref: $REF" >&2
  exit 1
}
SHORT="$(git rev-parse --short "$COMMIT")"
VERSION="$(git show "$COMMIT:Cargo.toml" | grep -m1 '^version = ' | cut -d'"' -f2 || true)"
if TAG="$(git describe --tags --exact-match "$COMMIT" 2>/dev/null)"; then
  LABEL="$TAG"
else
  LABEL="snapshot-${VERSION:-unknown}-$SHORT"
fi
[[ -n "$OUT" ]] || OUT="$ROOT/dist/community-$LABEL"

echo "== 社区快照"
echo "   ref     : $REF"
echo "   commit  : $COMMIT"
echo "   version : ${VERSION:-<unknown>}"
echo "   label   : $LABEL"
echo "   out     : $OUT"

# 仅导出已跟踪文件；历史/未跟踪的规划文档天然不会进入快照。
# EN: Export tracked files only; historical/untracked planning docs naturally never
# EN: enter the snapshot.
TREE="$(mktemp -d)"
cleanup() { rm -rf "$TREE"; }
trap cleanup EXIT
git archive --format=tar "$COMMIT" | tar -x -C "$TREE"

EXCLUDED=()
for p in "${DEFAULT_EXCLUDES[@]}" ${PUBLISH_EXCLUDES:-} "${EXTRA_EXCLUDES[@]+"${EXTRA_EXCLUDES[@]}"}"; do
  [[ -n "$p" ]] || continue
  case "$p" in
    /* | *".."*) echo "!! refusing unsafe exclude path: $p" >&2; exit 1 ;;
  esac
  if [[ -e "$TREE/$p" ]]; then
    rm -rf "$TREE/$p"
    EXCLUDED+=("$p")
  fi
done
echo "== 已排除（相对仓库根）：${EXCLUDED[*]:-<none>}"
echo "== 未跟踪文件不会进入快照（如本地草稿/PPTX/PDF）。"

echo "== 密钥/隐私扫描"
python3 - "$TREE" <<'PY'
import os, re, sys

root = sys.argv[1]
# 高置信度模式：命中即失败（占位符如 sk-... 不会命中 20+ 字符规则）。
# EN: High-confidence patterns: a hit fails immediately (placeholders like sk-... do
# EN: not match the 20+ char rule).
patterns = {
    "private key": re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----"),
    "aws access key": re.compile(r"AKIA[0-9A-Z]{16}"),
    "github token": re.compile(r"gh[pousr]_[A-Za-z0-9]{30,}"),
    "github pat": re.compile(r"github_pat_[A-Za-z0-9_]{22,}"),
    "openai key": re.compile(r"sk-[A-Za-z0-9]{20,}"),
    "slack token": re.compile(r"xox[baprs]-[A-Za-z0-9-]{10,}"),
    "personal mac path": re.compile(r"/Users/[A-Za-z0-9._-]+/"),
}
skip_dirs = {".git"}
hits = []
for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = [d for d in dirnames if d not in skip_dirs]
    for name in filenames:
        path = os.path.join(dirpath, name)
        try:
            if os.path.getsize(path) > 2 * 1024 * 1024:
                continue
            with open(path, "r", encoding="utf-8", errors="ignore") as fh:
                for lineno, line in enumerate(fh, 1):
                    for label, rx in patterns.items():
                        if rx.search(line):
                            hits.append((label, os.path.relpath(path, root), lineno, line.strip()[:160]))
        except OSError:
            continue
if hits:
    print("!! 发现疑似敏感内容：", file=sys.stderr)
    for label, path, lineno, text in hits:
        print(f"   [{label}] {path}:{lineno}: {text}", file=sys.stderr)
    sys.exit(1)
print("   通过：未发现高置信度密钥/个人路径。")
PY

if [[ "$DRY_RUN" == "1" ]]; then
  echo "== dry-run：跳过目标目录初始化/提交。"
  echo "   文件数：$(find "$TREE" -type f | wc -l | tr -d ' ')"
  exit 0
fi

# 目标目录：首次为空则 init；已有 .git 则做「整体替换 + 新阶段提交」。
# EN: Target directory: init if empty on first use; if .git already exists, do
# EN: "full replacement + new-stage commit".
if [[ -d "$OUT/.git" ]]; then
  APPEND=1
else
  if [[ -e "$OUT" && -n "$(ls -A "$OUT" 2>/dev/null || true)" ]]; then
    echo "!! $OUT 非空且不是 git 仓库；请换目录或清理后重试。" >&2
    exit 1
  fi
  mkdir -p "$OUT"
  git -C "$OUT" init -q -b main
  APPEND=0
fi
git -C "$OUT" config user.name "$(git -C "$ROOT" config user.name || echo 'HouyiDB Community')"
git -C "$OUT" config user.email "$(git -C "$ROOT" config user.email || echo 'community@localhost')"

echo "== 同步快照到 $OUT"
rsync -a --delete --exclude '.git/' "$TREE/" "$OUT/"
# -f：个别被用户全局 gitignore 命中的已跟踪文件（如 **/.dockerignore）也要进
# 公共快照；树本身来自 git archive，不存在需要忽略的构建产物。
# EN: -f: also include tracked files hit by the user's global gitignore (e.g.
# EN: **/.dockerignore) in the public snapshot; the tree itself comes from git
# EN: archive, so there are no build artifacts to ignore.
git -C "$OUT" add -A -f

EXPECTED_FILES="$(find "$TREE" \( -type f -o -type l \) | wc -l | tr -d ' ')"
STAGED_FILES="$(git -C "$OUT" ls-files --cached | wc -l | tr -d ' ')"
if [[ "$EXPECTED_FILES" != "$STAGED_FILES" ]]; then
  echo "!! 快照文件数与暂存数不一致（$EXPECTED_FILES vs $STAGED_FILES）：请检查忽略规则。" >&2
  exit 1
fi

if [[ "$SKIP_BOUNDARY" != "1" ]]; then
  echo "== 开源边界检查（在快照树上执行）"
  ( cd "$OUT" && bash scripts/community_boundary_check.sh )
fi

if git -C "$OUT" diff --cached --quiet; then
  echo "== 快照与当前公共分支一致，无需提交。"
  exit 0
fi

if [[ -n "$MESSAGE_FILE" ]]; then
  BODY_FILE="$MESSAGE_FILE"
else
  BODY_FILE="$(mktemp)"
  python3 - "$TREE/CHANGELOG.md" "$LABEL" "$VERSION" >"$BODY_FILE" <<'PY'
import re, sys
path, label, version = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    lines = open(path, encoding="utf-8").read().splitlines()
except OSError:
    sys.exit(0)

def norm(s: str) -> str:
    return s.strip().lstrip("v")

targets = {norm(label), norm(version or "")}
start = None
for i, line in enumerate(lines):
    if line.startswith("## ") and norm(line[3:].split()[0] if line[3:].split() else "") in targets:
        start = i
        break
if start is None:
    sys.exit(0)
out = []
for line in lines[start + 1:]:
    if line.startswith("## "):
        break
    out.append(line)
print("\n".join(out).strip())
PY
fi

SUBJECT="release: $LABEL"
if [[ -s "$BODY_FILE" ]]; then
  git -C "$OUT" commit -q -m "$SUBJECT" -m "$(cat "$BODY_FILE")"
else
  git -C "$OUT" commit -q -m "$SUBJECT"
fi
[[ "$BODY_FILE" != "$MESSAGE_FILE" ]] && rm -f "$BODY_FILE" || true

TAG_CREATED=""
if [[ "$LABEL" == v* ]] && ! git -C "$OUT" rev-parse -q --verify "refs/tags/$LABEL" >/dev/null; then
  git -C "$OUT" tag -a "$LABEL" -m "HouyiDB $LABEL"
  TAG_CREATED="$LABEL"
fi

echo
echo "== 完成（未 push）"
echo "   目录   : $OUT"
echo "   提交   : $(git -C "$OUT" log -1 --format='%h %s')"
[[ -n "$TAG_CREATED" ]] && echo "   标签   : $TAG_CREATED"
echo "   文件数 : $(git -C "$OUT" ls-files | wc -l | tr -d ' ')"
echo
echo "下一步（确认内容无误后）：
  cd \"$OUT\"
  git remote add origin git@github.com:<org>/<repo>.git   # 若尚未添加
  git push -u origin main
  [[ -n \"$TAG_CREATED\" ]] && git push origin \"$TAG_CREATED\"
"
