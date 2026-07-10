/// Audit log: one JSON line per tool invocation on stderr. stdout is reserved
/// for the MCP stdio transport.

const MAX_ARGS_LENGTH = 2048;

export interface AuditEvent {
  ts: string;
  tool: string;
  args: string;
  ok: boolean;
  error?: string;
  durationMs: number;
}

function serializeArgs(args: unknown): string {
  let serialized: string;
  try {
    serialized = JSON.stringify(args) ?? "null";
  } catch {
    serialized = "<unserializable>";
  }
  if (serialized.length > MAX_ARGS_LENGTH) {
    return `${serialized.slice(0, MAX_ARGS_LENGTH)}…(truncated)`;
  }
  return serialized;
}

export function auditLog(event: AuditEvent): void {
  process.stderr.write(`${JSON.stringify(event)}\n`);
}

/// Wraps a tool handler so every invocation (including failures) is logged.
export async function withAudit<T>(
  tool: string,
  args: unknown,
  fn: () => Promise<T>,
): Promise<T> {
  const start = performance.now();
  try {
    const result = await fn();
    auditLog({
      ts: new Date().toISOString(),
      tool,
      args: serializeArgs(args),
      ok: true,
      durationMs: Math.round(performance.now() - start),
    });
    return result;
  } catch (err) {
    auditLog({
      ts: new Date().toISOString(),
      tool,
      args: serializeArgs(args),
      ok: false,
      error: err instanceof Error ? err.message : String(err),
      durationMs: Math.round(performance.now() - start),
    });
    throw err;
  }
}
