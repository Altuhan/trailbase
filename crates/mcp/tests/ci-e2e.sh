#!/usr/bin/env bash
# End-to-end driver for the embedded MCP server: builds a fresh throwaway
# depot (seed user + `note` table + two record APIs with different ACLs) and
# runs both live suites against the given `trail` binary — stdio
# (tests/e2e.mjs, includes the sandbox flow) and Streamable HTTP
# (tests/e2e-http.mjs).
#
# Usage (from the repo root, after `pnpm install` and a trail build):
#   crates/mcp/tests/ci-e2e.sh target/debug/trail
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TRAIL_BIN="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
TESTS_DIR="${REPO_ROOT}/crates/mcp/tests"

command -v node >/dev/null || { echo "node is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

DEPOT="$(mktemp -d -t trailbase-mcp-e2e-XXXXXX)"
trap 'rm -rf "${DEPOT}"' EXIT

echo "==> Preparing fresh depot at ${DEPOT}"
"${TRAIL_BIN}" --data-dir "${DEPOT}" user add seed@localhost seed-password-123

python3 - "${DEPOT}" <<'PYEOF'
import sqlite3
import sys

conn = sqlite3.connect(f"{sys.argv[1]}/data/main.db")
conn.execute(
    "CREATE TABLE note (id INTEGER PRIMARY KEY NOT NULL, body TEXT, secret TEXT) STRICT"
)
conn.commit()
PYEOF

cat >>"${DEPOT}/config.textproto" <<'CFGEOF'
record_apis: [
  {
    name: "note"
    table_name: "note"
    acl_authenticated: [CREATE, READ, UPDATE, DELETE]
  },
  {
    name: "note_ro"
    table_name: "note"
    acl_authenticated: [READ]
  }
]
CFGEOF

# The .mjs drivers resolve @modelcontextprotocol/sdk from the cwd.
cd "${REPO_ROOT}/examples/mcp-server"

echo "==> stdio e2e (records + guards + introspection + audit + sandbox)"
node "${TESTS_DIR}/e2e.mjs" "${TRAIL_BIN}" "${DEPOT}"

echo "==> streamable-http e2e (per-caller auth on /mcp)"
node "${TESTS_DIR}/e2e-http.mjs" "${TRAIL_BIN}" "${DEPOT}"

echo "==> MCP e2e: all green"
