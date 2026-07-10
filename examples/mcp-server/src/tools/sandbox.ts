import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

import { jsonResult, toolHandler, type ToolContext } from "./common";

export function registerSandboxTools(server: McpServer, ctx: ToolContext): void {
  server.registerTool(
    "sandbox_create",
    {
      title: "Create sandbox instance",
      description:
        "Creates an ephemeral TrailBase sandbox: takes a consistent snapshot of the live depot's main database (VACUUM INTO), copies config and migrations (secrets and sessions are NOT copied — the sandbox gets fresh keys), creates a random-password sandbox admin and starts a `trail` server on a free localhost port. Requires filesystem access to the depot (same host).",
      inputSchema: {
        source_data_dir: z
          .string()
          .optional()
          .describe("Source depot path; defaults to TRAILBASE_DATA_DIR"),
      },
    },
    toolHandler("sandbox_create", async (args) => {
      const manifest = await ctx.sandbox.create({
        sourceDataDir: args.source_data_dir,
      });
      return jsonResult({
        ...manifest,
        note: "Admin-write tools (admin_query, schema_*, admin_config_set) now target this sandbox. Schema changes are recorded as migration files; inspect them with sandbox_diff.",
      });
    }),
  );

  server.registerTool(
    "sandbox_status",
    {
      title: "Sandbox status",
      description: "Reports whether a sandbox is running and healthy.",
      inputSchema: {},
    },
    toolHandler("sandbox_status", async () => {
      return jsonResult(await ctx.sandbox.status());
    }),
  );

  server.registerTool(
    "sandbox_diff",
    {
      title: "Sandbox changes for review",
      description:
        "Lists what changed in the sandbox since creation: newly recorded migration files (the reviewable artifact — apply them to production via your normal deploy) and config.textproto changes. There is deliberately no auto-apply to production.",
      inputSchema: {},
    },
    toolHandler("sandbox_diff", async () => {
      return jsonResult(await ctx.sandbox.diff());
    }),
  );

  server.registerTool(
    "sandbox_destroy",
    {
      title: "Destroy sandbox",
      description:
        "Stops the sandbox server and deletes its data dir (pass keep_dir=true to keep it, e.g. to hand-inspect generated migrations).",
      inputSchema: {
        keep_dir: z.boolean().optional(),
      },
    },
    toolHandler("sandbox_destroy", async (args) => {
      return jsonResult(await ctx.sandbox.destroy({ keepDir: args.keep_dir }));
    }),
  );
}
