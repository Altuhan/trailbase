#!/usr/bin/env node
// End-to-end drill for the built-in remote backups against a real `trail`
// binary, without any network: the "bucket" is a local directory served by
// the LocalFileSystem object-store backend (TB_BACKUP_FS_DIR).
//
// Flow: seed a depot (admin via CLI, tenant databases as files) → start the
// server with backups enabled → trigger the "Remote Backup" job through the
// admin API → assert latest/ and epochs/<today>/ objects → write more rows,
// re-run, assert re-upload → stop the server and restore a tenant database
// from the bucket, verifying integrity and row counts.
//
// Usage: node tools/backup-e2e.mjs [--bin target/debug/trail] [--keep]

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

const ADMIN_EMAIL = "e2e-admin@localhost";
const ADMIN_PASSWORD = "e2e-password-1";
const HEALTH_TIMEOUT_MS = 60_000;

function fail(message) {
  console.error(`backup-e2e: FAIL: ${message}`);
  process.exit(1);
}

function parseArgs(argv) {
  const args = { bin: "target/debug/trail", keep: false };
  for (let i = 2; i < argv.length; i++) {
    switch (argv[i]) {
      case "--bin":
        args.bin = argv[++i];
        break;
      case "--keep":
        args.keep = true;
        break;
      default:
        fail(`unknown argument '${argv[i]}'`);
    }
  }
  return args;
}

function runCli(bin, cliArgs) {
  const result = spawnSync(bin, cliArgs, { encoding: "utf8" });
  if (result.status !== 0) {
    fail(`'${bin} ${cliArgs.join(" ")}' exited ${result.status}:\n${result.stderr}`);
  }
}

async function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        server.close(() => reject(new Error("no port")));
        return;
      }
      server.close(() => resolvePort(address.port));
    });
  });
}

async function waitHealthy(url, child) {
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      fail(`server exited early with code ${child.exitCode}`);
    }
    try {
      if ((await fetch(`${url}/api/healthcheck`)).ok) return;
    } catch {
      // Not up yet.
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  fail("server did not become healthy in time");
}

function seedTenantDb(path, rows) {
  const db = new DatabaseSync(path);
  db.exec("PRAGMA journal_mode=WAL");
  db.exec(
    "CREATE TABLE sales (id INTEGER PRIMARY KEY, item TEXT NOT NULL, amount INTEGER NOT NULL)",
  );
  const insert = db.prepare("INSERT INTO sales (id, item, amount) VALUES (?, ?, ?)");
  for (let i = 1; i <= rows; i++) {
    insert.run(i, `item-${i}`, (i * 13) % 100);
  }
  db.close();
}

function appendRows(path, from, to) {
  const db = new DatabaseSync(path);
  const insert = db.prepare("INSERT INTO sales (id, item, amount) VALUES (?, ?, ?)");
  for (let i = from; i <= to; i++) {
    insert.run(i, `item-${i}`, (i * 13) % 100);
  }
  db.close();
}

function countRows(path) {
  const db = new DatabaseSync(path, { readOnly: true });
  try {
    const integrity = db.prepare("PRAGMA integrity_check").get();
    const check = Object.values(integrity)[0];
    if (check !== "ok") fail(`integrity_check: ${check}`);
    const row = db.prepare("SELECT COUNT(*) AS n FROM sales").get();
    return Number(row.n);
  } finally {
    db.close();
  }
}

function walk(dir, prefix = "") {
  if (!existsSync(dir)) return [];
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      out.push(...walk(join(dir, entry.name), rel));
    } else {
      out.push(rel);
    }
  }
  return out.sort();
}

