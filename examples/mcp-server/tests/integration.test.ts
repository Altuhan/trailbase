import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { loadConfig } from "../src/config";
import { connect } from "../src/trailbase";
import { newToolContext, type ToolContext } from "../src/tools/common";
import { SandboxManager } from "../src/sandbox";

// Resolve the `trail` binary. Skips the whole suite when it is unavailable so
// CI without a Rust build (and `pnpm test` on its own) stays green.
function resolveTrailBin(): string | undefined {
  const envBin = process.env.TRAIL_BIN;
  if (envBin && existsSync(envBin)) return envBin;

  const repoRoot = resolve(__dirname, "../../..");
  for (const profile of ["debug", "release"]) {
    const candidate = join(repoRoot, "target", profile, "trail");
    if (existsSync(candidate)) return candidate;
  }
  return undefined;
}

const trailBin = resolveTrailBin();
const maybe = trailBin ? describe : describe.skip;

// A minimal source depot: an empty data dir that `trail` initializes on first
// use (running any `trail` subcommand applies the baseline migrations).
function createSourceDepot(trail: string): string {
  const dir = mkdtempSync(join(tmpdir(), "tb-mcp-e2e-src-"));
  // `user add` runs AppState::init which creates data/main.db and applies the
  // embedded baseline migrations, giving us a valid depot to snapshot.
  execFileSync(trail, ["--data-dir", dir, "user", "add", "seed@localhost", "seed-password-123"], {
    stdio: "ignore",
  });
  return dir;
}

maybe("sandbox end-to-end", () => {
  let sourceDir: string;
  let ctx: ToolContext;

  beforeAll(() => {
    sourceDir = createSourceDepot(trailBin!);
    const config = loadConfig({
      TRAILBASE_MODE: "sandbox",
      TRAILBASE_DATA_DIR: sourceDir,
      TRAIL_BIN: trailBin!,
    });
    ctx = newToolContext(config, connect);
  });

  afterAll(async () => {
    if (ctx?.sandbox.isActive()) {
      await ctx.sandbox.destroy();
    }
    if (sourceDir) rmSync(sourceDir, { recursive: true, force: true });
  });

  test(
    "create → DDL records a migration → diff surfaces it → CRUD → destroy",
    async () => {
      const sandbox: SandboxManager = ctx.sandbox;

      const manifest = await sandbox.create();
      expect(manifest.url).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
      expect((await sandbox.status()).healthy).toBe(true);

      // DDL via the admin endpoint should record a migration file.
      const admin = sandbox.adminClient();
      const created = (await admin.sendJson("POST", "/table", {
        schema: {
          name: { name: "note", database_schema: null },
          strict: false,
          columns: [
            {
              name: "id",
              data_type: "Integer",
              options: [{ Unique: { is_primary: true } }],
            },
            { name: "body", data_type: "Text", options: [] },
          ],
          foreign_keys: [],
          unique: [],
          checks: [],
          virtual_table: false,
          temporary: false,
        },
        dry_run: false,
      })) as { sql: string };
      expect(created.sql).toContain("note");

      const diff = await sandbox.diff();
      expect(diff.newMigrations.length).toBeGreaterThanOrEqual(1);
      expect(diff.newMigrations.some((m) => /CREATE TABLE/i.test(m.content))).toBe(true);

      // CRUD via the record API requires a configured record API; instead we
      // assert the DDL landed by querying it back over the admin SQL endpoint.
      await admin.execQuery("INSERT INTO note (body) VALUES ('hello')");
      const rows = (await admin.execQuery("SELECT body FROM note")) as {
        rows: unknown[][];
      };
      expect(rows.rows).toEqual([["hello"]]);

      const destroyed = await sandbox.destroy();
      expect(destroyed.removed).toBe(true);
      expect(existsSync(manifest.dataDir)).toBe(false);
      expect(sandbox.isActive()).toBe(false);
    },
    120_000,
  );
});
