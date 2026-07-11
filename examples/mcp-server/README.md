# TrailBase MCP Server

An [MCP](https://modelcontextprotocol.io) (Model Context Protocol) server that
gives an AI agent (Claude Code, Claude Desktop, …) **safe, mode-gated access**
to a TrailBase instance — and a **production sandbox**: an ephemeral copy of
your live data the agent can freely experiment against, handing back reviewable
migration files instead of touching production.

It talks to a stock TrailBase server over its normal HTTP APIs; no core
changes required.

## Why this shape

TrailBase's only server-enforced privilege boundary is the **Record API** (per
API ACLs plus row-level SQL rules). The admin API is all-or-nothing and its SQL
endpoint always runs on the writer connection. So the safe design is:

- **Read/write real data** only through Record APIs, as a dedicated **non-admin
  user** — the server enforces exactly what that user may see and change.
- **Schema work and arbitrary SQL** only inside a **throwaway sandbox** spun up
  from a snapshot of production. Changes there are recorded as migration files
  you review and apply through your normal deploy.

## Access modes

Set `TRAILBASE_MODE`:

| Mode                  | Tools exposed                                                                  | Enforcement                                                             |
| --------------------- | ------------------------------------------------------------------------------ | ----------------------------------------------------------------------- |
| `prod-safe` (default) | `records_*`, `auth_status`, `sandbox_*`                                        | **Server-side** Record API ACLs for the configured user                 |
| `prod-admin-readonly` | + `admin_tables`, `admin_config_get`, `admin_logs`, `admin_jobs`, `admin_info` | Read-only by **MCP policy** (not a server guarantee — see Security)     |
| `sandbox`             | + `admin_query`, `admin_config_set`, `schema_*`                                | All data/admin tools target the **ephemeral sandbox**, never production |

In `sandbox` mode the configured `TRAILBASE_URL`/`TRAILBASE_DATA_DIR` is used
only as the snapshot source; every records/admin/schema call is routed to the
sandbox instance created with `sandbox_create`.

## Tools

**Records** (all modes): `records_list`, `records_read`, `records_create`,
`records_update`, `records_delete`, `records_schema`, `auth_status`; in prod
modes with confirmation enabled also `write_confirm` / `write_cancel` (see
_Write guards_).

**Admin read** (`prod-admin-readonly`, `sandbox`): `admin_tables`,
`admin_config_get`, `admin_logs`, `admin_jobs`, `admin_info`.

**Admin write** (`sandbox` only): `admin_query` (arbitrary SQL), `admin_config_set`,
and `schema_create_table` / `schema_alter_table` / `schema_drop_table` /
`schema_create_index` / `schema_drop_index`. Schema tools go through the admin
DDL endpoints, so each change is written as a `U<timestamp>__*.sql` migration
file — the artifact you review.

**Sandbox management** (all modes): `sandbox_create`, `sandbox_status`,
`sandbox_diff`, `sandbox_destroy`.

## Configuration (environment)

| Variable                                           | Purpose                                                                             |
| -------------------------------------------------- | ----------------------------------------------------------------------------------- |
| `TRAILBASE_URL`                                    | Instance base URL (default `http://localhost:4000`)                                 |
| `TRAILBASE_MODE`                                   | `prod-safe` \| `prod-admin-readonly` \| `sandbox`                                   |
| `TRAILBASE_USER` / `TRAILBASE_PASSWORD`            | Record-API login for the dedicated agent user                                       |
| `TRAILBASE_AUTH_TOKEN` / `TRAILBASE_REFRESH_TOKEN` | Pre-issued tokens (alternative to login)                                            |
| `TRAILBASE_ADMIN_TOKEN`                            | Admin token for admin-read tools in prod modes                                      |
| `TRAILBASE_DATA_DIR`                               | Path to the live depot (`traildepot`), required for `sandbox_create`                |
| `TRAIL_BIN`                                        | `trail` binary used to run sandbox instances (default `trail`)                      |
| `TRAILBASE_BUDGET_WRITES`                          | Record mutations allowed per session in prod modes (default `100`, `0` = unlimited) |
| `TRAILBASE_REDACT_COLUMNS`                         | Comma-separated case-insensitive regexes; matching column names are masked in reads |
| `TRAILBASE_CONFIRM_WRITES`                         | Two-phase writes (`true`/`false`; default on in prod modes, off in `sandbox`)       |
| `TRAILBASE_CONFIRM_TIMEOUT_SECS`                   | Seconds until an unconfirmed write expires (default `120`)                          |

Create the dedicated non-admin user once:

```bash
trail user add claude-agent@example.com "$(openssl rand -base64 24)"
```

