#!/usr/bin/env bash
# 供应链体检本地入口（P4.8 / S9）：与 CI 的 security.yml 同口径。
# EN: local supply-chain gate matching .github/workflows/security.yml.
#
# 用法 / Usage:
#   ./scripts/security_audit.sh              # audit + deny + SBOM
#   SKIP_SBOM=1 ./scripts/security_audit.sh  # 跳过 SBOM（快速）
#   SBOM_OUT=/tmp/houyidb.cdx.json ./scripts/security_audit.sh
#
# 前置工具（缺一不可）：
#   cargo install cargo-audit cargo-deny cargo-cyclonedx --locked
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

missing=()
for tool in cargo-audit cargo-deny; do
  command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [[ "${SKIP_SBOM:-0}" != "1" ]]; then
  command -v cargo-cyclonedx >/dev/null 2>&1 || missing+=("cargo-cyclonedx")
fi
if ((${#missing[@]} > 0)); then
  echo "缺少工具: ${missing[*]}" >&2
  echo "安装: cargo install cargo-audit cargo-deny cargo-cyclonedx --locked" >&2
  exit 2
fi

echo "==> cargo audit (RustSec)"
cargo audit

echo "==> cargo deny check (licenses / bans / sources)"
cargo deny --all-features check

if [[ "${SKIP_SBOM:-0}" != "1" ]]; then
  out="${SBOM_OUT:-target/sbom/houyidb.cdx.json}"
  mkdir -p "$(dirname "$out")"
  echo "==> CycloneDX SBOM -> $out"
  # cargo-cyclonedx 固定写到包目录，再移动到目标路径。
  # EN: cargo-cyclonedx always writes to the package directory; move it to the target
  # EN: path afterwards.
  cargo cyclonedx --manifest-path houyi-server/Cargo.toml \
    --format json --override-filename houyidb.cdx
  mv houyi-server/houyidb.cdx.json "$out"
  # 该工具会在各 workspace 成员目录留下同名文件；只保留归档的那一份。
  # EN: The tool leaves same-named files in each workspace member directory; keep only
  # EN: the archived copy.
  find . -name 'houyidb.cdx.json' -not -path './target/*' -delete >/dev/null
fi

echo "供应链体检通过。"
