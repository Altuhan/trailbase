import type { Client } from "trailbase";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

import type { Config } from "../config";
import { withAudit } from "../audit";

export interface ToolContext {
  readonly config: Config;
  /// Lazily constructed, memoized TrailBase client for record APIs.
  client(): Promise<Client>;
}

export function newToolContext(
  config: Config,
  connect: (config: Config) => Promise<Client>,
): ToolContext {
  let client: Promise<Client> | undefined;
  return {
    config,
    client: () => (client ??= connect(config)),
  };
}

export function jsonResult(data: unknown): CallToolResult {
  return {
    content: [{ type: "text", text: JSON.stringify(data, null, 2) }],
  };
}

export function textResult(text: string): CallToolResult {
  return { content: [{ type: "text", text }] };
}

function errorResult(err: unknown): CallToolResult {
  const message = err instanceof Error ? err.message : String(err);
  return {
    content: [{ type: "text", text: `Error: ${message}` }],
    isError: true,
  };
}

/// Wraps a tool implementation with audit logging and error-to-result
/// conversion, so failures surface to the model instead of crashing the
/// server.
export function toolHandler<Args>(
  name: string,
  fn: (args: Args) => Promise<CallToolResult>,
): (args: Args) => Promise<CallToolResult> {
  return async (args: Args) => {
    try {
      return await withAudit(name, args, () => fn(args));
    } catch (err) {
      return errorResult(err);
    }
  };
}
