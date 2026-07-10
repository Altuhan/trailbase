import type { Mode } from "./config";

export const TIERS = [
  "records",
  "admin-read",
  "admin-write",
  "sandbox-mgmt",
] as const;

/// Coarse capability tiers tools belong to:
///  - "records": record API CRUD + schema, enforced server-side by ACLs.
///  - "admin-read": read-only admin endpoints (tables, config, logs, jobs).
///  - "admin-write": arbitrary SQL, DDL (migration-recording endpoints),
///    config updates, user management.
///  - "sandbox-mgmt": creating/inspecting/destroying ephemeral sandbox
///    instances snapshotted from the live depot.
export type ToolTier = (typeof TIERS)[number];

const MATRIX: Record<Mode, readonly ToolTier[]> = {
  "prod-safe": ["records", "sandbox-mgmt"],
  "prod-admin-readonly": ["records", "admin-read", "sandbox-mgmt"],
  // In sandbox mode every data/admin tool is routed to the ephemeral sandbox
  // instance created via sandbox_create; the configured URL is only ever used
  // as the snapshot source. This makes it structurally impossible for
  // arbitrary SQL/DDL to reach production.
  sandbox: ["records", "admin-read", "admin-write", "sandbox-mgmt"],
};

export function enabledTiers(mode: Mode): ReadonlySet<ToolTier> {
  return new Set(MATRIX[mode]);
}

export function isTierEnabled(mode: Mode, tier: ToolTier): boolean {
  return enabledTiers(mode).has(tier);
}
