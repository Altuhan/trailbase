// Live e2e for `trail mcp`: spawns the real binary over stdio and drives the
// full guarded-write flow through the MCP protocol.
//
// Setup (from the repo root):
//   cargo build -p trailbase-cli
//   D=$(mktemp -d)
//   target/debug/trail --data-dir $D user add seed@localhost seed-password-123
//   # create a STRICT table `note(id INTEGER PK, body TEXT, secret TEXT)` in
//   # $D/data/main.db and append record APIs `note` (authenticated CRUD) and
//   # `note_ro` (authenticated READ) to $D/config.textproto, then:
//   cd examples/mcp-server   # borrows @modelcontextprotocol/sdk from here
//   node ../../crates/mcp/tests/e2e.mjs ../../target/debug/trail $D
import { pathToFileURL } from "node:url";

// The MCP SDK is resolved from the *current working directory* (run this from
// examples/mcp-server), not from this file's location inside crates/.
const sdk = (path) =>
  import(new URL(`node_modules/@modelcontextprotocol/sdk/dist/esm/${path}`,
    pathToFileURL(process.cwd() + "/")).href);
const { Client } = await sdk("client/index.js");
const { StdioClientTransport } = await sdk("client/stdio.js");

const [trailBin, depot] = process.argv.slice(2);
let failures = 0;
const check = (cond, label) => {
  console.log(`${cond ? "PASS" : "FAIL"}: ${label}`);
  if (!cond) failures += 1;
};

const transport = new StdioClientTransport({
  command: trailBin,
  args: [
    "--data-dir", depot,
    "mcp",
    "--user", "seed@localhost",
    "--mode", "records",
    "--redact-columns", "secret",
    "--sandbox",
  ],
  env: { ...process.env, TRAIL_MCP_PASSWORD: "seed-password-123" },
  stderr: "ignore",
});
const client = new Client({ name: "e2e", version: "0.0.0" });
await client.connect(transport);

const call = async (name, args = {}) => {
  const res = await client.callTool({ name, arguments: args });
  const text = res.content?.[0]?.type === "text" ? res.content[0].text : "";
  return { text, isError: res.isError === true };
};
const asJson = (r) => JSON.parse(r.text);

// 1. Handshake + expected tool set.
const { tools } = await client.listTools();
const names = tools.map((t) => t.name).sort();
for (const tool of ["records_apis", "records_list", "records_create", "write_confirm", "write_cancel", "auth_status"]) {
  check(names.includes(tool), `tool exposed: ${tool}`);
}

// 2. Introspection: both configured record APIs are visible.
const apis = asJson(await call("records_apis")).record_apis.map((a) => a.name);
check(apis.includes("note") && apis.includes("note_ro"), `records_apis lists note+note_ro (${apis})`);

// 3. Create parks; nothing written yet.
const proposed = asJson(await call("records_create", { api: "note", record: { body: "hello", secret: "s3cr3t" } }));
check(proposed.status === "pending_confirmation" && !!proposed.pending_id, "create parked as pending");
const empty = asJson(await call("records_list", { api: "note" }));
check((empty.records ?? []).length === 0, "no rows before write_confirm");

// 4. Confirm executes; replay is rejected.
const confirmed = await call("write_confirm", { pending_id: proposed.pending_id });
check(!confirmed.isError, "write_confirm executes");
const createdId = asJson(confirmed).ids?.[0] ?? asJson(confirmed).id;
check(createdId !== undefined, `created id: ${JSON.stringify(asJson(confirmed))}`);
const replay = await call("write_confirm", { pending_id: proposed.pending_id });
check(replay.isError, "pending_id is single-use");

// 5. Reads: row present, secret column redacted.
const listed = asJson(await call("records_list", { api: "note" }));
check(listed.records.length === 1 && listed.records[0].body === "hello", "row visible after confirm");
check(listed.records[0].secret === "[REDACTED]", "secret column redacted");

// 6. Server-side ACL: the read-only API rejects writes with an HTTP error.
const denied = await call("records_create", { api: "note_ro", record: { body: "nope" } });
const deniedConfirm = denied.isError ? denied : await call("write_confirm", { pending_id: asJson(denied).pending_id });
check(deniedConfirm.isError && /HTTP 4/.test(deniedConfirm.text), `ACL write via note_ro rejected server-side (${deniedConfirm.text.slice(0, 60)})`);

