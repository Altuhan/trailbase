---
name: trailbase-sandbox
description: Safely evolve a TrailBase schema against a throwaway snapshot of production, then hand back reviewable migration files. Use when asked to change tables/indexes, run exploratory SQL, or try config changes on a TrailBase instance without risking live data.
---

# TrailBase sandbox workflow

The `trailbase` MCP server (mode `sandbox`) can spin up an **ephemeral copy** of
a live TrailBase depot. Schema changes there are recorded as migration files you
review and apply through the normal deploy — never mutate production directly.

The same workflow exists in the embedded server (`trail mcp --sandbox`,
stdio): there the tools are `sandbox_ddl` (instead of the `schema_*` family;
pass `action` + the endpoint `payload`) and `sandbox_query` (instead of
`admin_query`); create/status/diff/destroy behave identically.

## When to use

- Any DDL: creating/altering/dropping tables or indexes.
- Arbitrary/exploratory SQL (`admin_query`).
- Trying server-config changes.

For plain data reads/writes against production, use the `records_*` tools
directly instead — they are enforced by the server's ACLs. In prod modes,
mutations are two-phase by default: `records_create/update/delete` return a
`pending_id` and write nothing until you call `write_confirm` with it
(`write_cancel` discards). Re-check the returned action summary before
confirming; unconfirmed writes expire on their own, and each session has a
limited write budget (see `auth_status`).

## Loop

1. **Create**: call `sandbox_create` (needs `TRAILBASE_DATA_DIR`). It snapshots
   `main.db` (SQLite online-backup API), copies config + migrations, starts a
   private localhost instance. All later admin/schema/records calls target it.
2. **Inspect** the current schema with `admin_tables` before changing it — the
   `schema_*` tools take TrailBase table/index objects shaped like those
   entries.
3. **Change** via `schema_create_table` / `schema_alter_table` /
   `schema_drop_table` / `schema_create_index` / `schema_drop_index`. Use
   `dry_run: true` first to preview the generated SQL. Prefer these over
   `admin_query` for DDL so the change is recorded as a migration.
4. **Verify** by exercising the change: `admin_query` for a quick SELECT, or
   `records_*` if a Record API covers the table.
5. **Review**: call `sandbox_diff`. It returns the new `U*__*.sql` migration
   files (and any config changes). Present these to the human — they are the
   deliverable. Do **not** attempt to apply them to production yourself.
6. **Destroy**: call `sandbox_destroy` when done (pass `keep_dir: true` only if
   the human wants to inspect the generated files on disk).

## Rules

- Never run DDL or `admin_query` against production; they are only wired to the
  sandbox by design.
- Always finish with `sandbox_diff` so the human has the migration files, then
  `sandbox_destroy`.
- Applying migrations to production is a human step (normal deploy); the agent
  stops at producing and explaining them.
