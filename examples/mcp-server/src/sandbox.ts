import { randomBytes } from "node:crypto";
import {
  cpSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { ChildProcess } from "node:child_process";

import spawn from "nano-spawn";
import { initClient, type Client } from "trailbase";

import { AdminClient } from "./admin-client";
import type { Config } from "./config";

const SANDBOX_ADMIN_EMAIL = "sandbox-admin@localhost";
const HEALTH_TIMEOUT_MS = 60_000;
const MANIFEST_FILE = ".sandbox.json";

export class SandboxError extends Error {}

export interface SandboxManifest {
  createdAt: string;
  sourceDataDir: string;
  dataDir: string;
  url: string;
  pid: number | undefined;
  adminEmail: string;
  /// Migration files (relative to `migrations/main`) present at creation.
  baselineMigrations: string[];
}

export interface SandboxDiff {
  newMigrations: { file: string; content: string }[];
  configChanged: boolean;
  configBefore?: string;
  configAfter?: string;
}

interface ActiveSandbox {
  manifest: SandboxManifest;
  child: ChildProcess;
  client: Client;
  baselineConfig: string | undefined;
}

function sqlQuoteSingle(value: string): string {
  return `'${value.replaceAll("'", "''")}'`;
}

/// Takes a transactionally consistent snapshot of a (potentially live, WAL)
/// SQLite database using `VACUUM INTO`. Prefers the built-in `node:sqlite`
/// and falls back to the `sqlite3` CLI.
export async function snapshotSqliteDb(source: string, dest: string): Promise<void> {
  const vacuum = `VACUUM INTO ${sqlQuoteSingle(dest)}`;

  let nodeSqliteError: unknown;
  try {
    const { DatabaseSync } = await import("node:sqlite");
    const db = new DatabaseSync(source, { readOnly: true });
    try {
      db.exec(vacuum);
      return;
    } finally {
      db.close();
    }
  } catch (err) {
    nodeSqliteError = err;
  }

  try {
    await spawn("sqlite3", [source, vacuum]);
    return;
  } catch (cliError) {
    throw new SandboxError(
      `Failed to snapshot '${source}': node:sqlite failed with '${nodeSqliteError}' and the sqlite3 CLI fallback failed with '${cliError}'.`,
    );
  }
}

function listMainMigrations(dataDir: string): string[] {
  const dir = join(dataDir, "migrations", "main");
  if (!existsSync(dir)) {
    return [];
  }
  return readdirSync(dir)
    .filter((f) => f.endsWith(".sql"))
    .sort();
}

function readConfig(dataDir: string): string | undefined {
  const path = join(dataDir, "config.textproto");
  return existsSync(path) ? readFileSync(path, "utf8") : undefined;
}

/// Assembles a self-contained sandbox depot from a source depot: consistent
/// main.db snapshot + config + migrations. Session/logs/queue databases and
/// secrets are intentionally NOT copied — they are recreated fresh, so
/// production sessions, signing keys and tokens never leak into the sandbox.
export async function assembleSandboxDepot(
  sourceDataDir: string,
  sandboxDir: string,
): Promise<{ baselineMigrations: string[]; baselineConfig: string | undefined }> {
  const sourceDb = join(sourceDataDir, "data", "main.db");
  if (!existsSync(sourceDb)) {
    throw new SandboxError(
      `'${sourceDb}' does not exist; is '${sourceDataDir}' a TrailBase data dir?`,
    );
  }

  mkdirSync(join(sandboxDir, "data"), { recursive: true });
  await snapshotSqliteDb(sourceDb, join(sandboxDir, "data", "main.db"));

  const sourceConfig = join(sourceDataDir, "config.textproto");
  if (existsSync(sourceConfig)) {
    copyFileSync(sourceConfig, join(sandboxDir, "config.textproto"));
  }

  const sourceMigrations = join(sourceDataDir, "migrations");
  if (existsSync(sourceMigrations)) {
    cpSync(sourceMigrations, join(sandboxDir, "migrations"), { recursive: true });
  }

  return {
    baselineMigrations: listMainMigrations(sandboxDir),
    baselineConfig: readConfig(sandboxDir),
  };
}

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        server.close(() => reject(new SandboxError("Could not allocate a port.")));
        return;
      }
      server.close(() => resolvePort(address.port));
    });
  });
}

async function waitHealthy(url: string, child: ChildProcess): Promise<void> {
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new SandboxError(
        `Sandbox server exited early with code ${child.exitCode}; see the sandbox's server.log.`,
      );
    }
    try {
      const response = await fetch(`${url}/api/healthcheck`);
      if (response.ok) {
        return;
      }
    } catch {
      // Not up yet.
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new SandboxError("Sandbox server did not become healthy in time.");
}

/// Owns at most one ephemeral sandbox instance spawned from a snapshot of the
/// live depot. All admin-write tools operate on this instance, never on the
/// configured production URL.
export class SandboxManager {
  private active: ActiveSandbox | undefined;
  private exitHook: (() => void) | undefined;

  constructor(private readonly config: Config) {}

  isActive(): boolean {
    return this.active !== undefined;
  }

  /// URL of the running sandbox; throws when none is active.
  url(): string {
    return this.require().manifest.url;
  }

