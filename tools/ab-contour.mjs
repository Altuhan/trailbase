#!/usr/bin/env node
// A/B regression contour: runs one `trail` binary through a deterministic
// functional scenario (golden log) and a latency/throughput/memory probe,
// then writes a machine-readable JSON report.
//
// The same script is run against two binaries — A (baseline, e.g. `main`)
// and B (feature branch) — and the reports are compared:
//   * `<out>.golden.jsonl` must be byte-identical between A and B;
//   * metrics must stay within the tolerances checked by `--compare`.
//
// Usage:
//   node tools/ab-contour.mjs --bin target/debug/trail --out report-A.json \
//     [--label A] [--server-env K=V]... [--keep]
//   node tools/ab-contour.mjs --compare report-A.json report-B.json
//
// Requirements: node >= 22 (built-in fetch), no npm dependencies.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const ADMIN_EMAIL = "contour-admin@localhost";
// Deterministic on purpose: the contour only ever runs against throwaway
// depots created seconds earlier.
const ADMIN_PASSWORD = "contour-password-1";
const HEALTH_TIMEOUT_MS = 60_000;

const LATENCY_WARMUP = 100;
const LATENCY_SAMPLES = 500;
const QUERY_SAMPLES = 250;
const THROUGHPUT_WORKERS = 8;
const THROUGHPUT_MS = 3_000;

// Tolerances for `--compare` (fractions of the A value).
const LATENCY_TOLERANCE = 0.05;
const RSS_TOLERANCE = 0.05;

function fail(message) {
  console.error(`ab-contour: ${message}`);
  process.exit(1);
}

function parseArgs(argv) {
  const args = {
    serverEnv: {},
    keep: false,
    label: undefined,
    bin: undefined,
    out: undefined,
    compare: undefined,
  };
  for (let i = 2; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case "--bin":
        args.bin = argv[++i];
        break;
      case "--out":
        args.out = argv[++i];
        break;
      case "--label":
        args.label = argv[++i];
        break;
      case "--keep":
        args.keep = true;
        break;
      case "--server-env": {
        const kv = argv[++i] ?? "";
        const eq = kv.indexOf("=");
        if (eq <= 0) fail(`invalid --server-env '${kv}', expected K=V`);
        args.serverEnv[kv.slice(0, eq)] = kv.slice(eq + 1);
        break;
      }
      case "--compare":
        args.compare = [argv[++i], argv[++i]];
        break;
      default:
        fail(`unknown argument '${arg}'`);
    }
  }
  return args;
}

/// JSON.stringify with recursively sorted object keys — stable golden lines.
function stable(value) {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value) {
  if (Array.isArray(value)) {
    return value.map(sortKeys);
  }
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((k) => [k, sortKeys(value[k])]),
    );
  }
  return value;
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil((p / 100) * sorted.length) - 1),
  );
  return sorted[idx];
}

function stats(samplesUs) {
  const sorted = [...samplesUs].sort((a, b) => a - b);
  const sum = sorted.reduce((a, b) => a + b, 0);
  return {
    count: sorted.length,
    mean_us: Math.round(sum / sorted.length),
    p50_us: Math.round(percentile(sorted, 50)),
    p95_us: Math.round(percentile(sorted, 95)),
    p99_us: Math.round(percentile(sorted, 99)),
    max_us: Math.round(sorted[sorted.length - 1]),
  };
}

async function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        server.close(() => reject(new Error("could not allocate a port")));
        return;
      }
      server.close(() => resolvePort(address.port));
    });
  });
}

function runCli(bin, args) {
  const result = spawnSync(bin, args, { encoding: "utf8" });
  if (result.status !== 0) {
    fail(
      `'${bin} ${args.join(" ")}' exited with ${result.status}:\n${result.stderr}`,
    );
  }
}

async function waitHealthy(url, child) {
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      fail(`server exited early with code ${child.exitCode}`);
    }
    try {
      const response = await fetch(`${url}/api/healthcheck`);
      if (response.ok) return;
    } catch {
      // Not up yet.
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  fail("server did not become healthy in time");
}