then grant it access only to the specific Record APIs it needs (with the
narrowest ACL flags, optionally row-level rules referencing `_USER_.id`).

## Use with Claude Code

Build once (`pnpm i && pnpm -C examples/mcp-server build`), then add the server:

```jsonc
// .mcp.json
{
  "mcpServers": {
    "trailbase": {
      "command": "node",
      "args": ["examples/mcp-server/dist/index.js"],
      "env": {
        "TRAILBASE_URL": "http://localhost:4000",
        "TRAILBASE_MODE": "prod-safe",
        "TRAILBASE_USER": "claude-agent@example.com",
        "TRAILBASE_PASSWORD": "…",
      },
    },
  },
}
```

Or: `claude mcp add trailbase -- node examples/mcp-server/dist/index.js`.

The sandbox workflow is documented as a skill in
[`skills/trailbase-sandbox.md`](skills/trailbase-sandbox.md) — copy it into
`.claude/skills/` to teach the agent the create → change → diff → review → destroy
loop.

## Write guards (prod modes)

Mutations against a live instance get three extra layers, adapted from the
guard design of [applix-fr/mcp-trailbase](https://github.com/applix-fr/mcp-trailbase):

- **Two-phase confirmation** (default on in prod modes):
  `records_create/update/delete` don't execute — they park the mutation and
  return a `pending_id` plus a human-readable summary. Nothing is written
  until `write_confirm`; `write_cancel` discards, and unconfirmed writes
  expire after `TRAILBASE_CONFIRM_TIMEOUT_SECS`. This puts every mutation
  intent on the record (transcript + audit log) before it happens.
- **Write budget**: at most `TRAILBASE_BUDGET_WRITES` mutations per server
  process; when exhausted, further writes fail with instructions to have a
  human raise the limit. `auth_status` reports the remaining budget.
- **Column redaction**: values of columns matching `TRAILBASE_REDACT_COLUMNS`
  (e.g. `password,token,.*_secret`) come back as `"[REDACTED]"` from
  `records_list`/`records_read`, including rows expanded through foreign keys.

Sandbox instances are disposable snapshots, so none of this applies in
`sandbox` mode.

## Security model

- **Live production access is only ever the Record API of a non-admin user.**
  Give that user the minimum ACLs; the server enforces them.
- **`prod-admin-readonly` is MCP-side policy, not a server guarantee.** The
  admin token it uses is technically full-access; the server cannot restrict it
  to reads. For real production, bind the admin API to a separate port
  (`--admin-address`) behind a firewall and prefer the sandbox for anything
  mutating.
- **The sandbox is the only place arbitrary SQL/DDL runs**, and those tools are
  wired to the sandbox instance, so they _cannot_ reach the production URL even
  by misconfiguration. It listens on localhost only, gets a random admin
  password (kept in memory, never written to disk) and fresh signing keys
  (production tokens don't work in it).
- Every tool call is written as one JSON line to stderr (audit log). Tokens are
  read from the environment and never persisted.

## Topology & limitations

`sandbox_create` needs filesystem access to the depot and the `trail` binary,
so it works when the MCP server runs **on the same host** as the instance
(dev/staging, or a single prod box). For a fully remote instance, the record
and admin-read tools work over HTTP but the snapshot sandbox does not; take a
snapshot out of band (the admin `Backup` job writes `backups/backup.db`) and
point a local `TRAILBASE_DATA_DIR` at it.

## Deployment (self-contained artifact)

For hosts without pnpm or a repo checkout, package the server into a tarball
that only needs Node 22+:

```bash
examples/mcp-server/scripts/package.sh
# -> examples/mcp-server/out/trailbase-mcp-<version>.tar.gz
```

On the target host:

```bash
tar -xzf trailbase-mcp-<version>.tar.gz
node trailbase-mcp/index.js   # point .mcp.json's args at this path
```

Alternatively build a container image (from the repository root):

```bash
docker build -f examples/mcp-server/Dockerfile -t trailbase-mcp .
claude mcp add trailbase -- docker run -i --rm \
  -e TRAILBASE_URL=https://your-instance.example \
  -e TRAILBASE_MODE=prod-safe \
  -e TRAILBASE_USER=claude-agent@example.com \
  -e TRAILBASE_PASSWORD=... \
  trailbase-mcp
```

`-i` is required — MCP speaks over stdin/stdout. For sandbox mode inside a
container, mount the depot and a `trail` binary and set
`TRAILBASE_DATA_DIR`/`TRAIL_BIN` accordingly.

## Development

```bash
pnpm -C examples/mcp-server check   # tsc + eslint
pnpm -C examples/mcp-server test    # unit tests (integration test needs a `trail` binary)
```