  /// Admin client against the running sandbox, authenticated as the
  /// generated sandbox admin.
  adminClient(): AdminClient {
    const active = this.require();
    const token = active.client.tokens()?.auth_token;
    if (token === undefined) {
      throw new SandboxError("Sandbox admin session lost its tokens.");
    }
    return AdminClient.fromToken(active.manifest.url, token);
  }

  private require(): ActiveSandbox {
    const active = this.active;
    if (active === undefined) {
      throw new SandboxError("No active sandbox. Create one with sandbox_create.");
    }
    return active;
  }

  async create(opts: { sourceDataDir?: string } = {}): Promise<SandboxManifest> {
    if (this.active !== undefined) {
      throw new SandboxError(
        `A sandbox is already running at ${this.active.manifest.url}; destroy it first (sandbox_destroy).`,
      );
    }

    const sourceDataDir = opts.sourceDataDir ?? this.config.dataDir;
    if (sourceDataDir === undefined) {
      throw new SandboxError(
        "No source data dir. Set TRAILBASE_DATA_DIR or pass source_data_dir.",
      );
    }
    const source = resolve(sourceDataDir);
    const sandboxDir = mkdtempSync(join(tmpdir(), "trailbase-sandbox-"));

    try {
      const { baselineMigrations, baselineConfig } = await assembleSandboxDepot(
        source,
        sandboxDir,
      );

      // `trail user add`/`admin promote` run AppState::init, which also
      // applies pending migrations and generates fresh keys.
      const adminPassword = randomBytes(24).toString("base64url");
      const trail = this.config.trailBin;
      await spawn(trail, ["--data-dir", sandboxDir, "user", "add", SANDBOX_ADMIN_EMAIL, adminPassword]);
      await spawn(trail, ["--data-dir", sandboxDir, "admin", "promote", SANDBOX_ADMIN_EMAIL]);

      const port = await freePort();
      const address = `127.0.0.1:${port}`;
      const url = `http://${address}`;

      const subprocess = spawn(
        trail,
        ["--data-dir", sandboxDir, "run", `--address=${address}`],
        { stdout: "ignore", stderr: "ignore" },
      );
      const child = await subprocess.nodeChildProcess;

      try {
        await waitHealthy(url, child);

        const client = initClient(url);
        const mfa = await client.login(SANDBOX_ADMIN_EMAIL, adminPassword);
        if (mfa !== undefined) {
          throw new SandboxError("Unexpected MFA challenge for the sandbox admin.");
        }

        const manifest: SandboxManifest = {
          createdAt: new Date().toISOString(),
          sourceDataDir: source,
          dataDir: sandboxDir,
          url,
          pid: child.pid,
          adminEmail: SANDBOX_ADMIN_EMAIL,
          baselineMigrations,
        };
        writeFileSync(join(sandboxDir, MANIFEST_FILE), JSON.stringify(manifest, null, 2));

        this.active = { manifest, child, client, baselineConfig };
        this.exitHook = () => child.kill("SIGTERM");
        process.once("exit", this.exitHook);
        return manifest;
      } catch (err) {
        child.kill("SIGTERM");
        throw err;
      }
    } catch (err) {
      rmSync(sandboxDir, { recursive: true, force: true });
      throw err;
    }
  }

  async status(): Promise<{
    active: boolean;
    manifest?: SandboxManifest;
    healthy?: boolean;
  }> {
    const active = this.active;
    if (active === undefined) {
      return { active: false };
    }
    let healthy = false;
    try {
      healthy = (await fetch(`${active.manifest.url}/api/healthcheck`)).ok;
    } catch {
      // Unreachable counts as unhealthy.
    }
    return { active: true, manifest: active.manifest, healthy };
  }

  /// Reports what changed relative to sandbox creation: newly recorded
  /// migration files (the reviewable artifact to apply to production) and
  /// config.textproto changes.
  async diff(): Promise<SandboxDiff> {
    const active = this.require();
    const { manifest, baselineConfig } = active;

    const baseline = new Set(manifest.baselineMigrations);
    const newMigrations = listMainMigrations(manifest.dataDir)
      .filter((file) => !baseline.has(file))
      .map((file) => ({
        file: join("migrations", "main", file),
        content: readFileSync(join(manifest.dataDir, "migrations", "main", file), "utf8"),
      }));

    const configAfter = readConfig(manifest.dataDir);
    const configChanged = configAfter !== baselineConfig;

    return {
      newMigrations,
      configChanged,
      ...(configChanged ? { configBefore: baselineConfig, configAfter } : {}),
    };
  }

  async destroy(opts: { keepDir?: boolean } = {}): Promise<{ dataDir: string; removed: boolean }> {
    const active = this.require();
    const { manifest, child } = active;

    child.kill("SIGTERM");
    // Give the server a moment for graceful shutdown, then make sure.
    await new Promise((r) => setTimeout(r, 500));
    if (child.exitCode === null) {
      child.kill("SIGKILL");
    }

    if (this.exitHook !== undefined) {
      process.removeListener("exit", this.exitHook);
      this.exitHook = undefined;
    }
    this.active = undefined;

    const removed = !(opts.keepDir ?? false);
    if (removed) {
      rmSync(manifest.dataDir, { recursive: true, force: true });
    }
    return { dataDir: manifest.dataDir, removed };
  }
}
