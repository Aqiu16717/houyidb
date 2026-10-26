#!/usr/bin/env bash
# 构建全部 SDK 发布产物到 dist/sdk（不上传）。
# Build all SDK release artifacts into dist/sdk (no upload).
#
# 依赖 / Requires:
#   python3 -m pip install build twine
#   npm, dotnet, gradle, cargo
#
# 用法 / Usage:
#   ./scripts/package_sdks.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$ROOT/dist/sdk}"
cd "$ROOT"
rm -rf "$OUT"
mkdir -p "$OUT"

echo "== Python"
for d in sdk/python/houyidb sdk/python/langchain-houyidb sdk/python/llama-index-vector-stores-houyidb; do
  ( cd "$d" && python3 -m build --outdir "$OUT" )
done
python3 -m twine check "$OUT"/*.whl "$OUT"/*.tar.gz

echo "== TypeScript"
( cd sdk/typescript/houyidb && npm ci && npm run build && npm pack --pack-destination "$OUT" )

echo "== .NET"
( cd sdk/dotnet && dotnet test --nologo -v q && dotnet pack src/HouyiDb/HouyiDb.csproj -c Release -o "$OUT" )

echo "== Java (local Maven repo; Central needs signing + manual upload)"
( cd sdk/java/houyidb && gradle --offline test build publishAllPublicationsToLocalBuildRepository )
cp sdk/java/houyidb/build/libs/*.jar "$OUT"/

echo "== Rust"
cargo package -p houyidb-client
cp target/package/houyidb-client-*.crate "$OUT"/

echo "== Go (tag instead of package artifact)"
echo "   after release: git tag sdk/go/houyidb/v\$VERSION && git push origin sdk/go/houyidb/v\$VERSION"

echo
echo "artifacts: $OUT"
ls -lh "$OUT"