async function main() {
  const args = parseArgs(process.argv);
  const depot = mkdtempSync(join(tmpdir(), "tb-e2e-depot-"));
  const bucket = mkdtempSync(join(tmpdir(), "tb-e2e-bucket-"));
  const today = new Date().toISOString().slice(0, 10);
  let child;

  try {
    // Depot with an admin (also runs migrations/keys) and two tenant DBs.
    runCli(args.bin, ["--data-dir", depot, "user", "add", ADMIN_EMAIL, ADMIN_PASSWORD]);
    runCli(args.bin, ["--data-dir", depot, "admin", "promote", ADMIN_EMAIL]);
    seedTenantDb(join(depot, "data", "tenant_a.db"), 100);
    seedTenantDb(join(depot, "data", "tenant_b.db"), 50);

    const port = await freePort();
    const url = `http://127.0.0.1:${port}`;
    child = spawn(
      args.bin,
      ["--data-dir", depot, "run", `--address=127.0.0.1:${port}`],
      {
        stdio: ["ignore", "ignore", "pipe"],
        env: {
          ...process.env,
          TB_BACKUP_FS_DIR: bucket,
          TB_CONN_CACHE_CAPACITY: "2",
        },
      },
    );
    let serverStderr = "";
    child.stderr.on("data", (chunk) => {
      serverStderr = (serverStderr + chunk.toString()).slice(-8192);
    });
    await waitHealthy(url, child);

    // Admin session.
    const login = await fetch(`${url}/api/auth/v1/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email: ADMIN_EMAIL, password: ADMIN_PASSWORD }),
    });
    if (!login.ok) fail(`login: HTTP ${login.status}`);
    const tokens = await login.json();
    const claims = JSON.parse(
      Buffer.from(tokens.auth_token.split(".")[1], "base64url").toString("utf8"),
    );
    const adminHeaders = {
      Authorization: `Bearer ${tokens.auth_token}`,
      "Content-Type": "application/json",
      ...(claims.csrf_token ? { "CSRF-Token": claims.csrf_token } : {}),
    };

    // Find and trigger the "Remote Backup" job.
    const jobsResponse = await fetch(`${url}/api/_admin/jobs`, {
      headers: adminHeaders,
    });
    if (!jobsResponse.ok) fail(`jobs: HTTP ${jobsResponse.status}`);
    const jobs = (await jobsResponse.json()).jobs ?? [];
    const backupJob = jobs.find((j) => j.name === "Remote Backup");
    if (!backupJob) {
      fail(
        `no "Remote Backup" job registered; jobs: ${jobs.map((j) => j.name).join(", ")}\n` +
          `server stderr tail:\n${serverStderr}`,
      );
    }

    const runJob = async () => {
      const response = await fetch(`${url}/api/_admin/job/run`, {
        method: "POST",
        headers: adminHeaders,
        body: JSON.stringify({ id: backupJob.id }),
      });
      if (!response.ok) fail(`job/run: HTTP ${response.status}`);
    };

    await runJob();

    const expected = [
      `epochs/${today}/main.db`,
      `epochs/${today}/tenant_a.db`,
      `epochs/${today}/tenant_b.db`,
      "latest/main.db",
      "latest/tenant_a.db",
      "latest/tenant_b.db",
    ];
    {
      const objects = walk(bucket);
      const missing = expected.filter((e) => !objects.includes(e));
      if (missing.length > 0) {
        fail(`missing objects after sweep: ${missing.join(", ")}; got: ${objects.join(", ")}`);
      }
      console.log(`backup-e2e: sweep OK (${objects.length} objects)`);
    }

    // Restored copies must carry the seeded data even while the source
    // keeps running (snapshot isolation).
    if (countRows(join(bucket, "latest", "tenant_a.db")) !== 100) {
      fail("latest/tenant_a.db row count mismatch");
    }

    // New writes → the file is dirty again → re-run re-uploads it.
    const sizeBefore = statSync(join(bucket, "latest", "tenant_a.db")).size;
    appendRows(join(depot, "data", "tenant_a.db"), 101, 400);
    await new Promise((r) => setTimeout(r, 20));
    await runJob();
    const restoredCount = countRows(join(bucket, "latest", "tenant_a.db"));
    if (restoredCount !== 400) {
      fail(`expected re-uploaded tenant_a with 400 rows, got ${restoredCount}`);
    }
    const sizeAfter = statSync(join(bucket, "latest", "tenant_a.db")).size;
    console.log(
      `backup-e2e: re-upload OK (rows 100 -> 400, bytes ${sizeBefore} -> ${sizeAfter})`,
    );

    // Restore drill: server down, bucket object becomes the new data file.
    child.kill("SIGTERM");
    await new Promise((r) => setTimeout(r, 500));
    if (child.exitCode === null) child.kill("SIGKILL");
    child = undefined;

    const restoreDepot = mkdtempSync(join(tmpdir(), "tb-e2e-restore-"));
    try {
      const { mkdirSync, copyFileSync } = await import("node:fs");
      mkdirSync(join(restoreDepot, "data"), { recursive: true });
      copyFileSync(
        join(bucket, "latest", "tenant_a.db"),
        join(restoreDepot, "data", "tenant_a.db"),
      );
      const restored = countRows(join(restoreDepot, "data", "tenant_a.db"));
      if (restored !== 400) fail(`restored row count ${restored} != 400`);
      console.log("backup-e2e: restore OK (integrity ok, 400 rows)");
    } finally {
      if (!args.keep) rmSync(restoreDepot, { recursive: true, force: true });
    }

    console.log("backup-e2e: PASS");
  } finally {
    if (child && child.exitCode === null) {
      child.kill("SIGKILL");
    }
    if (args.keep) {
      console.log(`backup-e2e: kept depot=${depot} bucket=${bucket}`);
    } else {
      rmSync(depot, { recursive: true, force: true });
      rmSync(bucket, { recursive: true, force: true });
    }
  }
}

await main();
