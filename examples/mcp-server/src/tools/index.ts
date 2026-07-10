import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

import { isTierEnabled } from "../policy";
import type { ToolContext } from "./common";
import { registerAdminReadTools } from "./admin-read";
import { registerAdminWriteTools } from "./admin-write";
import { registerRecordsTools } from "./records";
import { registerSandboxTools } from "./sandbox";

/// Registers all tools whose tier is enabled for the configured mode.
/// Admin-write and sandbox tools are added by later MCP_PLAN.md items.
export function registerAllTools(server: McpServer, ctx: ToolContext): void {
  const mode = ctx.config.mode;

  if (isTierEnabled(mode, "records")) {
    registerRecordsTools(server, ctx);
  }
  if (isTierEnabled(mode, "admin-read")) {
    registerAdminReadTools(server, ctx);
  }
  if (isTierEnabled(mode, "admin-write")) {
    registerAdminWriteTools(server, ctx);
  }
  if (isTierEnabled(mode, "sandbox-mgmt")) {
    registerSandboxTools(server, ctx);
  }
}
