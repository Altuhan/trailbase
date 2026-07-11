import { randomUUID } from "node:crypto";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";

import type { Config } from "./config";

/// Write guards for prod modes: a per-process mutation budget and a
/// two-phase (propose/confirm) flow for record mutations, plus column
/// redaction for read results. The guard design (session budgets, pending
/// actions with TTL, column redaction) is adapted from the
/// applix-fr/mcp-trailbase project.

const MAX_PENDING = 10;

export class GuardError extends Error {}

export interface PendingWrite {
  readonly id: string;
  readonly tool: string;
  readonly summary: string;
  readonly execute: () => Promise<CallToolResult>;
  readonly expiresAt: number;
}

export class Guards {
  /// Whether writes are guarded at all; sandbox instances are disposable
  /// snapshots, so budget and confirmation only apply to prod modes.
  readonly active: boolean;
  readonly confirmWrites: boolean;
  readonly confirmTimeoutSecs: number;

  /// Remaining write budget; null means unlimited.
  private remaining: number | null;
  private readonly pending = new Map<string, PendingWrite>();

  constructor(config: Config) {
    this.active = config.mode !== "sandbox";
    this.confirmWrites = this.active && config.confirmWrites;
    this.confirmTimeoutSecs = config.confirmTimeoutSecs;
    this.remaining =
      this.active && config.budgetWrites > 0 ? config.budgetWrites : null;
  }

  budgetRemaining(): number | null {
    return this.remaining;
  }

  /// Consumes one unit of write budget or throws when exhausted.
  useBudget(): void {
    if (this.remaining === null) {
      return;
    }
    if (this.remaining === 0) {
      throw new GuardError(
        "Write budget for this session is exhausted. A human can raise " +
          "TRAILBASE_BUDGET_WRITES (0 = unlimited) and restart the server.",
      );
    }
    this.remaining -= 1;
  }

  /// Parks a mutation for explicit confirmation and returns its handle.
  propose(
    tool: string,
    summary: string,
    execute: () => Promise<CallToolResult>,
  ): PendingWrite {
    this.purgeExpired();
    if (this.pending.size >= MAX_PENDING) {
      throw new GuardError(
        `Too many unconfirmed writes (max ${MAX_PENDING}); confirm or ` +
          "cancel pending ones first.",
      );
    }
    const entry: PendingWrite = {
      id: randomUUID(),
      tool,
      summary,
      execute,
      expiresAt: Date.now() + this.confirmTimeoutSecs * 1000,
    };
    this.pending.set(entry.id, entry);
    return entry;
  }

  /// Removes and returns a pending write, charging the budget. The caller
  /// runs `execute()`; a failed execution does not refund the budget.
  confirm(pendingId: string): PendingWrite {
    const entry = this.take(pendingId);
    this.useBudget();
    return entry;
  }

  cancel(pendingId: string): PendingWrite {
    return this.take(pendingId);
  }

  private take(pendingId: string): PendingWrite {
    this.purgeExpired();
    const entry = this.pending.get(pendingId);
    if (entry === undefined) {
      throw new GuardError(
        `No pending write with id '${pendingId}'; it may have expired ` +
          `(timeout ${this.confirmTimeoutSecs}s), been confirmed or been ` +
          "cancelled already.",
      );
    }
    this.pending.delete(pendingId);
    return entry;
  }

  private purgeExpired(): void {
    const now = Date.now();
    for (const [id, entry] of this.pending) {
      if (entry.expiresAt <= now) {
        this.pending.delete(id);
      }
    }
  }
}

/// Recursively masks values of columns whose name matches any pattern,
/// including rows nested through foreign-key `expand`.
export function redactData(
  data: unknown,
  patterns: readonly RegExp[],
): unknown {
  if (patterns.length === 0) {
    return data;
  }
  return walk(data, patterns);
}

function walk(value: unknown, patterns: readonly RegExp[]): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => walk(item, patterns));
  }
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, val]) => [
        key,
        patterns.some((p) => p.test(key)) ? "[REDACTED]" : walk(val, patterns),
      ]),
    );
  }
  return value;
}
