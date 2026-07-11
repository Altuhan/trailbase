use log::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use object_store::ObjectStore;
use object_store::path::Path as ObjPath;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::backup::{
  BackupConfig, BackupConfigError, Manifest, ManifestEntry, SnapshotError, snapshot_db_file,
};
use crate::data_dir::DataDir;

/// Databases that must never leave the machine: logs (PII), sessions and
/// queue (secrets/ephemeral). `main` is subject to `include_main`.
pub(crate) const EXCLUDED_DB_NAMES: [&str; 3] = ["logs", "session", "queue"];

pub(crate) const LATEST_PREFIX: &str = "latest";
pub(crate) const EPOCHS_PREFIX: &str = "epochs";

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(60);
const MANIFEST_FILE_NAME: &str = "r2_state.json";

#[derive(Debug, Error)]
pub enum BackupError {
  #[error("Snapshot: {0}")]
  Snapshot(#[from] SnapshotError),
  #[error("Store: {0}")]
  Store(#[from] object_store::Error),
  #[error("IO: {0}")]
  Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
  /// Pause before each re-attempt; total attempts = backoff.len() + 1.
  pub backoff: Vec<Duration>,
}

impl Default for RetryPolicy {
  fn default() -> Self {
    return RetryPolicy {
      backoff: vec![
        Duration::from_secs(1),
        Duration::from_secs(5),
        Duration::from_secs(25),
      ],
    };
  }
}

enum Msg {
  Db(String),
  /// Acknowledged once every message queued before it has been fully
  /// processed (including in-flight uploads).
  Barrier(oneshot::Sender<()>),
}

struct Inner {
  config: BackupConfig,
  store: Arc<dyn ObjectStore>,
  /// Directory holding `<name>.db` files, i.e. `DataDir::data_path()`.
  data_path: PathBuf,
  tmp_path: PathBuf,
  manifest_path: PathBuf,
  manifest: Mutex<Manifest>,
  pending: Mutex<HashSet<String>>,
  queue: mpsc::UnboundedSender<Msg>,
}

/// Cheaply clonable handle to the background backup pipeline.
///
/// `enqueue` is wait-free and safe to call from latency-sensitive paths
/// (e.g. the connection-cache eviction hook); all IO happens on a worker
/// task bounded by the configured concurrency. Failed uploads keep the
/// database dirty and are picked up by the next enqueue or nightly sweep.
#[derive(Clone)]
pub(crate) struct BackupService {
  inner: Arc<Inner>,
}

impl BackupService {
  pub(crate) fn from_config(
    config: BackupConfig,
    data_dir: &DataDir,
  ) -> Result<Self, BackupConfigError> {
    let store = config.build_store()?;
    return Ok(Self::with_store(
      config,
      store,
      data_dir,
      RetryPolicy::default(),
    ));
  }

  /// Test-friendly constructor with an injected store and retry policy.
  /// Must be called within a tokio runtime.
  pub(crate) fn with_store(
    config: BackupConfig,
    store: Arc<dyn ObjectStore>,
    data_dir: &DataDir,
    retry: RetryPolicy,
  ) -> Self {
    let backup_path = data_dir.backup_path();
    let tmp_path = backup_path.join("tmp");
    if let Err(err) = std::fs::create_dir_all(&tmp_path) {
      warn!("Failed to create backup tmp dir {tmp_path:?}: {err}");
    }

    let manifest_path = backup_path.join(MANIFEST_FILE_NAME);
    let manifest = Manifest::load(&manifest_path);

    let (queue, rx) = mpsc::unbounded_channel::<Msg>();
    let inner = Arc::new(Inner {
      config,
      store,
      data_path: data_dir.data_path(),
      tmp_path,
      manifest_path,
      manifest: Mutex::new(manifest),
      pending: Mutex::new(HashSet::new()),
      queue,
    });

    tokio::spawn(worker_loop(inner.clone(), rx, retry));

    return BackupService { inner };
  }

  pub(crate) fn config(&self) -> &BackupConfig {
    return &self.inner.config;
  }

  /// Marks a database as a backup candidate. Wait-free: a queue send plus
  /// a hash-set insert; duplicates of a not-yet-processed name are dropped.
  pub(crate) fn enqueue(&self, name: &str) {
    if EXCLUDED_DB_NAMES.contains(&name) {
      return;
    }
    if name == "main" && !self.inner.config.include_main {
      return;
    }

    let mut pending = self.inner.pending.lock();
    if pending.insert(name.to_string()) {
      if self.inner.queue.send(Msg::Db(name.to_string())).is_err() {
        // Worker gone, i.e. runtime shutting down.
        pending.remove(name);
      }
    }
  }

  /// Resolves once everything enqueued before the call has been processed.
  pub(crate) async fn drain(&self) {
    let (ack, done) = oneshot::channel();
    if self.inner.queue.send(Msg::Barrier(ack)).is_ok() {
      let _ = done.await;
    }
  }

  #[cfg(test)]
  pub(crate) fn manifest_snapshot(&self) -> Manifest {
    return self.inner.manifest.lock().clone();
  }
}

async fn worker_loop(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Msg>, retry: RetryPolicy) {
  let semaphore = Arc::new(Semaphore::new(inner.config.concurrency));
  let mut tasks = tokio::task::JoinSet::new();

  while let Some(msg) = rx.recv().await {
    match msg {
      Msg::Db(name) => {
        // A name is "pending" only while queued; once picked up, concurrent
        // writes may legitimately re-enqueue it.
        inner.pending.lock().remove(&name);

        // Opportunistically reap finished uploads.
        while tasks.try_join_next().is_some() {}

        let Ok(permit) = semaphore.clone().acquire_owned().await else {
          return;
        };
        let inner = inner.clone();
        let retry = retry.clone();
        tasks.spawn(async move {
          let _permit = permit;
          backup_one_with_retries(&inner, &name, &retry).await;
        });
      }
      Msg::Barrier(ack) => {
        while tasks.join_next().await.is_some() {}
        let _ = ack.send(());
      }
    }
  }
}

async fn backup_one_with_retries(inner: &Inner, name: &str, retry: &RetryPolicy) {
  let mut attempt = 0;
  loop {
    match backup_one(inner, name).await {
      Ok(Outcome::Uploaded(size)) => {
        debug!("Backed up '{name}' ({size} bytes).");
        return;
      }
      Ok(Outcome::Clean | Outcome::Missing) => {
        return;
      }
      Err(err) => {
        if attempt >= retry.backoff.len() {
          warn!(
            "Backup of '{name}' failed after {} attempts: {err}. The database stays dirty and will be retried by the next cycle.",
            attempt + 1
          );
          return;
        }
        tokio::time::sleep(retry.backoff[attempt]).await;
        attempt += 1;
      }
    }
  }
}

enum Outcome {
  Uploaded(u64),
  Clean,
  Missing,
}

async fn backup_one(inner: &Inner, name: &str) -> Result<Outcome, BackupError> {
  let db_path = inner.data_path.join(format!("{name}.db"));
  if !db_path.exists() {
    debug!("Skipping backup of missing database '{name}'.");
    return Ok(Outcome::Missing);
  }

  {
    let manifest = inner.manifest.lock();
    if !is_dirty(&db_path, manifest.entries.get(name)) {
      return Ok(Outcome::Clean);
    }
  }

  // Timestamp from *before* the snapshot: writes racing the copy leave the
  // database dirty for the next cycle.
  let start_ms = now_ms();

  let snap_path = inner.tmp_path.join(format!("{name}.db.snap"));
  let size = snapshot_db_file(db_path, snap_path.clone(), SNAPSHOT_TIMEOUT).await?;

  let upload = upload_file(&inner.store, &snap_path, &latest_object_path(name)).await;
  let _ = tokio::fs::remove_file(&snap_path).await;
  upload?;

  {
    let mut manifest = inner.manifest.lock();
    manifest.entries.insert(
      name.to_string(),
      ManifestEntry {
        last_backup_start_ms: start_ms,
        size_bytes: size,
      },
    );
    manifest.store(&inner.manifest_path)?;
  }

  return Ok(Outcome::Uploaded(size));
}

/// Streams a local file into the object store without buffering it whole;
/// `BufWriter` transparently switches to multipart uploads for large files.
async fn upload_file(
  store: &Arc<dyn ObjectStore>,
  src: &Path,
  dst: &ObjPath,
) -> Result<(), BackupError> {
  let mut file = tokio::fs::File::open(src).await?;
  let mut writer = object_store::buffered::BufWriter::new(store.clone(), dst.clone());

  return match tokio::io::copy(&mut file, &mut writer).await {
    Ok(_) => {
      // NOTE: No abort on shutdown failures — the writer is already in its
      // terminal state then (and would panic on abort).
      writer.shutdown().await?;
      Ok(())
    }
    Err(err) => {
      // Cancel a potentially in-flight multipart upload.
      let _ = writer.abort().await;
      Err(err.into())
    }
  };
}

pub(crate) fn latest_object_path(name: &str) -> ObjPath {
  return ObjPath::from(format!("{LATEST_PREFIX}/{name}.db"));
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct NightlySummary {
  pub scanned: usize,
  pub uploaded: usize,
  pub still_dirty: usize,
  pub epochs_copied: usize,
  pub epochs_pruned: usize,
}

impl std::fmt::Display for NightlySummary {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    return write!(
      f,
      "scanned={} uploaded={} still-dirty={} epochs-copied={} epochs-pruned={}",
      self.scanned, self.uploaded, self.still_dirty, self.epochs_copied, self.epochs_pruned
    );
  }
}

impl BackupService {
  /// The nightly sweep: enqueues every database file for upload (the
  /// pipeline skips clean ones), waits for the queue to drain, then
  /// maintains the dated `epochs/` copies — server-side copies of
  /// `latest/` for every database uploaded within the last day — and
  /// prunes epochs older than the retention window.
  pub(crate) async fn run_nightly(&self) -> Result<NightlySummary, BackupError> {
    use futures_util::StreamExt;
    use object_store::ObjectStoreExt;

    let inner = &self.inner;
    let sweep_start_ms = now_ms();

    let names = list_database_names(&inner.data_path, inner.config.include_main)?;
    let mut summary = NightlySummary {
      scanned: names.len(),
      ..Default::default()
    };

    for name in &names {
      self.enqueue(name);
    }
    self.drain().await;

    // Bookkeeping and the epoch-copy candidates (uploaded within a day —
    // this sweep or earlier eviction syncs).
    const DAY_MS: i64 = 24 * 3600 * 1000;
    let mut epoch_candidates = Vec::new();
    {
      let manifest = inner.manifest.lock();
      for name in &names {
        let entry = manifest.entries.get(name);
        if let Some(entry) = entry {
          if entry.last_backup_start_ms >= sweep_start_ms {
            summary.uploaded += 1;
          }
          if entry.last_backup_start_ms >= sweep_start_ms - DAY_MS {
            epoch_candidates.push(name.clone());
          }
        }
        if is_dirty(&inner.data_path.join(format!("{name}.db")), entry) {
          summary.still_dirty += 1;
        }
      }
    }

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    for name in epoch_candidates {
      let from = latest_object_path(&name);
      let to = ObjPath::from(format!("{EPOCHS_PREFIX}/{today}/{name}.db"));
      match inner.store.copy(&from, &to).await {
        Ok(()) => summary.epochs_copied += 1,
        Err(err) => warn!("Failed to copy {from} to {to}: {err}"),
      }
    }

    // Retention: drop whole epochs/<date>/ prefixes past the window.
    let cutoff = chrono::Utc::now().date_naive()
      - chrono::Days::new(u64::from(inner.config.epoch_retain_days));
    let listing = inner
      .store
      .list_with_delimiter(Some(&ObjPath::from(EPOCHS_PREFIX)))
      .await?;
    for prefix in listing.common_prefixes {
      let Some(date_part) = prefix.as_ref().strip_prefix("epochs/") else {
        continue;
      };
      let Ok(date) = date_part.parse::<chrono::NaiveDate>() else {
        warn!("Skipping unrecognized epoch prefix: {prefix}");
        continue;
      };
      if date >= cutoff {
        continue;
      }

      let mut objects = inner.store.list(Some(&prefix));
      while let Some(meta) = objects.next().await {
        let location = meta?.location;
        match inner.store.delete(&location).await {
          Ok(()) => summary.epochs_pruned += 1,
          Err(err) => warn!("Failed to prune old epoch object {location}: {err}"),
        }
      }
    }

    return Ok(summary);
  }
}

/// Lists backup-eligible database names in `data_path`: regular `*.db`
/// files minus the never-uploaded system databases (and `main` unless
/// included).
fn list_database_names(data_path: &Path, include_main: bool) -> std::io::Result<Vec<String>> {
  let mut names = Vec::new();
  for entry in std::fs::read_dir(data_path)? {
    let entry = entry?;
    let path = entry.path();
    if !entry.file_type()?.is_file() || path.extension().and_then(|e| e.to_str()) != Some("db") {
      continue;
    }
    let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
      warn!("Skipping non-UTF-8 database file: {path:?}");
      continue;
    };
    if EXCLUDED_DB_NAMES.contains(&name) || (name == "main" && !include_main) {
      continue;
    }
    names.push(name.to_string());
  }
  names.sort();
  return Ok(names);
}

pub(crate) fn now_ms() -> i64 {
  return SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as i64)
    .unwrap_or(0);
}

