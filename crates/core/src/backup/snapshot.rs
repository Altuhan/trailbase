use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SnapshotError {
  #[error("Sqlite: {0}")]
  Sqlite(#[from] rusqlite::Error),
  #[error("IO: {0}")]
  Io(#[from] std::io::Error),
  #[error("Timed out after {0:?}; the database may be written to continuously")]
  Timeout(Duration),
  #[error("Unexpected backup step result: {0}")]
  Unexpected(String),
  #[error("Join: {0}")]
  Join(#[from] tokio::task::JoinError),
}

/// Copies a transactionally consistent point-in-time snapshot of the SQLite
/// database at `source` into a fresh file at `dest`.
///
/// Uses the online-backup API: page-level copying that never evaluates
/// schema SQL (TrailBase's extension functions are irrelevant to it), reads
/// through a live WAL and tolerates concurrent writers — a write into
/// `source` mid-copy restarts the affected pass. Runs on the blocking pool.
pub(crate) async fn snapshot_db_file(
  source: PathBuf,
  dest: PathBuf,
  timeout: Duration,
) -> Result<u64, SnapshotError> {
  return tokio::task::spawn_blocking(move || {
    return snapshot_db_file_blocking(&source, &dest, timeout);
  })
  .await?;
}

pub(crate) fn snapshot_db_file_blocking(
  source: &Path,
  dest: &Path,
  timeout: Duration,
) -> Result<u64, SnapshotError> {
  let result = snapshot_impl(source, dest, timeout);
  if result.is_err() {
    // Never leave a partial snapshot behind.
    let _ = std::fs::remove_file(dest);
  }
  return result;
}

fn snapshot_impl(source: &Path, dest: &Path, timeout: Duration) -> Result<u64, SnapshotError> {
  use rusqlite::OpenFlags;
  use rusqlite::backup::{Backup, StepResult};

  let deadline = Instant::now() + timeout;

  if let Some(parent) = dest.parent() {
    std::fs::create_dir_all(parent)?;
  }
  // A stale file from an aborted earlier run must not contribute leftovers.
  let _ = std::fs::remove_file(dest);

  let src = rusqlite::Connection::open_with_flags(
    source,
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
  )?;
  let mut dst = rusqlite::Connection::open(dest)?;

  {
    let backup = Backup::new(&src, &mut dst)?;
    loop {
      if Instant::now() >= deadline {
        return Err(SnapshotError::Timeout(timeout));
      }

      match backup.step(/* num_pages= */ 128)? {
        StepResult::Done => break,
        StepResult::More => {
          // Just continue; each step re-acquires the source lock.
        }
        StepResult::Busy | StepResult::Locked => {
          std::thread::sleep(Duration::from_millis(5));
        }
        r => {
          return Err(SnapshotError::Unexpected(format!("{r:?}")));
        }
      }
    }
  }

  drop(src);
  dst.close().map_err(|(_conn, err)| err)?;

  return Ok(std::fs::metadata(dest)?.len());
}

#[cfg(test)]
mod tests {
  use super::*;

  fn count_items(db: &Path) -> i64 {
    let conn = rusqlite::Connection::open(db).expect("open");
    return conn
      .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
      .expect("count");
  }

  fn integrity_check(db: &Path) -> String {
    let conn = rusqlite::Connection::open(db).expect("open");
    return conn
      .query_row("PRAGMA integrity_check", [], |row| row.get(0))
      .expect("integrity_check");
  }

  fn setup_wal_db(path: &Path, rows: usize) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).expect("open");
    conn
      .pragma_update(None, "journal_mode", "WAL")
      .expect("wal");
    conn
      .execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
      .expect("ddl");
    for chunk in 0..(rows / 100) {
      let mut batch = String::from("BEGIN;");
      for i in 0..100 {
        let id = chunk * 100 + i + 1;
        batch.push_str(&format!(
          "INSERT INTO items (id, value) VALUES ({id}, 'value-{id}');"
        ));
      }
      batch.push_str("COMMIT;");
      conn.execute_batch(&batch).expect("insert");
    }
    return conn;
  }

  #[test]
  fn test_snapshot_reads_through_live_wal() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let source = tmp.path().join("tenant.db");
    let dest = tmp.path().join("tenant.db.snap");

    // Keep the writer open: the WAL is neither checkpointed nor deleted.
    let writer = setup_wal_db(&source, 1000);
    let wal = tmp.path().join("tenant.db-wal");
    assert!(
      wal.exists() && std::fs::metadata(&wal).expect("wal").len() > 0,
      "expected a live WAL"
    );

    let size = snapshot_db_file_blocking(&source, &dest, Duration::from_secs(30)).expect("snap");
    assert!(size > 0);

    assert_eq!("ok", integrity_check(&dest));
    assert_eq!(1000, count_items(&dest));

    // The snapshot is a plain, self-contained database: further writes to
    // the source do not affect it.
    writer
      .execute("DELETE FROM items WHERE id <= 500", [])
      .expect("delete");
    assert_eq!(1000, count_items(&dest));
  }

  #[test]
  fn test_snapshot_zero_timeout() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let source = tmp.path().join("tenant.db");
    let dest = tmp.path().join("tenant.db.snap");

    let _writer = setup_wal_db(&source, 100);

    let result = snapshot_db_file_blocking(&source, &dest, Duration::ZERO);
    assert!(matches!(result, Err(SnapshotError::Timeout(_))));
    // No partial snapshot left behind.
    assert!(!dest.exists());
  }

  #[tokio::test]
  async fn test_snapshot_async_wrapper() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let source = tmp.path().join("tenant.db");
    let dest = tmp.path().join("nested").join("tenant.db.snap");

    drop(setup_wal_db(&source, 100));

    let size = snapshot_db_file(source, dest.clone(), Duration::from_secs(30))
      .await
      .expect("snap");
    assert_eq!(size, std::fs::metadata(&dest).expect("meta").len());
    assert_eq!(100, count_items(&dest));
  }
}
