// Live e2e for the remote MCP endpoint (`trail run --mcp`): starts a real
// server, logs in over HTTP and drives the guarded-write flow through
// Streamable HTTP with per-caller Authorization.
//
// Setup: same depot recipe as e2e.mjs (seed user + note/note_ro record APIs).
// Run from examples/mcp-server (borrows @modelcontextprotocol/sdk):
//   node ../../crates/mcp/tests/e2e-http.mjs ../../target/debug/trail $DEPOT
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

const sdk = (path) =>
  import(
    new URL(
      `node_modules/@modelcontextprotocol/sdk/dist/esm/${path}`,
      pathToFileURL(process.cwd() + "/"),
    ).href
  );
const { Client } = await sdk("client/index.js");
const { StreamableHTTPClientTransport } = await sdk("client/streamableHttp.js");

const [trailBin, depot] = process.argv.slice(2);
const port = 41000 + Math.floor(Math.random() * 1000);
const base = `http://127.0.0.1:${port}`;

let failures = 0;
const check = (cond, label) => {
  console.log(`${cond ? "PASS" : "FAIL"}: ${label}`);
  if (!cond) failures += 1;
};

// 1. Start the real server with the MCP endpoint enabled.
const server = spawn(
  trailBin,
  [
    "--data-dir", depot,
    "run",
    "--address", `127.0.0.1:${port}`,
    "--mcp",
    "--mcp-mode", "records",
    "--mcp-redact-columns", "secret",
  ],
  { stdio: ["ignore", "ignore", "ignore"] },
);
const stop = () => {
  try {
    server.kill("SIGKILL");
  } catch {
    // Already dead.
  }
};
process.on("exit", stop);

let healthy = false;
for (let i = 0; i < 100; i++) {
  await new Promise((r) => setTimeout(r, 200));
  try {
    const res = await fetch(`${base}/api/healthcheck`);
    if (res.ok) {
      healthy = true;
      break;
    }
  } catch {
    // Not up yet.
  }
}
check(healthy, "server healthy");

// 2. Login over the network as the seed user.
const login = await fetch(`${base}/api/auth/v1/login`, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ email: "seed@localhost", password: "seed-password-123" }),
});
check(login.ok, `login over HTTP (${login.status})`);
const { auth_token } = await login.json();
check(typeof auth_token === "string" && auth_token.length > 0, "auth token issued");

const connect = async (headers) => {
  const client = new Client({ name: "e2e-http", version: "0.0.0" });
  await client.connect(
    new StreamableHTTPClientTransport(new URL(`${base}/mcp`), {
      requestInit: { headers },
    }),
  );
  return client;
};
const call = async (client, name, args = {}) => {
  const res = await client.callTool({ name, arguments: args });
  const text = res.content?.[0]?.type === "text" ? res.content[0].text : "";
  return { text, isError: res.isError === true };
};
const asJson = (r) => JSON.parse(r.text);

// 3. Authenticated session: full guarded flow under the caller's own token.
const mcp = await connect({ Authorization: `Bearer ${auth_token}` });
const { tools } = await mcp.listTools();
check(tools.length >= 12, `handshake exposes tools (${tools.length})`);

const status = asJson(await call(mcp, "auth_status"));
check(status.transport.includes("streamable-http"), "transport reported");
check(String(status.user).includes("per-request"), "per-request auth reported");

const proposed = asJson(
  await call(mcp, "records_create", { api: "note", record: { body: "remote", secret: "s3cr3t" } }),
);
check(proposed.status === "pending_confirmation", "create parked as pending");
const confirmed = await call(mcp, "write_confirm", { pending_id: proposed.pending_id });
check(!confirmed.isError, "write_confirm executes over http session");

const listed = asJson(await call(mcp, "records_list", { api: "note" }));
const row = listed.records.find((r) => r.body === "remote");
check(row !== undefined && row.secret === "[REDACTED]", "read back redacted");

// Server-side ACL with the caller's token: read-only API rejects the write.
const denied = asJson(await call(mcp, "records_create", { api: "note_ro", record: { body: "x" } }));
const deniedConfirm = await call(mcp, "write_confirm", { pending_id: denied.pending_id });
check(deniedConfirm.isError && /HTTP 4/.test(deniedConfirm.text), "ACL rejects via caller token");

await mcp.close();

// 4. Unauthenticated session: handshake succeeds, tools demand a token.
const anon = await connect({});
const anonList = await call(anon, "records_list", { api: "note" });
check(anonList.isError && /Authorization/.test(anonList.text), "no token -> guided error");
const anonSchema = await call(anon, "schema_tables");
check(!anonSchema.isError, "introspection works without token (metadata only)");
await anon.close();

stop();
console.log(failures === 0 ? "ALL PASS" : `${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