// 7. Cancelled delete leaves the row.
const del = asJson(await call("records_delete", { api: "note", id: String(createdId) }));
await call("write_cancel", { pending_id: del.pending_id });
const still = asJson(await call("records_list", { api: "note" }));
check(still.records.length === 1, "cancelled delete left the row");

// 8. Budget: exactly one confirmed create + one confirmed (rejected) ACL write charged.
const status = asJson(await call("auth_status"));
check(status.write_guards.confirm_writes === true, "confirm_writes on");
check(typeof status.write_guards.budget_remaining === "number" && status.write_guards.budget_remaining <= 99, `budget charged (${status.write_guards.budget_remaining})`);
check(status.user.email === "seed@localhost", "acting user reported");

// 9. Introspection: schema_tables sees the user table (hidden) and note.
const schema = asJson(await call("schema_tables"));
const note = schema.objects.find((o) => o.name === "note");
const userTable = schema.objects.find((o) => o.name === "_user");
check(note?.kind === "table" && note?.hidden === false && /CREATE TABLE/i.test(note?.sql ?? ""), "schema_tables lists note with SQL");
check(userTable?.hidden === true, "_user marked hidden");
const info = asJson(await call("instance_info"));
check(info.record_apis === 2, `instance_info counts record APIs (${info.record_apis})`);

// 10. Audit: in-process tool calls are logged to _logs like normal HTTP requests.
const { DatabaseSync } = await import("node:sqlite");
let logged = 0;
for (let i = 0; i < 20; i++) {
  await new Promise((r) => setTimeout(r, 500));
  try {
    const db = new DatabaseSync(`${depot}/data/logs.db`, { readOnly: true });
    logged = db
      .prepare("SELECT COUNT(*) AS n FROM _logs WHERE url LIKE '%/api/records/v1/note%'")
      .get().n;
    db.close();
  } catch {
    // Log DB may be mid-write; retry.
  }
  if (logged > 0) break;
}
check(logged > 0, `records_* calls audited in _logs (${logged} rows)`);

// 11. Sandbox: snapshot -> DDL records a migration -> diff -> SQL -> destroy.
const manifest = asJson(await call("sandbox_create"));
check(typeof manifest.url === "string" && manifest.url.includes("127.0.0.1"), `sandbox up at ${manifest.url}`);
const sbStatus = asJson(await call("sandbox_status"));
check(sbStatus.active === true && sbStatus.healthy === true, "sandbox healthy");

const ddl = await call("sandbox_ddl", {
  action: "create_table",
  payload: {
    schema: {
      name: { name: "draft", database_schema: null },
      strict: true,
      columns: [
        {
          name: "id",
          type_name: "INTEGER",
          data_type: "Integer",
          affinity_type: "Integer",
          options: [{ Unique: { is_primary: true, conflict_clause: null } }, "NotNull"],
        },
        { name: "title", type_name: "TEXT", data_type: "Text", affinity_type: "Text", options: [] },
      ],
      foreign_keys: [],
      unique: [],
      checks: [],
      virtual_table: false,
      temporary: false,
    },
    dry_run: false,
  },
});
check(!ddl.isError, `sandbox_ddl create_table (${ddl.text.slice(0, 60)})`);

const diff = asJson(await call("sandbox_diff"));
check(
  diff.new_migrations.length >= 1 && diff.new_migrations.some((m) => /CREATE TABLE/i.test(m.content)),
  "diff surfaces the new migration file",
);

const inserted = await call("sandbox_query", { query: "INSERT INTO draft (title) VALUES ('x')" });
check(!inserted.isError, "sandbox_query INSERT");
const selected = await call("sandbox_query", { query: "SELECT title FROM draft" });
check(!selected.isError && /x/.test(selected.text), "sandbox_query SELECT reads back");

// The live instance is untouched by all of the above.
const schemaAfter = asJson(await call("schema_tables"));
check(!schemaAfter.objects.some((o) => o.name === "draft"), "live instance untouched by sandbox DDL");

const destroyed = asJson(await call("sandbox_destroy"));
check(destroyed.removed === true, "sandbox destroyed");
const { existsSync } = await import("node:fs");
check(!existsSync(manifest.data_dir), "sandbox dir removed");

await client.close();
console.log(failures === 0 ? "ALL PASS" : `${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
