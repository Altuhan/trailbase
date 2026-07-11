#!/usr/bin/env bash
# Packages the MCP server into a self-contained tarball: bundled dist/index.js
# plus plain-npm production node_modules. The target host only needs Node 22+
# (no pnpm, no repo checkout).
#
# Usage (from anywhere inside the repo, after `pnpm install`):
#   examples/mcp-server/scripts/package.sh
# Output: examples/mcp-server/out/trailbase-mcp-<version>.tar.gz
set -euo pipefail

PKG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${PKG_DIR}/out"
STAGE="${OUT_DIR}/trailbase-mcp"

command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
command -v pnpm >/dev/null || { echo "pnpm is required to build" >&2; exit 1; }

VERSION="$(node -p "require('${PKG_DIR}/package.json').version")"

echo "==> Building bundle (vite)"
pnpm -C "${PKG_DIR}" build

echo "==> Staging"
rm -rf "${STAGE}"
mkdir -p "${STAGE}"
cp "${PKG_DIR}/dist/index.js" "${STAGE}/index.js"
cp "${PKG_DIR}/README.md" "${STAGE}/README.md"
cp -r "${PKG_DIR}/skills" "${STAGE}/skills"

# Minimal manifest with the bundle's external runtime dependencies, pinned to
# the exact versions the bundle was built and tested against.
node - "$PKG_DIR" "$STAGE" "$VERSION" <<'EOF'
const fs = require("node:fs");
const path = require("node:path");
const [pkgDir, stage, version] = process.argv.slice(2);

const externals = [
  "@bufbuild/protobuf",
  "@modelcontextprotocol/sdk",
  "nano-spawn",
  "zod",
];
const dependencies = {};
for (const name of externals) {
  const manifest = path.join(pkgDir, "node_modules", name, "package.json");
  dependencies[name] = JSON.parse(fs.readFileSync(manifest, "utf8")).version;
}

fs.writeFileSync(
  path.join(stage, "package.json"),
  JSON.stringify(
    {
      name: "trailbase-mcp",
      version,
      private: true,
      type: "module",
      engines: { node: ">=22" },
      dependencies,
    },
    null,
    2,
  ) + "\n",
);
EOF

echo "==> Installing production dependencies into the stage (plain npm)"
(cd "${STAGE}" && npm install --omit=dev --no-audit --no-fund --loglevel=error)

echo "==> Creating tarball"
TARBALL="${OUT_DIR}/trailbase-mcp-${VERSION}.tar.gz"
tar -czf "${TARBALL}" -C "${OUT_DIR}" trailbase-mcp

echo "==> Done: ${TARBALL}"
echo "    Deploy: tar -xzf $(basename "${TARBALL}") && node trailbase-mcp/index.js"
