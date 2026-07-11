#!/usr/bin/env bash
# Compiles the MCP server into a single self-contained executable with the Bun
# runtime embedded: the target host needs no Node, npm or node_modules.
#
# Usage (from anywhere inside the repo, after `pnpm install`):
#   examples/mcp-server/scripts/build-binary.sh
# Cross-compile:
#   TARGET=bun-linux-x64 examples/mcp-server/scripts/build-binary.sh
#   (targets: bun-linux-x64|bun-linux-arm64|bun-darwin-x64|bun-darwin-arm64)
# Output: examples/mcp-server/out/trailbase-mcp-<version>-<platform>
set -euo pipefail

PKG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${PKG_DIR}/out"

command -v bun >/dev/null || { echo "bun is required (https://bun.sh)" >&2; exit 1; }
command -v node >/dev/null || { echo "node is required for the MCP smoke test" >&2; exit 1; }

VERSION="$(node -p "require('${PKG_DIR}/package.json').version")"

TARGET_ARGS=()
SUFFIX="$(bun --print 'process.platform + "-" + process.arch')"
if [[ -n "${TARGET:-}" ]]; then
  TARGET_ARGS=(--target "${TARGET}")
  SUFFIX="${TARGET#bun-}"
fi

BINARY="${OUT_DIR}/trailbase-mcp-${VERSION}-${SUFFIX}"
mkdir -p "${OUT_DIR}"

# Compiling from src/index.ts (not dist/): the vite bundle keeps published
# dependencies external, while `bun build --compile` inlines everything.
echo "==> Compiling single executable (${TARGET:-host build})"
(cd "${PKG_DIR}" && bun build --compile src/index.ts "${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"}" --outfile "${BINARY}")

echo "==> Size: $(du -h "${BINARY}" | cut -f1)"

# Cross-compiled binaries cannot run on this machine; smoke host builds only.
if [[ -z "${TARGET:-}" ]]; then
  echo "==> Smoke: startup banner"
  banner="$(TRAILBASE_MODE=prod-safe timeout 5 "${BINARY}" </dev/null 2>&1 >/dev/null || true)"
  echo "    ${banner}"
  [[ "${banner}" == *"mode=prod-safe"* ]] || {
    echo "banner smoke failed" >&2
    exit 1
  }

  echo "==> Smoke: MCP handshake"
  node "${PKG_DIR}/scripts/smoke-binary.mjs" "${BINARY}"
fi

echo "==> Done: ${BINARY}"
echo "    Deploy: copy the file to the host and point .mcp.json at it."
