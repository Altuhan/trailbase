# trailbase-mcp — MCP server inside `trail`

Two transports, one tool set, zero extra deployment:

- **stdio** (`trail mcp`) — for local/SSH use; tools act as one dedicated
  user logged in at startup.
- **Streamable HTTP** (`trail run --mcp`) — a `/mcp` endpoint on the running
  server; every tool call forwards the **caller's** `Authorization` header,
  so each MCP client acts under its own TrailBase account and ACLs.

## stdio

```bash
TRAIL_MCP_PASSWORD=… trail --data-dir ./traildepot mcp \
    --user agent@example.com --mode records --redact-columns password,token
```

```jsonc
// .mcp.json
{
  "mcpServers": {
    "trailbase": {
      "command": "trail",
      "args": ["--data-dir", "/path/to/traildepot", "mcp", "--user", "agent@example.com", "--mode", "records"],
      "env": { "TRAIL_MCP_PASSWORD": "…" },
    },
  },
}
```

## Remote (`/mcp` on the running server)

```bash
trail run --mcp --mcp-mode records \
    --mcp-redact-columns password,token \
    --mcp-allowed-hosts api.example.com   # required for non-loopback hosts
```

Connect from Claude Code with a TrailBase auth token (from
`/api/auth/v1/login` or a client library):

```bash
claude mcp add --transport http trailbase https://api.example.com/mcp \
    --header "Authorization: Bearer <auth token>"
```

Notes:

- Credentials are per request: the endpoint holds no tokens, and two clients
  connecting with different accounts get different ACLs — enforced by the
  same server code as any network request.
- The MCP handshake itself does not require a token; every data tool does
  (introspection tools like `schema_tables` respond without one). Guard
  state (budget, pending confirmations) is per MCP session.
- `--mcp-allowed-hosts` implements DNS-rebinding protection; loopback only
  by default.

## Why in-process

Tool calls are translated into requests against TrailBase's **real axum
router in-process** (`Server::init` builds the router; nothing is ever bound
to a socket). That means:

- **Server-enforced access.** The `--user` is logged in with the regular
  password flow at startup and every tool call carries that token through the
  same auth middleware and record API ACL checks as network traffic. A
  read-only API rejects agent writes with a real HTTP 403 — no MCP-side
  promises involved.
- **Zero extra deployment.** No Node/Bun runtime, no second artifact, no
  socket exposure; the MCP server ships inside the `trail` binary.
- **No token juggling.** No pre-issued or admin tokens in the environment —
  only the dedicated agent user's password (via the env var named by
  `--password-env`, default `TRAIL_MCP_PASSWORD`; never on the command line).

## Tools

| Tool | Modes | Notes |
| --- | --- | --- |
| `records_apis` | all | configured record APIs (name + table) |
| `records_schema` | all | JSON schema of an API's records |
| `schema_tables` | all | tables/views with CREATE statements (server-side metadata) |
| `instance_info` | all | version, data dir, record API count |
| `records_list` / `records_read` | all | reads, with column redaction applied |
| `records_create` / `records_update` / `records_delete` | `--mode records` | guarded mutations |
| `write_confirm` / `write_cancel` | `--mode records` | two-phase write flow |
| `auth_status` | all | mode, acting user, remaining write budget |

Write guards (ported from `examples/mcp-server`, design adapted from
applix-fr/mcp-trailbase): mutations are parked and only executed by
`write_confirm` (`--confirm-writes`, on by default; TTL
`--confirm-timeout-secs`), each process has a `--budget-writes` mutation
budget (0 = unlimited), and `--redact-columns` masks matching column values
in read results.

In `--mode read-only` (default) the mutation tools respond with a policy
error and nothing can be written at all.

## Audit

Because tool calls run through the real router, every `records_*` call is
logged to `_logs` exactly like a network request — method, URL, status,
latency and the acting user's id — and shows up in the admin UI's logs view.
No separate MCP audit trail to maintain.

## Relation to `examples/mcp-server`

The TypeScript server remains the full-featured option (admin tools and the
snapshot **sandbox** with migration-file diffs). This crate is the embedded,
zero-deployment path for record work against a live instance. Deployment
options across the repo: tarball / Docker / single Bun executable (see
`examples/mcp-server/README.md`) — or this subcommand, which needs nothing
but `trail` itself.

## Verification

Unit tests: `cargo test -p trailbase-mcp`. Live end-to-end (spawns the real
binary, drives park → confirm → redacted read → server-side 403 → cancel over
MCP): see `tests/e2e.mjs` header for setup; it borrows the MCP SDK from
`examples/mcp-server/node_modules`.

## Roadmap

- ~~Phase B: schema introspection and audit via `_logs`.~~ Done.
- ~~Phase C: Streamable HTTP endpoint `/mcp` with per-caller auth.~~ Done.
- Phase D: snapshot sandbox tools (today: use `examples/mcp-server`).
