import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { Client as McpClient } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

import { loadConfig } from "../src/config";
import { connect } from "../src/trailbase";
import { newToolContext, type ToolContext } from "../src/tools/common";
import { registerAllTools } from "../src/tools";
import { SandboxManager } from "../src/sandbox";

// Resolve the `trail` binary. Skips the whole suite when it is unavailable so
// CI without a Rust build (and `pnpm test` on its own) stays green.
function resolveTrailBin(): string | undefined {
  const envBin = process.env.TRAIL_BIN;
  if (envBin && existsSync(envBin)) return envBin;

  const repoRoot = resolve(__dirname, "../../..");
  for (const profile of ["debug", "release"]) {
    const candidate = join(repoRoot, "target", profile, "trail");
    if (existsSync(candidate)) return candidate;
  }
  return undefined;
}

const trailBin = resolveTrailBin();
const maybe = trailBin ? describe : describe.skip;

// A minimal source depot: an empty data dir that `trail` initializes on first
// use (running any `trail` subcommand applies the baseline migrations).
function createSourceDepot(trail: string): string {
  const dir = mkdtempSync(join(tmpdir(), "tb-mcp-e2e-src-"));
  // `user add` runs AppState::init which creates data/main.db and applies the
  // embedded baseline migrations, giving us a valid depot to snapshot.
  execFileSync(
    trail,
    ["--data-dir", dir, "user", "add", "seed@localhost", "seed-password-123"],
    {
      stdio: "ignore",
    },
  );
  return dir;
}

maybe("sandbox end-to-end", () => {
  let sourceDir: string;
  let ctx: ToolContext;

  beforeAll(() => {
    sourceDir = createSourceDepot(trailBin!);
    const config = loadConfig({
      TRAILBASE_MODE: "sandbox",
      TRAILBASE_DATA_DIR: sourceDir,
      TRAIL_BIN: trailBin!,
    });
    ctx = newToolContext(config, connect);
  });

  afterAll(async () => {
    if (ctx?.sandbox.isActive()) {
      await ctx.sandbox.destroy();
    }
    if (sourceDir) rmSync(sourceDir, { recursive: true, force: true });
  });

  test("create → DDL records a migration → diff surfaces it → CRUD → destroy", async () => {
    const sandbox: SandboxManager = ctx.sandbox;

    const manifest = await sandbox.create();
    expect(manifest.url).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
    expect((await sandbox.status()).healthy).toBe(true);

    // DDL via the admin endpoint should record a migration file.
    const admin = sandbox.adminClient();
    const created = (await admin.sendJson("POST", "/table", {
      schema: {
        name: { name: "note", database_schema: null },
        strict: false,
        columns: [
          {
            name: "id",
            type_name: "INTEGER",
            data_type: "Integer",
            affinity_type: "Integer",
            options: [
              { Unique: { is_primary: true, conflict_clause: null } },
              "NotNull",
            ],
          },
          {
            name: "body",
            type_name: "TEXT",
            data_type: "Text",
            affinity_type: "Text",
            options: [],
          },
        ],
        foreign_keys: [],
        unique: [],
        checks: [],
        virtual_table: false,
        temporary: false,
      },
      dry_run: false,
    })) as { sql: string };
    expect(created.sql).toContain("note");

    const diff = await sandbox.diff();
    expect(diff.newMigrations.length).toBeGreaterThanOrEqual(1);
    expect(
      diff.newMigrations.some((m) => /CREATE TABLE/i.test(m.content)),
    ).toBe(true);

    // CRUD via the record API requires a configured record API; instead we
    // assert the DDL landed by querying it back over the admin SQL endpoint.
    await admin.execQuery("INSERT INTO note (body) VALUES ('hello')");
    const rows = (await admin.execQuery("SELECT body FROM note")) as {
      rows: unknown[][];
    };
    // The admin query endpoint returns typed values (serialized SqlValue).
    expect(rows.rows).toEqual([[{ Text: "hello" }]]);

    const destroyed = await sandbox.destroy();
    expect(destroyed.removed).toBe(true);
    expect(existsSync(manifest.dataDir)).toBe(false);
    expect(sandbox.isActive()).toBe(false);
  }, 120_000);
});

