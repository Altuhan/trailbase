import { z } from "zod";

export const MODES = ["prod-safe", "prod-admin-readonly", "sandbox"] as const;

/// Access mode the MCP server runs in:
///  - "prod-safe": record APIs only. Enforcement happens server-side via
///    TrailBase's Record API ACLs for the configured (non-admin) user.
///  - "prod-admin-readonly": additionally exposes read-only admin tools.
///    This is MCP-side policy, NOT a server-side guarantee.
///  - "sandbox": the configured instance is treated as disposable; all tools
///    including arbitrary SQL and DDL are exposed.
export type Mode = (typeof MODES)[number];

const envSchema = z.object({
  TRAILBASE_URL: z.string().default("http://localhost:4000"),
  TRAILBASE_MODE: z.enum(MODES).default("prod-safe"),
  TRAILBASE_USER: z.string().optional(),
  TRAILBASE_PASSWORD: z.string().optional(),
  TRAILBASE_AUTH_TOKEN: z.string().optional(),
  TRAILBASE_REFRESH_TOKEN: z.string().optional(),
  TRAILBASE_ADMIN_TOKEN: z.string().optional(),
  TRAILBASE_DATA_DIR: z.string().optional(),
  TRAIL_BIN: z.string().default("trail"),
  TRAILBASE_BUDGET_WRITES: z.coerce.number().int().nonnegative().default(100),
  TRAILBASE_REDACT_COLUMNS: z.string().default(""),
  TRAILBASE_CONFIRM_WRITES: z
    .enum(["true", "false"])
    .transform((v) => v === "true")
    .optional(),
  TRAILBASE_CONFIRM_TIMEOUT_SECS: z.coerce
    .number()
    .int()
    .positive()
    .default(120),
});

export interface Config {
  /// Base URL of the TrailBase instance.
  url: string;
  mode: Mode;
  /// Credentials for record APIs: either email/username + password, or
  /// pre-issued tokens. Tokens take precedence when both are set.
  user?: string;
  password?: string;
  authToken?: string;
  refreshToken?: string;
  /// Admin credentials for admin tools ("prod-admin-readonly"/"sandbox").
  /// Falls back to the record credentials when unset.
  adminToken?: string;
  /// Path to the live instance's data dir (traildepot). Required for
  /// snapshot-based sandbox creation.
  dataDir?: string;
  /// The `trail` binary used to prepare and run sandbox instances.
  trailBin: string;
  /// Number of record mutations (create/update/delete) allowed per server
  /// process in prod modes; 0 means unlimited. Ignored in sandbox mode.
  budgetWrites: number;
  /// Case-insensitive patterns matched against column names; matching
  /// columns are masked in records_list/records_read results.
  redactColumns: readonly RegExp[];
  /// Whether prod-mode record mutations require a write_confirm round-trip.
  confirmWrites: boolean;
  /// Seconds until a pending (unconfirmed) write expires.
  confirmTimeoutSecs: number;
}

export class ConfigError extends Error {}

export function loadConfig(
  env: Record<string, string | undefined> = process.env,
): Config {
  const parsed = envSchema.safeParse(env);
  if (!parsed.success) {
    const issues = parsed.error.issues
      .map((issue) => `${issue.path.join(".")}: ${issue.message}`)
      .join("; ");
    throw new ConfigError(`Invalid environment: ${issues}`);
  }
  const e = parsed.data;

  let url: URL;
  try {
    url = new URL(e.TRAILBASE_URL);
  } catch {
    throw new ConfigError(
      `TRAILBASE_URL is not a valid URL: '${e.TRAILBASE_URL}'`,
    );
  }

  if (
    (e.TRAILBASE_USER === undefined) !==
    (e.TRAILBASE_PASSWORD === undefined)
  ) {
    throw new ConfigError(
      "TRAILBASE_USER and TRAILBASE_PASSWORD must be set together.",
    );
  }

  const redactColumns = e.TRAILBASE_REDACT_COLUMNS.split(",")
    .map((p) => p.trim())
    .filter((p) => p !== "")
    .map((p) => {
      try {
        return new RegExp(p, "i");
      } catch {
        throw new ConfigError(
          `TRAILBASE_REDACT_COLUMNS contains an invalid pattern: '${p}'`,
        );
      }
    });

  return {
    url: url.toString().replace(/\/$/, ""),
    mode: e.TRAILBASE_MODE,
    user: e.TRAILBASE_USER,
    password: e.TRAILBASE_PASSWORD,
    authToken: e.TRAILBASE_AUTH_TOKEN,
    refreshToken: e.TRAILBASE_REFRESH_TOKEN,
    adminToken: e.TRAILBASE_ADMIN_TOKEN,
    dataDir: e.TRAILBASE_DATA_DIR,
    trailBin: e.TRAIL_BIN,
    budgetWrites: e.TRAILBASE_BUDGET_WRITES,
    redactColumns,
    // Confirmation defaults on for modes that can reach production data and
    // off for disposable sandboxes; the env var overrides either way.
    confirmWrites: e.TRAILBASE_CONFIRM_WRITES ?? e.TRAILBASE_MODE !== "sandbox",
    confirmTimeoutSecs: e.TRAILBASE_CONFIRM_TIMEOUT_SECS,
  };
}
