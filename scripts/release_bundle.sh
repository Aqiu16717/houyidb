#!/usr/bin/env bash
# 构建本地发布包（不推送、不建远端 release）：
#   dist/houyidb-<version>-<date>-<os>-<arch>.tar.gz   预编译二进制包
#   dist/houyidb-<version>-src.tar.gz                 源码包（含 Dockerfile/全部 SDK）
#   dist/SHA256SUMS                                   校验和（多平台增量维护）
#
# 用法：
#   ./scripts/release_bundle.sh                         # 当前机器原生平台
#   TARGET=x86_64-apple-darwin ./scripts/release_bundle.sh   # 交叉编译 Intel Mac
#   SKIP_BUILD=1 TARGET=... ./scripts/release_bundle.sh # 复用已有二进制
#
# 前置：已跟踪文件无未提交改动（源码包来自 git archive HEAD）；交叉编译需先
# `rustup target add <triple>`。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
COMMIT="$(git rev-parse --short HEAD)"
DATE="$(date +%Y%m%d)"
TARGET="${TARGET:-}"

if [[ -n "$TARGET" ]]; then
  BIN="target/${TARGET}/release/houyi-server"
  case "$TARGET" in
    *-apple-darwin) OS="darwin" ;;
    *-linux-*) OS="linux" ;;
    *) OS="$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
  esac
  case "$TARGET" in
    aarch64-* | arm64-*) ARCH="arm64" ;;
    x86_64-* | amd64-*) ARCH="x86_64" ;;
    *) ARCH="${TARGET%%-*}" ;;
  esac
else
  BIN="target/release/houyi-server"
  OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
  case "$(uname -m)" in
    arm64 | aarch64) ARCH="arm64" ;;
    x86_64 | amd64) ARCH="x86_64" ;;
    *) ARCH="$(uname -m)" ;;
  esac
  case "$OS" in
    darwin) OS="darwin" ;;
    linux) OS="linux" ;;
  esac
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "!! 有未提交的已跟踪改动：源码包来自 HEAD，会与二进制不一致。" >&2
  git status --short >&2
  exit 1
fi

NAME="houyidb-${VERSION}-${DATE}-${OS}-${ARCH}"
STAGE="dist/${NAME}"
SRC_NAME="houyidb-${VERSION}-src"

echo "== 版本 ${VERSION}（${COMMIT}），平台 ${OS}-${ARCH}${TARGET:+，target ${TARGET}}"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "== 构建 release 二进制"
  if [[ -n "$TARGET" ]]; then
    cargo build --release -p houyi-server --target "$TARGET"
  else
    cargo build --release -p houyi-server
  fi
fi
[[ -x "$BIN" ]] || {
  echo "!! 缺少 ${BIN}（交叉编译请先 rustup target add ${TARGET:-<triple>}）" >&2
  exit 1
}

echo "== 组装 ${STAGE}"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/scripts" "$STAGE/docs" "$STAGE/sdk" "$STAGE/examples"

cp "$BIN" "$STAGE/bin/houyi-server"
cp docs/USER_MANUAL.md README.md CHANGELOG.md LICENSE NOTICE "$STAGE/"
cp docs/HOUYIDB_SERVER_MANUAL.md docs/DEPLOYMENT_GUIDE.md docs/EMBEDDING_GUIDE.md "$STAGE/docs/"
cp scripts/trial_start.sh scripts/trial_smoke.sh scripts/embed_smoke.sh "$STAGE/scripts/"
cp scripts/mock_embedding_server.py "$STAGE/scripts/"
cp examples/text_ingest.py "$STAGE/examples/"
chmod +x "$STAGE/bin/houyi-server" "$STAGE/scripts/"*.sh "$STAGE/scripts/"*.py "$STAGE/examples/"*.py
cp -R sdk/python "$STAGE/sdk/python"
find "$STAGE/sdk" -type d \( -name __pycache__ -o -name '*.egg-info' \) -prune -exec rm -rf {} + 2>/dev/null || true
cat >"$STAGE/VERSION" <<EOF
version=$VERSION
commit=$COMMIT
built=$DATE
platform=${OS}-${ARCH}
target=${TARGET:-native}
EOF

echo "== 打包二进制包"
mkdir -p dist
tar -C dist -czf "dist/${NAME}.tar.gz" "$NAME"

echo "== 打包源码包（git archive HEAD）"
git archive --format=tar.gz --prefix="${SRC_NAME}/" -o "dist/${SRC_NAME}.tar.gz" HEAD

echo "== 校验和（多平台增量维护）"
SUMS_FILE="dist/SHA256SUMS"
touch "$SUMS_FILE"
TMP_SUMS="$(mktemp)"
# 先移除本次产物的旧行；grep 无匹配时退出 1，用 || true 兜底。
# EN: Remove the previous line for this artifact first; grep exits 1 on no match, so
# EN: fall back with || true.
grep -v -E "  (${NAME}\.tar\.gz|${SRC_NAME}\.tar\.gz)$" "$SUMS_FILE" >"$TMP_SUMS" || true
if command -v shasum >/dev/null 2>&1; then
  ( cd dist && shasum -a 256 "${NAME}.tar.gz" "${SRC_NAME}.tar.gz" ) >>"$TMP_SUMS"
else
  ( cd dist && sha256sum "${NAME}.tar.gz" "${SRC_NAME}.tar.gz" ) >>"$TMP_SUMS"
fi
LC_ALL=C sort -k2 "$TMP_SUMS" -o "$SUMS_FILE"
rm -f "$TMP_SUMS"

echo
echo "完成："
ls -lh "dist/${NAME}.tar.gz" "dist/${SRC_NAME}.tar.gz" dist/SHA256SUMS
echo
cat dist/SHA256SUMS
