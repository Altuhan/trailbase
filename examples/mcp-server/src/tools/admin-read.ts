import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

import { AdminClient } from "../admin-client";
import { jsonResult, toolHandler, type ToolContext } from "./common";

/// Resolves an admin client from explicit TRAILBASE_ADMIN_TOKEN or, as a
/// fallback, from the record-API user's current auth token (useful when that
/// user is an admin, e.g. inside a sandbox). Admin-ness itself is enforced
/// server-side on every request.
export async function adminClient(ctx: ToolContext): Promise<AdminClient> {
  const config = ctx.config;
  if (config.adminToken !== undefined) {
    return AdminClient.fromToken(config.url, config.adminToken);
  }

  const client = await ctx.client();
  const token = client.tokens()?.auth_token;
  if (token === undefined) {
    throw new Error(
      "Admin tools require TRAILBASE_ADMIN_TOKEN or admin user credentials (TRAILBASE_USER/TRAILBASE_PASSWORD).",
    );
  }
  return AdminClient.fromToken(config.url, token);
}

export function registerAdminReadTools(server: McpServer, ctx: ToolContext): void {
  server.registerTool(
    "admin_tables",
    {
      title: "List tables and schema",
      description:
        "Lists all tables, indexes, triggers and views of the TrailBase instance including their CREATE statements.",
      inputSchema: {},
    },
    toolHandler("admin_tables", async () => {
      return jsonResult(await (await adminClient(ctx)).tables());
    }),
  );

  server.registerTool(
    "admin_config_get",
    {
      title: "Get server config",
      description:
        "Fetches the server configuration (secrets redacted) as JSON plus the hash needed for config updates.",
      inputSchema: {},
    },
    toolHandler("admin_config_get", async () => {
      return jsonResult(await (await adminClient(ctx)).getConfig());
    }),
  );

  server.registerTool(
    "admin_logs",
    {
      title: "Read request logs",
      description:
        "Reads the instance's request logs. Accepts a raw trailbase-qs query string for paging/filtering, e.g. 'limit=50&order=-created&filter[status][$gte]=400'.",
      inputSchema: {
        query: z.string().optional(),
      },
    },
    toolHandler("admin_logs", async (args) => {
      return jsonResult(await (await adminClient(ctx)).logs(args.query));
    }),
  );

  server.registerTool(
    "admin_jobs",
    {
      title: "List system jobs",
      description:
        "Lists periodic system jobs (backups, cleanups, ...) with their schedules and latest runs.",
      inputSchema: {},
    },
    toolHandler("admin_jobs", async () => {
      return jsonResult(await (await adminClient(ctx)).jobs());
    }),
  );

  server.registerTool(
    "admin_info",
    {
      title: "Instance info",
      description: "Fetches build and runtime metadata of the TrailBase instance.",
      inputSchema: {},
    },
    toolHandler("admin_info", async () => {
      return jsonResult(await (await adminClient(ctx)).info());
    }),
  );
}