// Exercises the prod-safe write guards through the real MCP layer against a
// live instance: a sandbox-launched TrailBase stands in for "production".
maybe("prod-safe write guards end-to-end", () => {
  let sourceDir: string;
  let infra: ToolContext;
  let mcp: McpClient;

  const callTool = async (
    name: string,
    args: Record<string, unknown>,
  ): Promise<{ text: string; isError: boolean }> => {
    const result = (await mcp.callTool({
      name,
      arguments: args,
    })) as CallToolResult;
    const first = result.content[0];
    return {
      text: first?.type === "text" ? first.text : "",
      isError: result.isError === true,
    };
  };

  const noteRows = async (): Promise<unknown[]> => {
    const result = (await infra.sandbox
      .adminClient()
      .execQuery("SELECT body FROM note")) as { rows: unknown[][] };
    return result.rows;
  };

  beforeAll(async () => {
    sourceDir = createSourceDepot(trailBin!);
    infra = newToolContext(
      loadConfig({
        TRAILBASE_MODE: "sandbox",
        TRAILBASE_DATA_DIR: sourceDir,
        TRAIL_BIN: trailBin!,
      }),
      connect,
    );
    const manifest = await infra.sandbox.create();
    const admin = infra.sandbox.adminClient();

    // A `note` table plus an authenticated-only record API for it, so the
    // prod-safe session exercises real server-side ACLs.
    await admin.sendJson("POST", "/table", {
      schema: {
        name: { name: "note", database_schema: null },
        // Record APIs require STRICT tables.
        strict: true,
        columns: [
          {
            name: "id",
            type_name: "INTEGER",
            data_type: "Integer",
            affinity_type: "Integer",
            options: [
              { Unique: { is_primary: true, conflict_clause: null } },
              "NotNull",
            ],
          },
          {
            name: "body",
            type_name: "TEXT",
            data_type: "Text",
            affinity_type: "Text",
            options: [],
          },
          {
            name: "secret",
            type_name: "TEXT",
            data_type: "Text",
            affinity_type: "Text",
            options: [],
          },
        ],
        foreign_keys: [],
        unique: [],
        checks: [],
        virtual_table: false,
        temporary: false,
      },
      dry_run: false,
    });
    const { config, hash } = await admin.getConfig();
    const proto = config as { recordApis?: unknown[] };
    proto.recordApis = [
      ...(proto.recordApis ?? []),
      {
        name: "note",
        tableName: "note",
        aclAuthenticated: ["CREATE", "READ", "UPDATE", "DELETE"],
      },
    ];
    await admin.updateConfig(proto, hash!);

    // The MCP server under test: prod-safe mode, logged in as the non-admin
    // seed user, with column redaction configured.
    const server = new McpServer({ name: "test", version: "0.0.0" });
    registerAllTools(
      server,
      newToolContext(
        loadConfig({
          TRAILBASE_MODE: "prod-safe",
          TRAILBASE_URL: manifest.url,
          TRAILBASE_USER: "seed@localhost",
          TRAILBASE_PASSWORD: "seed-password-123",
          TRAILBASE_REDACT_COLUMNS: "secret",
        }),
        connect,
      ),
    );
    const [clientTransport, serverTransport] =
      InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    mcp = new McpClient({ name: "test-client", version: "0.0.0" });
    await mcp.connect(clientTransport);
  }, 120_000);

  afterAll(async () => {
    await mcp?.close();
    if (infra?.sandbox.isActive()) {
      await infra.sandbox.destroy();
    }
    if (sourceDir) rmSync(sourceDir, { recursive: true, force: true });
  });

  test("create parks until confirmed, reads are redacted, cancel discards", async () => {
    // 1. The mutation is parked, nothing hits the database.
    const proposed = await callTool("records_create", {
      api: "note",
      record: { body: "hello", secret: "s3cr3t" },
    });
    const pending = JSON.parse(proposed.text) as {
      status: string;
      pending_id: string;
    };
    expect(pending.status).toBe("pending_confirmation");
    expect(await noteRows()).toEqual([]);

    // 2. Confirming executes the write against the live instance.
    const confirmed = await callTool("write_confirm", {
      pending_id: pending.pending_id,
    });
    expect(confirmed.isError).toBe(false);
    const { id } = JSON.parse(confirmed.text) as { id: string | number };
    expect(await noteRows()).toHaveLength(1);

    // A pending id is single-use.
    const replay = await callTool("write_confirm", {
      pending_id: pending.pending_id,
    });
    expect(replay.isError).toBe(true);

    // 3. Reads mask the redacted column but not the rest.
    const listed = await callTool("records_list", { api: "note" });
    const page = JSON.parse(listed.text) as {
      records: { body: string; secret: string }[];
    };
    expect(page.records).toHaveLength(1);
    expect(page.records[0].body).toBe("hello");
    expect(page.records[0].secret).toBe("[REDACTED]");

    // 4. Cancelling a parked delete leaves the record in place.
    const proposedDelete = await callTool("records_delete", {
      api: "note",
      id,
    });
    const pendingDelete = JSON.parse(proposedDelete.text) as {
      pending_id: string;
    };
    const cancelled = await callTool("write_cancel", {
      pending_id: pendingDelete.pending_id,
    });
    expect(cancelled.isError).toBe(false);
    expect(await noteRows()).toHaveLength(1);

    // 5. Exactly one confirmed write was charged against the budget.
    const status = await callTool("auth_status", {});
    const parsed = JSON.parse(status.text) as {
      write_guards: { confirm_writes: boolean; budget_remaining: number };
    };
    expect(parsed.write_guards.confirm_writes).toBe(true);
    expect(parsed.write_guards.budget_remaining).toBe(99);
  }, 120_000);
});