function csrfFromJwt(token) {
  try {
    const claims = JSON.parse(
      Buffer.from(token.split(".")[1], "base64url").toString("utf8"),
    );
    return typeof claims.csrf_token === "string"
      ? claims.csrf_token
      : undefined;
  } catch {
    return undefined;
  }
}

function procStats(pid) {
  try {
    const status = readFileSync(`/proc/${pid}/status`, "utf8");
    const rss = /VmRSS:\s+(\d+) kB/.exec(status);
    const fds = readdirSync(`/proc/${pid}/fd`).length;
    return { rss_kb: rss ? Number(rss[1]) : undefined, fds };
  } catch {
    return { rss_kb: undefined, fds: undefined };
  }
}

async function main() {
  const args = parseArgs(process.argv);

  if (args.compare) {
    compare(args.compare[0], args.compare[1]);
    return;
  }

  if (!args.bin || !args.out) {
    fail("--bin and --out are required (or use --compare A.json B.json)");
  }

  const binarySha256 = createHash("sha256")
    .update(readFileSync(args.bin))
    .digest("hex");

  const dataDir = mkdtempSync(join(tmpdir(), "tb-contour-"));
  const golden = [];
  const step = (name, status, body) => {
    golden.push({ step: name, status, body: sortKeys(body) });
  };

  let child;
  try {
    // Seed: admin user via the CLI (this also initializes the depot).
    runCli(args.bin, [
      "--data-dir",
      dataDir,
      "user",
      "add",
      ADMIN_EMAIL,
      ADMIN_PASSWORD,
    ]);
    runCli(args.bin, ["--data-dir", dataDir, "admin", "promote", ADMIN_EMAIL]);

    const port = await freePort();
    const url = `http://127.0.0.1:${port}`;
    child = spawn(
      args.bin,
      ["--data-dir", dataDir, "run", `--address=127.0.0.1:${port}`],
      { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, ...args.serverEnv } },
    );
    let serverStderr = "";
    child.stderr.on("data", (chunk) => {
      serverStderr = (serverStderr + chunk.toString()).slice(-8192);
    });
    child.on("exit", (code) => {
      if (code !== null && code !== 0) {
        console.error(`server stderr tail:\n${serverStderr}`);
      }
    });

    await waitHealthy(url, child);

    // ---- Functional golden scenario ----------------------------------

    const health = await fetch(`${url}/api/healthcheck`);
    step("healthcheck", health.status, await health.text());

    const badLogin = await fetch(`${url}/api/auth/v1/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email: ADMIN_EMAIL, password: "wrong" }),
    });
    // Body may contain redirects/messages; the status code is the contract.
    step("login-rejected", badLogin.status, null);

    const login = await fetch(`${url}/api/auth/v1/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email: ADMIN_EMAIL, password: ADMIN_PASSWORD }),
    });
    const tokens = login.ok ? await login.json() : await login.text();
    // Tokens are random: golden only records the response shape.
    step(
      "login",
      login.status,
      login.ok ? Object.keys(tokens).sort() : tokens,
    );
    if (!login.ok) fail(`admin login failed: ${stable(tokens)}`);

    const authToken = tokens.auth_token;
    const csrf = csrfFromJwt(authToken);
    const adminHeaders = {
      Authorization: `Bearer ${authToken}`,
      "Content-Type": "application/json",
      ...(csrf ? { "CSRF-Token": csrf } : {}),
    };
    const adminQuery = async (query) => {
      const response = await fetch(`${url}/api/_admin/query`, {
        method: "POST",
        headers: adminHeaders,
        body: JSON.stringify({ query }),
      });
      const text = await response.text();
      let body;
      try {
        body = text.length > 0 ? JSON.parse(text) : null;
      } catch {
        body = text;
      }
      return { status: response.status, body };
    };

    {
      const r = await adminQuery(
        "CREATE TABLE contour_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, qty INTEGER NOT NULL DEFAULT 0) STRICT",
      );
      step("ddl-create-table", r.status, r.body);
    }

    for (let batch = 0; batch < 4; batch++) {
      const values = Array.from({ length: 25 }, (_, i) => {
        const id = batch * 25 + i + 1;
        return `(${id}, 'item-${String(id).padStart(4, "0")}', ${(id * 7) % 100})`;
      }).join(", ");
      const r = await adminQuery(
        `INSERT INTO contour_items (id, name, qty) VALUES ${values}`,
      );
      step(`insert-batch-${batch}`, r.status, r.body);
    }

    {
      const r = await adminQuery(
        "SELECT COUNT(*) AS n, SUM(qty) AS total FROM contour_items",
      );
      step("select-aggregate", r.status, r.body);
    }
    {
      const r = await adminQuery(
        "SELECT id, name, qty FROM contour_items WHERE qty > 50 ORDER BY id LIMIT 10",
      );
      step("select-filtered", r.status, r.body);
    }
    {
      const r = await adminQuery(
        "UPDATE contour_items SET qty = qty + 1 WHERE id % 10 = 0",
      );
      step("update-rows", r.status, r.body);
    }
    {
      const r = await adminQuery("DELETE FROM contour_items WHERE id > 90");
      step("delete-rows", r.status, r.body);
    }
    {
      const r = await adminQuery(
        "SELECT COUNT(*) AS n, SUM(qty) AS total FROM contour_items",
      );
      step("select-after-mutations", r.status, r.body);
    }

    {
      // Normalized to sorted table names: robust against metadata additions.
      const response = await fetch(`${url}/api/_admin/tables`, {
        headers: adminHeaders,
      });
      const body = await response.json();
      const names = Array.isArray(body?.tables)
        ? body.tables.map((t) => t?.name?.name ?? t?.name).sort()
        : body;
      step("admin-tables", response.status, names);
    }

    // ---- Metrics ------------------------------------------------------

    const before = procStats(child.pid);

    const timed = async (fn) => {
      const t0 = process.hrtime.bigint();
      await fn();
      return Number(process.hrtime.bigint() - t0) / 1_000;
    };

    const healthSamples = [];
    for (let i = 0; i < LATENCY_WARMUP + LATENCY_SAMPLES; i++) {
      const us = await timed(async () => {
        await (await fetch(`${url}/api/healthcheck`)).arrayBuffer();
      });
      if (i >= LATENCY_WARMUP) healthSamples.push(us);
    }

    const querySamples = [];
    for (let i = 0; i < QUERY_SAMPLES; i++) {
      const us = await timed(async () => {
        await adminQuery("SELECT id, qty FROM contour_items WHERE id = 42");
      });
      querySamples.push(us);
    }

    let throughputCount = 0;
    {
      const stopAt = Date.now() + THROUGHPUT_MS;
      const worker = async () => {
        while (Date.now() < stopAt) {
          await (await fetch(`${url}/api/healthcheck`)).arrayBuffer();
          throughputCount++;
        }
      };
      await Promise.all(
        Array.from({ length: THROUGHPUT_WORKERS }, () => worker()),
      );
    }

    const after = procStats(child.pid);

    const report = {
      label: args.label ?? args.bin,
      binary: args.bin,
      binary_sha256: binarySha256,
      server_env: args.serverEnv,
      node: process.version,
      created_at: new Date().toISOString(),
      golden,
      metrics: {
        healthcheck: stats(healthSamples),
        admin_query: stats(querySamples),
        throughput_rps: Math.round(throughputCount / (THROUGHPUT_MS / 1000)),
        rss_kb_idle: before.rss_kb,
        rss_kb_after_load: after.rss_kb,
        fds_idle: before.fds,
        fds_after_load: after.fds,
      },
    };

    writeFileSync(args.out, JSON.stringify(report, null, 2));
    writeFileSync(
      `${args.out}.golden.jsonl`,
      golden.map((g) => stable(g)).join("\n") + "\n",
    );
    console.log(
      `ab-contour: wrote ${args.out} (golden steps: ${golden.length}, ` +
        `health p50/p99: ${report.metrics.healthcheck.p50_us}/${report.metrics.healthcheck.p99_us}us, ` +
        `rps: ${report.metrics.throughput_rps}, rss: ${report.metrics.rss_kb_after_load}kB)`,
    );
  } finally {
    if (child && child.exitCode === null) {
      child.kill("SIGTERM");
      await new Promise((r) => setTimeout(r, 500));
      if (child.exitCode === null) child.kill("SIGKILL");
    }
    if (!args.keep) {
      rmSync(dataDir, { recursive: true, force: true });
    } else {
      console.log(`ab-contour: kept data dir ${dataDir}`);
    }
  }
}

