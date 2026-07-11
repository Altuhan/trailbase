# Built-in Remote Backups (S3 / Cloudflare R2)

TrailBase (this fork) continuously backs up the depot's SQLite databases —
including per-tenant databases under `data/*.db` — to any S3-compatible
object store, designed for Cloudflare R2. No external daemons, no inotify,
no libSQL: snapshots use SQLite's in-process online-backup API and uploads
stream through the already-bundled `object_store` crate.

## How it works

Two triggers feed one bounded background pipeline:

1. **Eviction sync.** The connection cache (LRU of open multi-DB
   connections, capacity `TB_CONN_CACHE_CAPACITY`, default 256) enqueues a
   database for backup the moment its connection is evicted — a wait-free
   hook that never blocks request handling. Databases that actually changed
   (file/WAL mtime vs. the local manifest, `backups/r2_state.json`) are
   snapshotted and uploaded to `latest/<name>.db`.
2. **Nightly sweep.** A scheduler job ("Remote Backup", visible under
   admin → Jobs) scans all `data/*.db` files, uploads dirty ones, then
   server-side copies everything uploaded within the last day to
   `epochs/<YYYY-MM-DD>/<name>.db` (CopyObject — no repeated egress) and
   prunes epochs older than the retention window.

Properties:

- **Zero idle cost.** Unconfigured: the subsystem doesn't exist. Configured
  but idle: one parked worker task, no file descriptors, no traffic.
- **Consistent snapshots.** The online-backup API copies page-level through
  a live WAL and tolerates concurrent writers; a snapshot is a valid,
  self-contained database file.
- **Crash-safe bookkeeping.** Upload state lives in a local JSON manifest
  written atomically; losing it merely re-uploads everything. Failed
  uploads retry (1s/5s/25s) and then stay dirty for the next cycle.
- **Bounded resources.** At most `concurrency` (default 2) simultaneous
  snapshot+upload pipelines; uploads stream in parts, never buffering a
  whole database in memory. Fits a 4 GB host with thousands of tenants.
- **Never uploaded:** `logs.db`, `session.db`, `queue.db`. `main.db` is
  included unless `include_main` is off.

## Configuration

Settings live in the admin UI (**Settings → Backups**) or the
`server.backups` section of `config.textproto`; they apply on **server
restart**. The secret access key participates in TrailBase's standard
secret redaction/vault machinery.

```textproto
server {
  backups {
    s3_endpoint: "https://<account-id>.r2.cloudflarestorage.com"
    s3_bucket_name: "trailbase-backups"
    s3_access_key_id: "..."
    s3_secret_access_key: "..."   # redacted into the vault
    # Optional, defaults shown:
    # s3_region: "auto"
    # schedule: "0 0 22 * * * *"  # 7-field cron with seconds, UTC
    # concurrency: 2
    # epoch_retain_days: 14
    # include_main: true
  }
}
```

Environment variables **take precedence** over the config file:

| Variable | Meaning |
| --- | --- |
| `TB_BACKUP_S3_ENDPOINT` | S3-compatible endpoint URL |
| `TB_BACKUP_S3_BUCKET` | Bucket name |
| `TB_BACKUP_S3_REGION` | Region, default `auto` (fits R2) |
| `TB_BACKUP_S3_ACCESS_KEY_ID` | Access key id |
| `TB_BACKUP_S3_SECRET_ACCESS_KEY` | Secret access key |
| `TB_BACKUP_SCHEDULE` | Sweep cron (7-field, UTC). Default `0 0 22 * * * *` = 03:00 Almaty |
| `TB_BACKUP_CONCURRENCY` | Parallel pipelines, default 2 |
| `TB_BACKUP_EPOCH_RETAIN_DAYS` | Epoch retention, default 14 |
| `TB_BACKUP_INCLUDE_MAIN` | `true`/`false`, default `true` |
| `TB_BACKUP_FS_DIR` | Local directory as the "bucket" — tests/e2e only, mutually exclusive with S3 |
| `TB_CONN_CACHE_CAPACITY` | Open-connection LRU capacity, default 256 |

(The generic `TRAIL_SERVER_BACKUPS_*` config-override variables work too.)

## Cloudflare R2 setup

1. Create a bucket, e.g. `trailbase-backups`.
2. Create an R2 API token scoped to that bucket with **Object Read & Write**.
3. Endpoint is `https://<account-id>.r2.cloudflarestorage.com`, region
   stays `auto`.
4. Fill the values under Settings → Backups (or env), restart, then check
   the logs for `Backups enabled` and trigger the "Remote Backup" job from
   admin → Jobs for an immediate first sweep.

## Restore runbook

Per-tenant restore (most common):

1. Stop the server (or ensure the tenant is idle/evicted).
2. Download the object: `latest/<name>.db` for the freshest copy, or
   `epochs/<date>/<name>.db` for a point-in-time one.
3. Replace `<data-dir>/data/<name>.db` with the downloaded file and remove
   any stale `<name>.db-wal` / `<name>.db-shm` next to it.
4. Start the server.

Full-depot restore: restore `main.db` the same way into a depot that also
carries your `config.textproto`, `migrations/` and `secrets/` (those are
part of your deployment, not of the database backups), then restore tenant
databases as above.

Drill it regularly: `node tools/backup-e2e.mjs` performs the whole
seed → sweep → re-upload → restore cycle against a real binary and a
filesystem bucket.

## Verifying performance

`tools/ab-contour.mjs` runs a deterministic functional golden log plus
latency/throughput/RSS probes and compares two builds:

```sh
node tools/ab-contour.mjs --bin ./trail-A --out A.json
node tools/ab-contour.mjs --bin ./trail-B --out B.json \
  --server-env TB_BACKUP_FS_DIR=/tmp/bucket \
  --server-env 'TB_BACKUP_SCHEDULE=* * * * * * *'   # continuous churn
node tools/ab-contour.mjs --compare A.json B.json
```

Recorded verdict for this feature (debug builds, paired runs, golden
byte-identical): backups off — healthcheck p50 661→689 µs, admin query p50
14.5→14.5 ms, RSS +0.6%; backups on with *every-second* sweeps — p50
661→677 µs, p99 1527→1227 µs, RSS lower than baseline. See
`tools/ab-reports/`.

## Notes and limitations

- Backup settings apply on restart; the running service is not rebuilt on
  config changes.
- The RPO is "last eviction or last nightly sweep". Tenants evicted from
  the connection cache sync shortly after they go idle; permanently hot
  tenants are covered by the nightly sweep.
- A database written *continuously* for over a minute can make a snapshot
  time out; it stays dirty and is retried on the next cycle.
- The filesystem backend may leave `name.db#N` staging artifacts when the
  same epoch object is overwritten repeatedly in one day (an
  `object_store` LocalFileSystem quirk); S3/R2 backends are unaffected.
- Tenant database names must be valid SQLite schema identifiers; files are
  keyed by their `<name>.db` stem.
