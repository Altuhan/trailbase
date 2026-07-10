import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

import { isTierEnabled } from "../policy";
import type { ToolContext } from "./common";
import { registerRecordsTools } from "./records";

/// Registers all tools whose tier is enabled for the configured mode.
/// Admin and sandbox tools are added by later MCP_PLAN.md items.
export function registerAllTools(server: McpServer, ctx: ToolContext): void {
  const mode = ctx.config.mode;

  if (isTierEnabled(mode, "records")) {
    registerRecordsTools(server, ctx);
  }
}
