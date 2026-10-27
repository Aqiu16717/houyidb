#!/usr/bin/env bash
# 社区版边界门禁（P4.9 开源治理）。确保公共仓库不引用企业版代码/私有源。
# EN: enforce the open-core boundary in the public repository.
#
# 检查项：
#   1. 代码/构建文件不得引用 houyidb-enterprise / 私有 registry / 私有 CI 密钥；
#   2. 不得出现 ee/ 或 enterprise/ 专有代码目录；
#   3. 工作区许可证必须是 Apache-2.0，LICENSE/NOTICE/LICENSING.md 必须存在；
#   4. 依赖源只允许 crates.io（deny.toml 的第二道防线，这里做快速失败）。
#
# 用法：./scripts/community_boundary_check.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0
err() { echo "!! boundary: $*" >&2; fail=1; }

# 1) 私有仓库 / 私有 registry / 私有凭据引用（只查代码与构建面，文档可讨论）。
# EN: 1) Private repo / private registry / private credential references (code and
# EN: build surface only; docs may discuss them).
pattern='houyidb-enterprise|houyi-enterprise|houyidb_enterprise|houyi_enterprise|registry\.internal|private\.registry|HOUYI_ENTERPRISE_TOKEN'
hits=$(git grep -n -I -E "$pattern" -- \
  '*.rs' '*.toml' '*.yml' '*.yaml' 'Cargo.lock' 'Dockerfile' 'scripts/*' \
  ':!docs/*' ':!LICENSING.md' ':!CONTRIBUTING.md' ':!SECURITY.md' \
  ':!CLA.md' ':!TRADEMARK.md' \
  ':!scripts/community_boundary_check.sh' || true)
if [[ -n "$hits" ]]; then
  echo "$hits" >&2
  err "public code references enterprise/private sources"
fi

# 2) 专有目录不得进入公共仓库。
# EN: 2) Proprietary directories must not enter the public repository.
dirs=$(git ls-files | grep -E '^(ee|enterprise)/' || true)
if [[ -n "$dirs" ]]; then
  echo "$dirs" >&2
  err "proprietary directories (ee/ or enterprise/) must live in the private repo"
fi

# 3) 许可证一致性。
# EN: 3) License consistency.
if ! grep -qE '^license[[:space:]]*=[[:space:]]*"Apache-2\.0"' Cargo.toml; then
  err "workspace Cargo.toml must declare license = \"Apache-2.0\""
fi
for f in LICENSE NOTICE LICENSING.md; do
  [[ -f "$f" ]] || err "missing $f"
done
if ! head -3 LICENSE | grep -q 'Apache License'; then
  err "LICENSE is not the Apache-2.0 text"
fi

# 4) 依赖源快速失败（git/私有 registry 依赖；完整策略由 cargo-deny 负责）。
# Cargo.lock 的正常源：registry+https://github.com/rust-lang/crates.io-index
# 或 sparse+https://index.crates.io/；其余 source 行一律拒绝。
# EN: 4) Dependency source fast fail (git/private registry deps; the full policy is
# EN: handled by cargo-deny). Normal Cargo.lock sources:
# EN: registry+https://github.com/rust-lang/crates.io-index or
# EN: sparse+https://index.crates.io/; reject any other source line.
bad=$(grep -nE '^source = "(git\+|registry\+|sparse\+)' Cargo.lock \
  | grep -vE 'source = "(registry\+https://github\.com/rust-lang/crates\.io-index|sparse\+https://index\.crates\.io/)"$' \
  || true)
if [[ -n "$bad" ]]; then
  echo "$bad" >&2
  err "non-crates.io dependency source in Cargo.lock"
fi

if (( fail != 0 )); then
  echo "社区版边界检查失败。" >&2
  exit 1
fi
echo "社区版边界检查通过：无企业依赖、无专有目录、许可证一致。"
