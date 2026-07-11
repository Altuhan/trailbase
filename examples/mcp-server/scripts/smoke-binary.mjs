#!/usr/bin/env node
// MCP handshake smoke test against a compiled stdio server: spawns the given
// command, performs initialize + tools/list and checks a few expected tool
// names. Also reusable against `trail mcp` style servers:
//   node smoke-binary.mjs <command> [args...]
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const [command, ...args] = process.argv.slice(2);
if (!command) {
  console.error("usage: smoke-binary.mjs <command> [args...]");
  process.exit(2);
}

const transport = new StdioClientTransport({
  command,
  args,
  env: {
    ...process.env,
    TRAILBASE_MODE: "prod-safe",
    // Nothing must actually connect during registration; a dead URL proves it.
    TRAILBASE_URL: "http://localhost:59999",
  },
  stderr: "inherit",
});

const client = new Client({ name: "smoke", version: "0.0.0" });
await client.connect(transport);
const { tools } = await client.listTools();
const names = tools.map((tool) => tool.name).sort();
await client.close();

const expected = ["records_list", "write_confirm", "sandbox_create"];
const missing = expected.filter((name) => !names.includes(name));
if (missing.length > 0) {
  console.error(`missing tools: ${missing.join(", ")}`);
  console.error(`got: ${names.join(", ")}`);
  process.exit(1);
}
console.log(`    MCP handshake OK: ${names.length} tools exposed`);