fn mtime_ms(path: &Path) -> Option<i64> {
  let modified = std::fs::metadata(path).ok()?.modified().ok()?;
  return modified
    .duration_since(UNIX_EPOCH)
    .ok()
    .map(|d| d.as_millis() as i64);
}

/// Mtime of a *non-empty* WAL next to `db_path`. An empty WAL carries no
/// pages and is ignored — notably, merely opening a WAL-mode database (as
/// the snapshot itself does read-only) re-creates an empty `-wal` file,
/// which must not mark the database dirty again.
fn wal_mtime_ms(db_path: &Path) -> Option<i64> {
  let mut wal = db_path.as_os_str().to_owned();
  wal.push("-wal");

  let meta = std::fs::metadata(Path::new(&wal)).ok()?;
  if meta.len() == 0 {
    return None;
  }
  return meta
    .modified()
    .ok()?
    .duration_since(UNIX_EPOCH)
    .ok()
    .map(|d| d.as_millis() as i64);
}

/// A database is dirty when it (or its non-empty WAL) changed at or after
/// the start of the last successful backup — or was never backed up.
pub(crate) fn is_dirty(db_path: &Path, entry: Option<&ManifestEntry>) -> bool {
  let Some(entry) = entry else {
    return true;
  };

  let newest = [mtime_ms(db_path), wal_mtime_ms(db_path)]
    .into_iter()
    .flatten()
    .max();

  return match newest {
    Some(mtime) => mtime >= entry.last_backup_start_ms,
    // Metadata unreadable: err on the dirty side.
    None => true,
  };
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backup::BackupStoreConfig;
  use object_store::ObjectStoreExt;
  use std::sync::atomic::{AtomicUsize, Ordering};

  fn test_config() -> BackupConfig {
    return BackupConfig {
      store: BackupStoreConfig::Fs {
        dir: PathBuf::from("/unused"),
      },
      schedule: crate::backup::DEFAULT_SCHEDULE.to_string(),
      concurrency: 2,
      epoch_retain_days: 14,
      include_main: true,
    };
  }

  fn zero_retry(attempts: usize) -> RetryPolicy {
    return RetryPolicy {
      backoff: vec![Duration::ZERO; attempts.saturating_sub(1)],
    };
  }

  fn create_tenant_db(data_path: &Path, name: &str, rows: usize) {
    std::fs::create_dir_all(data_path).expect("mkdir");
    let conn = rusqlite::Connection::open(data_path.join(format!("{name}.db"))).expect("open");
    conn
      .pragma_update(None, "journal_mode", "WAL")
      .expect("wal");
    conn
      .execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
      .expect("ddl");
    for i in 0..rows {
      conn
        .execute(
          "INSERT INTO items (id, value) VALUES ($1, $2)",
          rusqlite::params![i as i64 + 1, format!("value-{i}")],
        )
        .expect("insert");
    }
    // Dirtiness compares millisecond file mtimes against the backup start;
    // put the creation and the first backup into distinct milliseconds.
    std::thread::sleep(Duration::from_millis(10));
  }

  async fn fetch_object(store: &Arc<dyn ObjectStore>, path: &ObjPath) -> Option<Vec<u8>> {
    let result = store.get(path).await.ok()?;
    return Some(result.bytes().await.expect("bytes").to_vec());
  }

  /// Delegates to an in-memory store, failing the first `fail_puts` write
  /// attempts and counting all of them.
  #[derive(Debug)]
  struct FlakyStore {
    inner: object_store::memory::InMemory,
    fail_puts: AtomicUsize,
    put_attempts: AtomicUsize,
  }

  impl FlakyStore {
    fn new(fail_puts: usize) -> Self {
      return FlakyStore {
        inner: object_store::memory::InMemory::new(),
        fail_puts: AtomicUsize::new(fail_puts),
        put_attempts: AtomicUsize::new(0),
      };
    }

    fn injected_failure(&self) -> Option<object_store::Error> {
      self.put_attempts.fetch_add(1, Ordering::Relaxed);
      let injected = self
        .fail_puts
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
        .is_ok();
      if injected {
        return Some(object_store::Error::Generic {
          store: "flaky",
          source: "injected failure".into(),
        });
      }
      return None;
    }
  }

  impl std::fmt::Display for FlakyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      return write!(f, "FlakyStore({})", self.inner);
    }
  }

  #[async_trait::async_trait]
  impl ObjectStore for FlakyStore {
    async fn put_opts(
      &self,
      location: &ObjPath,
      payload: object_store::PutPayload,
      opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
      if let Some(err) = self.injected_failure() {
        return Err(err);
      }
      return self.inner.put_opts(location, payload, opts).await;
    }

    async fn put_multipart_opts(
      &self,
      location: &ObjPath,
      opts: object_store::PutMultipartOpts,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
      if let Some(err) = self.injected_failure() {
        return Err(err);
      }
      return self.inner.put_multipart_opts(location, opts).await;
    }

    async fn get_opts(
      &self,
      location: &ObjPath,
      options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
      return self.inner.get_opts(location, options).await;
    }

    fn delete_stream(
      &self,
      locations: futures_util::stream::BoxStream<'static, object_store::Result<ObjPath>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjPath>> {
      return self.inner.delete_stream(locations);
    }

    fn list(
      &self,
      prefix: Option<&ObjPath>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
      return self.inner.list(prefix);
    }

    async fn list_with_delimiter(
      &self,
      prefix: Option<&ObjPath>,
    ) -> object_store::Result<object_store::ListResult> {
      return self.inner.list_with_delimiter(prefix).await;
    }

    async fn copy_opts(
      &self,
      from: &ObjPath,
      to: &ObjPath,
      options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
      return self.inner.copy_opts(from, to, options).await;
    }
  }

  #[tokio::test]
  async fn test_uploads_dirty_database_and_skips_clean_one() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    create_tenant_db(&data_dir.data_path(), "tenant-a", 100);

    let flaky = Arc::new(FlakyStore::new(0));
    let store: Arc<dyn ObjectStore> = flaky.clone();
    let service = BackupService::with_store(test_config(), store.clone(), &data_dir, zero_retry(1));

    service.enqueue("tenant-a");
    service.drain().await;

    // Uploaded and readable as a valid database.
    let bytes = fetch_object(&store, &latest_object_path("tenant-a"))
      .await
      .expect("object");
    let restored = tmp.path().join("restored.db");
    std::fs::write(&restored, &bytes).expect("write");
    let conn = rusqlite::Connection::open(&restored).expect("open");
    let count: i64 = conn
      .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
      .expect("count");
    assert_eq!(100, count);

    // Manifest persisted with the uploaded size.
    let manifest = service.manifest_snapshot();
    let entry = manifest.entries.get("tenant-a").expect("entry");
    assert_eq!(bytes.len() as u64, entry.size_bytes);
    assert!(
      Manifest::load(&data_dir.backup_path().join(MANIFEST_FILE_NAME))
        .entries
        .contains_key("tenant-a")
    );

    // No leftover snapshot files.
    let leftovers: Vec<_> = std::fs::read_dir(data_dir.backup_path().join("tmp"))
      .expect("tmp dir")
      .collect();
    assert!(leftovers.is_empty());

    // A second cycle without writes is a no-op — including the empty WAL
    // that the snapshot's own read-only connection leaves behind.
    let attempts_before = flaky.put_attempts.load(Ordering::Relaxed);
    service.enqueue("tenant-a");
    service.drain().await;
    assert_eq!(attempts_before, flaky.put_attempts.load(Ordering::Relaxed));
  }

  #[tokio::test]
  async fn test_retries_transient_store_failures() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    create_tenant_db(&data_dir.data_path(), "tenant-b", 10);

    let flaky = Arc::new(FlakyStore::new(2));
    let store: Arc<dyn ObjectStore> = flaky.clone();
    let service = BackupService::with_store(test_config(), store.clone(), &data_dir, zero_retry(3));

    service.enqueue("tenant-b");
    service.drain().await;

    assert_eq!(3, flaky.put_attempts.load(Ordering::Relaxed));
    assert!(
      fetch_object(&store, &latest_object_path("tenant-b"))
        .await
        .is_some()
    );
  }

  #[tokio::test]
  async fn test_permanent_failure_keeps_database_dirty() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    create_tenant_db(&data_dir.data_path(), "tenant-c", 10);

    let flaky = Arc::new(FlakyStore::new(usize::MAX));
    let store: Arc<dyn ObjectStore> = flaky.clone();
    let service = BackupService::with_store(test_config(), store.clone(), &data_dir, zero_retry(2));

    service.enqueue("tenant-c");
    service.drain().await;

    assert_eq!(2, flaky.put_attempts.load(Ordering::Relaxed));
    assert!(
      fetch_object(&store, &latest_object_path("tenant-c"))
        .await
        .is_none()
    );
    // Nothing recorded: the database remains dirty for the next cycle.
    assert!(service.manifest_snapshot().entries.is_empty());
    let db_path = data_dir.data_path().join("tenant-c.db");
    assert!(is_dirty(&db_path, None));
  }

  #[tokio::test]
  async fn test_excluded_and_missing_databases_are_skipped() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    std::fs::create_dir_all(data_dir.data_path()).expect("mkdir");

    let flaky = Arc::new(FlakyStore::new(0));
    let store: Arc<dyn ObjectStore> = flaky.clone();

    let mut config = test_config();
    config.include_main = false;
    let service = BackupService::with_store(config, store, &data_dir, zero_retry(1));

    for name in ["logs", "session", "queue", "main", "ghost"] {
      service.enqueue(name);
    }
    service.drain().await;

    assert_eq!(0, flaky.put_attempts.load(Ordering::Relaxed));
    assert!(service.manifest_snapshot().entries.is_empty());
  }

  #[tokio::test]
  async fn test_nightly_sweep_uploads_and_copies_epochs() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    for (name, rows) in [("tenant_a", 20), ("tenant_b", 10), ("main", 5), ("logs", 5)] {
      create_tenant_db(&data_dir.data_path(), name, rows);
    }

    let flaky = Arc::new(FlakyStore::new(0));
    let store: Arc<dyn ObjectStore> = flaky.clone();
    let service = BackupService::with_store(test_config(), store.clone(), &data_dir, zero_retry(1));

    let summary = service.run_nightly().await.expect("sweep");
    assert_eq!(3, summary.scanned, "logs is excluded from the sweep");
    assert_eq!(3, summary.uploaded);
    assert_eq!(0, summary.still_dirty);
    assert_eq!(3, summary.epochs_copied);
    assert_eq!(0, summary.epochs_pruned);

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut locations = list_locations(&store).await;
    locations.sort();
    assert_eq!(
      vec![
        format!("epochs/{today}/main.db"),
        format!("epochs/{today}/tenant_a.db"),
        format!("epochs/{today}/tenant_b.db"),
        "latest/main.db".to_string(),
        "latest/tenant_a.db".to_string(),
        "latest/tenant_b.db".to_string(),
      ],
      locations
    );

    // A second sweep without writes uploads nothing and keeps the same
    // set of objects (epoch copies overwrite today's entries).
    let attempts_before = flaky.put_attempts.load(Ordering::Relaxed);
    let summary = service.run_nightly().await.expect("sweep");
    assert_eq!(0, summary.uploaded);
    assert_eq!(3, summary.epochs_copied);
    assert_eq!(attempts_before, flaky.put_attempts.load(Ordering::Relaxed));
    assert_eq!(6, list_locations(&store).await.len());
  }

  #[tokio::test]
  async fn test_nightly_sweep_prunes_expired_epochs() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let data_dir = DataDir(tmp.path().to_path_buf());
    create_tenant_db(&data_dir.data_path(), "tenant_a", 5);

    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    for path in ["epochs/2020-01-01/ghost.db", "epochs/2020-01-01/other.db"] {
      store
        .put(
          &ObjPath::from(path),
          object_store::PutPayload::from_static(b"old"),
        )
        .await
        .expect("seed");
    }

    let service = BackupService::with_store(test_config(), store.clone(), &data_dir, zero_retry(1));
    let summary = service.run_nightly().await.expect("sweep");

    assert_eq!(2, summary.epochs_pruned);
    let locations = list_locations(&store).await;
    assert!(
      locations.iter().all(|l| !l.starts_with("epochs/2020")),
      "expired epochs must be gone: {locations:?}"
    );
  }

  #[test]
  fn test_list_database_names() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let dir = tmp.path();
    for file in [
      "tenant_a.db",
      "tenant_b.db",
      "main.db",
      "logs.db",
      "session.db",
      "queue.db",
      "notes.txt",
      "tenant_c.db-wal",
    ] {
      std::fs::write(dir.join(file), b"x").expect("write");
    }
    std::fs::create_dir(dir.join("sub.db")).expect("mkdir");

    assert_eq!(
      vec!["main".to_string(), "tenant_a".into(), "tenant_b".into()],
      list_database_names(dir, true).expect("names")
    );
    assert_eq!(
      vec!["tenant_a".to_string(), "tenant_b".into()],
      list_database_names(dir, false).expect("names")
    );
  }

  async fn list_locations(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    use futures_util::StreamExt;

    let mut locations = vec![];
    let mut stream = store.list(None);
    while let Some(meta) = stream.next().await {
      locations.push(meta.expect("meta").location.to_string());
    }
    return locations;
  }

  #[test]
  fn test_is_dirty_predicate() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let db = tmp.path().join("t.db");
    std::fs::write(&db, b"x").expect("write");

    // Never backed up.
    assert!(is_dirty(&db, None));

    let old_backup = ManifestEntry {
      last_backup_start_ms: now_ms() - 60_000,
      size_bytes: 1,
    };
    assert!(is_dirty(&db, Some(&old_backup)));

    let fresh_backup = ManifestEntry {
      last_backup_start_ms: now_ms() + 60_000,
      size_bytes: 1,
    };
    assert!(!is_dirty(&db, Some(&fresh_backup)));

    // A fresh WAL next to a stale main file marks the database dirty.
    let entry = ManifestEntry {
      last_backup_start_ms: mtime_ms(&db).expect("mtime") + 1,
      size_bytes: 1,
    };
    std::thread::sleep(Duration::from_millis(10));
    let wal = tmp.path().join("t.db-wal");
    std::fs::write(&wal, b"w").expect("write");
    assert!(is_dirty(&db, Some(&entry)));
  }
}