function compare(pathA, pathB) {
  const a = JSON.parse(readFileSync(pathA, "utf8"));
  const b = JSON.parse(readFileSync(pathB, "utf8"));
  const failures = [];

  const goldenA = a.golden.map((g) => stable(g)).join("\n");
  const goldenB = b.golden.map((g) => stable(g)).join("\n");
  if (goldenA !== goldenB) {
    const linesA = goldenA.split("\n");
    const linesB = goldenB.split("\n");
    const n = Math.max(linesA.length, linesB.length);
    for (let i = 0; i < n; i++) {
      if (linesA[i] !== linesB[i]) {
        failures.push(
          `golden mismatch at step ${i}:\n  A: ${linesA[i]}\n  B: ${linesB[i]}`,
        );
        break;
      }
    }
  }

  const checkLatency = (name, metricA, metricB) => {
    for (const key of ["p50_us", "p99_us"]) {
      const va = metricA[key];
      const vb = metricB[key];
      // Sub-millisecond debug-build jitter: also allow a small absolute slack.
      const allowed = va * (1 + LATENCY_TOLERANCE) + 100;
      if (vb > allowed) {
        failures.push(
          `${name}.${key}: B=${Math.round(vb)}us exceeds A=${Math.round(va)}us +${LATENCY_TOLERANCE * 100}%+100us`,
        );
      }
    }
  };
  checkLatency("healthcheck", a.metrics.healthcheck, b.metrics.healthcheck);
  checkLatency("admin_query", a.metrics.admin_query, b.metrics.admin_query);

  const rssA = a.metrics.rss_kb_after_load;
  const rssB = b.metrics.rss_kb_after_load;
  if (rssA && rssB && rssB > rssA * (1 + RSS_TOLERANCE) + 4096) {
    failures.push(`rss_kb_after_load: B=${rssB} exceeds A=${rssA} +5%+4MB`);
  }

  console.log(`A: ${a.label} (${a.binary_sha256.slice(0, 12)})`);
  console.log(`B: ${b.label} (${b.binary_sha256.slice(0, 12)})`);
  console.log(
    `healthcheck p50/p99 us: A=${a.metrics.healthcheck.p50_us}/${a.metrics.healthcheck.p99_us} ` +
      `B=${b.metrics.healthcheck.p50_us}/${b.metrics.healthcheck.p99_us}`,
  );
  console.log(
    `admin_query p50/p99 us: A=${a.metrics.admin_query.p50_us}/${a.metrics.admin_query.p99_us} ` +
      `B=${b.metrics.admin_query.p50_us}/${b.metrics.admin_query.p99_us}`,
  );
  console.log(
    `throughput rps: A=${a.metrics.throughput_rps} B=${b.metrics.throughput_rps}`,
  );
  console.log(
    `rss after load kB: A=${a.metrics.rss_kb_after_load} B=${b.metrics.rss_kb_after_load}`,
  );

  if (failures.length > 0) {
    console.error(`\nFAIL (${failures.length}):`);
    for (const f of failures) console.error(`  - ${f}`);
    process.exit(2);
  }
  console.log("\nOK: golden identical, metrics within tolerances.");
}

await main();
