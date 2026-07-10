import { describe, expect, test } from "vitest";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { Client as McpClient } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import { MODES, type Config, type Mode } from "../src/config";
import { enabledTiers } from "../src/policy";
import { registerAllTools } from "../src/tools";
import { newToolContext } from "../src/tools/common";

function testConfig(mode: Mode): Config {
  return {
    url: "http://localhost:4000",
    mode,
    trailBin: "trail",
  };
}

async function listToolNames(mode: Mode): Promise<string[]> {
  const server = new McpServer({ name: "test", version: "0.0.0" });
  const ctx = newToolContext(testConfig(mode), () => {
    throw new Error("client must not be constructed during registration");
  });
  registerAllTools(server, ctx);

  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);

  const client = new McpClient({ name: "test-client", version: "0.0.0" });
  await client.connect(clientTransport);
  try {
    const { tools } = await client.listTools();
    return tools.map((tool) => tool.name).sort();
  } finally {
    await client.close();
    await server.close();
  }
}

const SANDBOX_TOOLS = [
  "sandbox_create",
  "sandbox_destroy",
  "sandbox_diff",
  "sandbox_status",
];

const ADMIN_READ_TOOLS = [
  "admin_config_get",
  "admin_info",
  "admin_jobs",
  "admin_logs",
  "admin_tables",
];

const RECORDS_TOOLS = [
  "auth_status",
  "records_create",
  "records_delete",
  "records_list",
  "records_read",
  "records_schema",
  "records_update",
];

describe("policy matrix", () => {
  test("tier sets per mode", () => {
    expect(enabledTiers("prod-safe")).toEqual(new Set(["records", "sandbox-mgmt"]));
    expect(enabledTiers("prod-admin-readonly")).toEqual(
      new Set(["records", "admin-read", "sandbox-mgmt"]),
    );
    expect(enabledTiers("sandbox")).toEqual(
      new Set(["records", "admin-read", "admin-write"]),
    );
  });

  test("records tools are registered in every mode", async () => {
    for (const mode of MODES) {
      const names = await listToolNames(mode);
      for (const tool of RECORDS_TOOLS) {
        expect(names, `mode=${mode}`).toContain(tool);
      }
    }
  });

  test("no admin tools are exposed in prod-safe", async () => {
    const names = await listToolNames("prod-safe");
    expect(names.filter((n) => n.startsWith("admin_"))).toEqual([]);
    expect(names.filter((n) => n.startsWith("schema_"))).toEqual([]);
  });

  test("sandbox management is exposed in prod modes but not inside a sandbox", async () => {
    for (const mode of ["prod-safe", "prod-admin-readonly"] as const) {
      const names = await listToolNames(mode);
      for (const tool of SANDBOX_TOOLS) {
        expect(names, `mode=${mode}`).toContain(tool);
      }
    }
    const names = await listToolNames("sandbox");
    expect(names.filter((n) => n.startsWith("sandbox_"))).toEqual([]);
  });

  test("prod-admin-readonly exposes read-only admin tools", async () => {
    const names = await listToolNames("prod-admin-readonly");
    for (const tool of ADMIN_READ_TOOLS) {
      expect(names).toContain(tool);
    }
    // No arbitrary SQL, DDL, config mutation or user management.
    expect(names).not.toContain("admin_query");
    expect(names.filter((n) => n.startsWith("schema_"))).toEqual([]);
    expect(names).not.toContain("admin_config_set");
  });

  test("sandbox mode exposes admin-read tools", async () => {
    const names = await listToolNames("sandbox");
    for (const tool of ADMIN_READ_TOOLS) {
      expect(names).toContain(tool);
    }
  });
});
