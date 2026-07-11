import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

import { jsonResult, toolHandler, type ToolContext } from "./common";

/// All write tools resolve their admin client from the sandbox manager, so
/// arbitrary SQL, DDL and config mutations can only ever hit the ephemeral
/// sandbox instance — never the snapshot-source production URL.
function sandboxAdmin(ctx: ToolContext) {
  return ctx.sandbox.adminClient();
}

export function registerAdminWriteTools(
  server: McpServer,
  ctx: ToolContext,
): void {
  server.registerTool(
    "admin_query",
    {
      title: "Execute SQL (sandbox)",
      description:
        "Executes arbitrary SQL against the sandbox instance and returns columns/rows. Runs on the writer connection (mutations allowed). Sandbox-only. Prefer the schema_* tools for DDL so changes are recorded as migration files.",
      inputSchema: {
        query: z.string().min(1),
      },
    },
    toolHandler("admin_query", async (args) => {
      return jsonResult(await sandboxAdmin(ctx).execQuery(args.query));
    }),
  );

  server.registerTool(
    "admin_config_set",
    {
      title: "Update server config (sandbox)",
      description:
        "Replaces the sandbox instance's server configuration. Pass the full config object and the hash, both from admin_config_get (optimistic concurrency). Sandbox-only.",
      inputSchema: {
        config: z.record(z.string(), z.unknown()),
        hash: z.string().describe("Hash from a preceding admin_config_get"),
      },
    },
    toolHandler("admin_config_set", async (args) => {
      await sandboxAdmin(ctx).updateConfig(args.config, args.hash);
      return jsonResult({ ok: true });
    }),
  );

  // --- Schema DDL. These go through the dedicated admin DDL endpoints (not
  // /query) so every change is recorded as a `U<ts>__<suffix>.sql` migration
  // file, which is the reviewable artifact surfaced by sandbox_diff. ---

  const dryRun = z
    .boolean()
    .optional()
    .describe("Return the generated SQL without applying it");

  server.registerTool(
    "schema_create_table",
    {
      title: "Create table (sandbox)",
      description:
        "Creates a table in the sandbox. `schema` is a TrailBase table definition (same shape as an entry returned by admin_tables — inspect that first). Records a migration file unless dry_run.",
      inputSchema: {
        schema: z
          .record(z.string(), z.unknown())
          .describe("TrailBase Table schema object"),
        dry_run: dryRun,
      },
    },
    toolHandler("schema_create_table", async (args) => {
      return jsonResult(
        await sandboxAdmin(ctx).sendJson("POST", "/table", {
          schema: args.schema,
          dry_run: args.dry_run ?? false,
        }),
      );
    }),
  );

  server.registerTool(
    "schema_alter_table",
    {
      title: "Alter table (sandbox)",
      description:
        "Alters a table in the sandbox. `source_schema`/`target_schema` are TrailBase table definitions describing the before/after state (inspect admin_tables for the shape). Records a migration file unless dry_run.",
      inputSchema: {
        source_schema: z.record(z.string(), z.unknown()),
        target_schema: z.record(z.string(), z.unknown()),
        dry_run: dryRun,
      },
    },
    toolHandler("schema_alter_table", async (args) => {
      return jsonResult(
        await sandboxAdmin(ctx).sendJson("PATCH", "/table", {
          source_schema: args.source_schema,
          target_schema: args.target_schema,
          dry_run: args.dry_run ?? false,
        }),
      );
    }),
  );

  server.registerTool(
    "schema_drop_table",
    {
      title: "Drop table (sandbox)",
      description:
        "Drops a table in the sandbox. Records a migration file unless dry_run.",
      inputSchema: {
        name: z.string().min(1),
        dry_run: dryRun,
      },
    },
    toolHandler("schema_drop_table", async (args) => {
      return jsonResult(
        await sandboxAdmin(ctx).sendJson("DELETE", "/table", {
          name: args.name,
          dry_run: args.dry_run ?? false,
        }),
      );
    }),
  );

  server.registerTool(
    "schema_create_index",
    {
      title: "Create index (sandbox)",
      description:
        "Creates an index in the sandbox. `schema` is a TrailBase index definition. Records a migration file unless dry_run.",
      inputSchema: {
        schema: z
          .record(z.string(), z.unknown())
          .describe("TrailBase TableIndex schema object"),
        dry_run: dryRun,
      },
    },
    toolHandler("schema_create_index", async (args) => {
      return jsonResult(
        await sandboxAdmin(ctx).sendJson("POST", "/index", {
          schema: args.schema,
          dry_run: args.dry_run ?? false,
        }),
      );
    }),
  );

  server.registerTool(
    "schema_drop_index",
    {
      title: "Drop index (sandbox)",
      description:
        "Drops an index in the sandbox. Records a migration file unless dry_run.",
      inputSchema: {
        name: z.string().min(1),
        dry_run: dryRun,
      },
    },
    toolHandler("schema_drop_index", async (args) => {
      return jsonResult(
        await sandboxAdmin(ctx).sendJson("DELETE", "/index", {
          name: args.name,
          dry_run: args.dry_run ?? false,
        }),
      );
    }),
  );
}
