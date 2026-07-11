import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { Filter, ListOpts } from "trailbase";

import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

import { redactData } from "../guards";
import {
  jsonResult,
  textResult,
  toolHandler,
  type ToolContext,
} from "./common";

const COMPARE_OPS = [
  "equal",
  "notEqual",
  "lessThan",
  "lessThanEqual",
  "greaterThan",
  "greaterThanEqual",
  "like",
  "regexp",
  "isNull",
  "isNotNull",
] as const;

const apiName = z
  .string()
  .min(1)
  .describe("Name of the record API as configured in TrailBase");

const recordId = z
  .union([z.string(), z.number()])
  .describe("Record id (integer or UUID string primary key)");

const filterSchema = z.object({
  column: z.string(),
  op: z
    .enum(COMPARE_OPS)
    .optional()
    .describe("Comparison operator; defaults to 'equal'"),
  value: z
    .string()
    .describe("Value to compare against, always passed as a string"),
});

/// Runs a record mutation through the prod-mode write guards: with
/// confirmation enabled the mutation is parked and only executed by a
/// subsequent write_confirm; otherwise it just charges the write budget.
function guardedWrite(
  ctx: ToolContext,
  tool: string,
  summary: string,
  execute: () => Promise<CallToolResult>,
): Promise<CallToolResult> {
  const guards = ctx.guards;
  if (!guards.active) {
    return execute();
  }
  if (guards.confirmWrites) {
    const pending = guards.propose(tool, summary, execute);
    return Promise.resolve(
      jsonResult({
        status: "pending_confirmation",
        pending_id: pending.id,
        action: summary,
        expires_in_secs: guards.confirmTimeoutSecs,
        next:
          "Nothing was written yet. Call write_confirm with this " +
          "pending_id to execute, or write_cancel to discard.",
      }),
    );
  }
  guards.useBudget();
  return execute();
}

export function registerRecordsTools(
  server: McpServer,
  ctx: ToolContext,
): void {
  server.registerTool(
    "records_list",
    {
      title: "List records",
      description:
        "Lists records of a TrailBase record API with optional filters, ordering and cursor/offset pagination. Access is enforced server-side by the API's ACLs.",
      inputSchema: {
        api: apiName,
        limit: z.number().int().positive().optional(),
        cursor: z
          .string()
          .optional()
          .describe("Cursor from a previous response"),
        offset: z.number().int().nonnegative().optional(),
        order: z
          .array(z.string())
          .optional()
          .describe("Columns to order by; prefix with '-' for descending"),
        filters: z.array(filterSchema).optional(),
        count: z.boolean().optional().describe("Include total_count"),
        expand: z
          .array(z.string())
          .optional()
          .describe("Foreign-key columns to expand inline"),
      },
    },
    toolHandler("records_list", async (args) => {
      const client = await ctx.client();
      const opts: ListOpts = {
        pagination: {
          limit: args.limit,
          cursor: args.cursor,
          offset: args.offset,
        },
        order: args.order,
        filters: args.filters?.map((f): Filter => ({ ...f })),
        count: args.count,
        expand: args.expand,
      };
      const page = await client.records(args.api).list(opts);
      return jsonResult(redactData(page, ctx.config.redactColumns));
    }),
  );

  server.registerTool(
    "records_read",
    {
      title: "Read record",
      description: "Reads a single record by id from a TrailBase record API.",
      inputSchema: {
        api: apiName,
        id: recordId,
        expand: z.array(z.string()).optional(),
      },
    },
    toolHandler("records_read", async (args) => {
      const client = await ctx.client();
      const record = await client
        .records(args.api)
        .read(args.id, { expand: args.expand });
      return jsonResult(redactData(record, ctx.config.redactColumns));
    }),
  );

  server.registerTool(
    "records_create",
    {
      title: "Create record",
      description:
        "Creates a record via a TrailBase record API. Write access is enforced server-side by the API's ACLs.",
      inputSchema: {
        api: apiName,
        record: z.record(z.string(), z.unknown()).describe("Column/value map"),
      },
    },
    toolHandler("records_create", (args) =>
      guardedWrite(
        ctx,
        "records_create",
        `create record in '${args.api}' with columns [${Object.keys(args.record).join(", ")}]`,
        async () => {
          const client = await ctx.client();
          const id = await client.records(args.api).create(args.record);
          return jsonResult({ id });
        },
      ),
    ),
  );

  server.registerTool(
    "records_update",
    {
      title: "Update record",
      description: "Partially updates an existing record by id.",
      inputSchema: {
        api: apiName,
        id: recordId,
        record: z
          .record(z.string(), z.unknown())
          .describe("Column/value map of fields to update"),
      },
    },
    toolHandler("records_update", (args) =>
      guardedWrite(
        ctx,
        "records_update",
        `update record '${args.id}' in '${args.api}', columns [${Object.keys(args.record).join(", ")}]`,
        async () => {
          const client = await ctx.client();
          await client.records(args.api).update(args.id, args.record);
          return textResult("OK");
        },
      ),
    ),
  );

  server.registerTool(
    "records_delete",
    {
      title: "Delete record",
      description: "Deletes a record by id.",
      inputSchema: {
        api: apiName,
        id: recordId,
      },
    },
    toolHandler("records_delete", (args) =>
      guardedWrite(
        ctx,
        "records_delete",
        `delete record '${args.id}' from '${args.api}'`,
        async () => {
          const client = await ctx.client();
          await client.records(args.api).delete(args.id);
          return textResult("OK");
        },
      ),
    ),
  );

  server.registerTool(
    "records_schema",
    {
      title: "Record API JSON schema",
      description:
        "Fetches the JSON schema describing records of the given record API.",
      inputSchema: {
        api: apiName,
      },
    },
    toolHandler("records_schema", async (args) => {
      const client = await ctx.client();
      const response = await client.fetch(
        `/api/records/v1/${encodeURIComponent(args.api)}/schema`,
      );
      return jsonResult(await response.json());
    }),
  );

  server.registerTool(
    "auth_status",
    {
      title: "Authentication status",
      description:
        "Reports which TrailBase instance the server talks to, the access mode and the authenticated user (if any).",
      inputSchema: {},
    },
    toolHandler("auth_status", async () => {
      const client = await ctx.client();
      const user = client.user();
      return jsonResult({
        url: ctx.config.url,
        mode: ctx.config.mode,
        authenticated: user !== undefined,
        user:
          user !== undefined
            ? { id: user.id, email: user.email, username: user.username }
            : null,
        write_guards: {
          confirm_writes: ctx.guards.confirmWrites,
          budget_remaining: ctx.guards.budgetRemaining() ?? "unlimited",
        },
      });
    }),
  );

  if (ctx.guards.confirmWrites) {
    const pendingId = z
      .string()
      .describe("pending_id returned by a records_* mutation");

    server.registerTool(
      "write_confirm",
      {
        title: "Confirm pending write",
        description:
          "Executes a record mutation previously parked by records_create/update/delete. Confirming charges the write budget.",
        inputSchema: { pending_id: pendingId },
      },
      toolHandler("write_confirm", (args) => {
        const pending = ctx.guards.confirm(args.pending_id);
        return pending.execute();
      }),
    );

    server.registerTool(
      "write_cancel",
      {
        title: "Cancel pending write",
        description:
          "Discards a parked record mutation without executing it or charging the budget.",
        inputSchema: { pending_id: pendingId },
      },
      toolHandler("write_cancel", (args) => {
        const pending = ctx.guards.cancel(args.pending_id);
        return Promise.resolve(
          textResult(`Cancelled without executing: ${pending.summary}`),
        );
      }),
    );
  }
}
