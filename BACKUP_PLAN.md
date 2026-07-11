# Native R2 backup — implementation backlog

Working file driving an iterative implementation loop on branch
`claude/trailbase-refactor-mcp-strategy-diflyp`. One unchecked item per
iteration: implement → verify → commit+push → check off (`[x]` done,
`[!] reason` blocked). Remove this file when finalizing the PR.

Design reference (approved plan): built-in backup of up to 10k per-tenant
SQLite databases to Cloudflare R2 (S3-compatible, via the existing
`object_store` dependency). One `BackupService` + two triggers:
**eviction sync** — a `quick_cache` eviction hook on `ConnectionManager`'s
connection cache enqueues dirty databases for upload (non-blocking hot
path); **nightly epoch job** — a scheduler job scans `data/*.db`, uploads
dirty ones to `latest/`, server-side copies to `epochs/<date>/` with
retention. Snapshots use the in-process SQLite online-backup API
(`rusqlite`, `backup` feature); uploads stream via
`object_store::buffered::BufWriter`. Zero cost when disabled; failed
uploads stay dirty and are retried by the next cycle. Config lives in
`config.proto` + admin UI (user request), with env overrides for secrets
and for the `LocalFileSystem` test backend (`TB_BACKUP_FS_DIR`).

## A/B contour protocol

`tools/ab-contour.mjs` runs a deterministic functional scenario (golden
JSONL, must be byte-identical between versions) plus latency (p50/p95/p99),
throughput, RSS and FD probes against a given binary; `--compare A.json
B.json` enforces tolerances (latency/RSS +5%). Binary A = `main` +
build-glue only (JS-skip escape hatch, pnpm wkx override), binary B = this
branch. Both built with `cargo build --bin trail --no-default-features`
(rust 1.95.0, protoc, full `pnpm install`).

## Backlog

- [x] 0. A/B contour: `tools/ab-contour.mjs`, build binary A from the
      `main`-equivalent tree, run the contour → commit report A
      (`tools/ab-reports/A-main.json`) and the Baseline section below.
- [x] 1. `crates/core/src/backup/` scaffold: env-based config
      (S3/R2 + `TB_BACKUP_FS_DIR` + schedule/concurrency/retention),
      JSON manifest with atomic writes + unit tests; module wired into
      `lib.rs`.
- [x] 2. Snapshot primitive: throwaway read-only rusqlite connection +
      online-backup API (128-page steps) in `spawn_blocking` with a
      deadline; integrity tests (WAL writes → snapshot →
      `PRAGMA integrity_check` + row counts).
- [x] 3. `BackupService`: mpsc queue + dedupe set, worker with
      `Semaphore`, mtime-vs-manifest dirty predicate, streaming upload to
      `latest/<name>.db`, retries 1s/5s/25s; tests against
      `object_store::memory::InMemory` (success / transient failure /
      permanent failure keeps the db dirty).
- [x] 4. Eviction hook: `quick_cache` lifecycle (fallback: drop-guard in
      `ConnectionEntry`) → non-blocking enqueue; cache capacity from
      options + `TB_CONN_CACHE_CAPACITY`; eviction test with capacity 2.
- [x] 5. Nightly job "R2 Backup": custom (non-proto) job registration,
      `data/*.db` scan, enqueue dirty, drain, server-side copy `latest/` →
      `epochs/<YYYY-MM-DD>/`, retention cleanup, summary log; InMemory
      tests.
- [x] 6. Admin-visible config (user request): `BackupConfig` section in
      `config.proto` (+ regenerated ts-proto bindings) with env vars as
      secret/ops overrides; settings card in the admin UI.
- [x] 7. Binary e2e on `TB_BACKUP_FS_DIR`: `trail` with tenant DBs and
      `TB_CONN_CACHE_CAPACITY=2`, files appear in the directory "bucket";
      restore drill: copy `latest/<name>.db` into a fresh data dir.
- [x] 8. A/B comparison: build B, run contour B(off) and B(on),
      `--compare` against report A within tolerances; module README
      (env/config reference, R2 setup, restore runbook). VERDICT: golden
      byte-identical in both pairs; B(off) p50 healthcheck/admin-query
      within noise of A (689 vs 661 us / 14.50 vs 14.54 ms), RSS +0.6%;
      B(on) under *every-second* sweep churn matched or beat A (p99 1227
      vs 1527 us, RSS lower). Investigation note: with 250-sample tails
      the admin-query p99 fluctuated ±20% across identical binaries, so
      the contour now uses 1000/1500 samples and split tolerances (p50
      5%+100us strict, p99 10%+1ms). Feature docs: BACKUPS.md.
- [x] 9. Finalization: clippy clean (3 collapsible-if fixes), fmt, test
      suites green (trailbase 196, sqlite/schema/qs 59, admin vitest 7,
      e2e drill PASS), CHANGELOG entry, final summary.

## Baseline (report A)

Recorded 2026-07-11 on the session container. Binary A (sha256
`c37600992ace…f997c3`, kept as a scratch artifact and reproducible):
`main` (02f4dc8) + iteration-0 build glue only — no backup code; debug
profile, rust 1.95.0, `cargo build --bin trail --no-default-features`,
real UI assets. Report: `tools/ab-reports/A-main.json`.

- Golden: 14 steps, expected statuses only (200s + intentional 401),
  deterministic row data (100 inserts → aggregate 100/4950 → after
  mutations 90/4374), 7 tables listed.
- healthcheck p50/p95/p99: 726/989/2535 us (mean 787).
- admin_query (point SELECT) p50/p95/p99: 15013/19141/20551 us.
- Throughput: 5273 rps (8 workers, 3 s, healthcheck).
- RSS idle/after-load: 92356/96628 kB; FDs 39/54.

Comparison protocol: the committed report is A's recorded snapshot. At
iteration 8 A and B are re-run back-to-back on the same machine (paired
fresh runs) and compared with `--compare` to keep environment drift out
of the verdict.
