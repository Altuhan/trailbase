# MCP-server implementation backlog

Working file driving an iterative implementation loop on branch
`claude/trailbase-tech-improvements-aq05eu` (PR #1). One unchecked item per
iteration: implement → verify → commit+push → check off (`[x]` done,
`[!] reason` blocked). Remove this file when finalizing the PR.

Design reference: an external MCP server (TypeScript, `@modelcontextprotocol/sdk`,
stdio) at `examples/mcp-server/` with three access modes — `prod-safe` (record
APIs only, server-enforced ACLs via a dedicated non-admin user),
`prod-admin-readonly` (MCP-side policy, read-only admin tools), `sandbox`
(ephemeral snapshot instance of the depot with full admin) — plus sandbox
lifecycle tools (`sandbox_create/status/diff/destroy`) built on `VACUUM INTO`
snapshots and migration-file diffs.

- [x] 1. Package scaffold at `examples/mcp-server/`: package.json (name
      `trailbase-mcp`, node >= 22; deps: `@modelcontextprotocol/sdk`, `zod`,
      `trailbase` workspace client, `nano-spawn`; dev: typescript, eslint,
      prettier, vitest), tsconfig/eslint/prettier configs modeled after
      `examples/wasm-guest-ts` and `crates/assets/js/client`; register in
      `pnpm-workspace.yaml`; `src/config.ts` (env parsing: TRAILBASE_URL,
      TRAILBASE_MODE, TRAILBASE_USER/PASSWORD, TRAILBASE_AUTH_TOKEN/REFRESH,
      TRAILBASE_ADMIN_TOKEN, TRAILBASE_DATA_DIR, TRAIL_BIN), `src/policy.ts`
      (mode→tool matrix), `src/audit.ts` (stderr JSONL), minimal `src/index.ts`
      (McpServer + StdioServerTransport, no tools yet).
      Verify: `pnpm i` and `pnpm -C examples/mcp-server check` pass.
- [x] 2. Records tools + auth: `src/tools/records.ts` — records_list (filters,
      order, pagination, count), records_read, records_create, records_update,
      records_delete, records_schema, auth_status; client construction from
      config (login or pre-issued tokens). Unit test `tests/policy.test.ts`
      for mode gating. Verify: check + `pnpm -C examples/mcp-server test`.
- [ ] 3. Admin client + read tools: `src/admin-client.ts` (fetch wrapper,
      Bearer + CSRF-Token from JWT claims; GET /api/_admin/tables, /config,
      /logs/list, /jobs, /info; POST /job/run, /query; DDL endpoints);
      `src/tools/admin-read.ts` — admin_tables, admin_config_get, admin_logs,
      admin_jobs, admin_info. Verify: check + unit tests.
- [ ] 4. Sandbox lifecycle: `src/sandbox.ts` — snapshot main.db via node:sqlite
      `VACUUM INTO` (fallback: `sqlite3` CLI), assemble depot (copy
      config.textproto + migrations/, fresh data/secrets), create+promote
      sandbox admin via `trail` CLI, free port, spawn `trail run`, poll
      /api/healthcheck, HTTP login, manifest `.sandbox.json`;
      `src/tools/sandbox.ts` — sandbox_create/status/diff/destroy.
      Unit test `tests/sandbox_assembly.test.ts` (no server needed).
      Verify: check + unit tests.
- [ ] 5. Sandbox-gated write tools: `src/tools/admin-write.ts` — admin_query,
      admin_config_set, schema_create_table/alter_table/drop_table,
      schema_create_index/drop_index (via DDL endpoints so migrations get
      recorded — NOT via /query). Extend policy tests to assert the full
      mode matrix. Verify: check + tests.
- [ ] 6. Integration e2e test `tests/integration.test.ts` (skips gracefully if
      `trail` binary unavailable): build via cargo, temp depot, sandbox_create
      against it, schema_create_table → `U*__*.sql` migration file appears,
      sandbox_diff reports it, record CRUD works inside sandbox,
      sandbox_destroy kills the child process. Verify: run it locally.
- [ ] 7. Docs + finalization: `examples/mcp-server/README.md` (modes, security
      model and its limits, `.mcp.json` snippet, sandbox workflow, remote-prod
      degradation), `skills/trailbase-sandbox.md`, entry in
      `examples/README.md`; run repo formatters (prettier); final push and a
      user-facing summary. PR #1 updates automatically.
