//! Built-in backup of the depot's SQLite databases to an S3-compatible
//! object store, e.g. Cloudflare R2.
//!
//! One background `BackupService` with two triggers: connection-cache
//! eviction (dirty databases are enqueued when their connection is dropped
//! from the LRU cache) and a nightly scheduler job that sweeps all `*.db`
//! files. Snapshots use SQLite's online-backup API in-process; uploads
//! stream through the `object_store` crate. The whole subsystem is inert —
//! zero tasks, file descriptors or network traffic — unless configured.
//!
//! Working backlog: BACKUP_PLAN.md (iterations 1-9).

// The service consuming these lands in follow-up iterations (BACKUP_PLAN.md
// items 3-5); this allow goes away with it.
#![allow(dead_code)]

mod manifest;
mod snapshot;

pub(crate) use manifest::{Manifest, ManifestEntry};
pub(crate) use snapshot::{SnapshotError, snapshot_db_file};

use std::path::PathBuf;
use std::sync::Arc;

use object_store::ObjectStore;
use thiserror::Error;

pub(crate) const ENV_FS_DIR: &str = "TB_BACKUP_FS_DIR";
pub(crate) const ENV_S3_ENDPOINT: &str = "TB_BACKUP_S3_ENDPOINT";
pub(crate) const ENV_S3_BUCKET: &str = "TB_BACKUP_S3_BUCKET";
pub(crate) const ENV_S3_REGION: &str = "TB_BACKUP_S3_REGION";
pub(crate) const ENV_S3_ACCESS_KEY_ID: &str = "TB_BACKUP_S3_ACCESS_KEY_ID";
pub(crate) const ENV_S3_SECRET_ACCESS_KEY: &str = "TB_BACKUP_S3_SECRET_ACCESS_KEY";
pub(crate) const ENV_SCHEDULE: &str = "TB_BACKUP_SCHEDULE";
pub(crate) const ENV_CONCURRENCY: &str = "TB_BACKUP_CONCURRENCY";
pub(crate) const ENV_EPOCH_RETAIN_DAYS: &str = "TB_BACKUP_EPOCH_RETAIN_DAYS";
pub(crate) const ENV_INCLUDE_MAIN: &str = "TB_BACKUP_INCLUDE_MAIN";

/// Default nightly schedule. TrailBase cron runs in UTC; 22:00 UTC is 03:00
/// in Asia/Almaty (UTC+5).
pub(crate) const DEFAULT_SCHEDULE: &str = "0 0 22 * * * *";
pub(crate) const DEFAULT_CONCURRENCY: usize = 2;
pub(crate) const DEFAULT_EPOCH_RETAIN_DAYS: u32 = 14;

#[derive(Debug, Error)]
pub enum BackupConfigError {
  #[error("Incomplete backup config: {0}")]
  Incomplete(&'static str),
  #[error("Invalid backup setting {0}: {1}")]
  Invalid(&'static str, String),
  #[error("ObjectStore: {0}")]
  ObjectStore(#[from] object_store::Error),
}

/// Where snapshots are uploaded to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BackupStoreConfig {
  /// Any S3-compatible endpoint; for Cloudflare R2 use
  /// `https://<account-id>.r2.cloudflarestorage.com` and region `auto`.
  S3 {
    endpoint: String,
    bucket: String,
    region: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
  },
  /// Local directory acting as the "bucket"; meant for tests and e2e runs,
  /// not for disaster recovery (same disk as the data).
  Fs { dir: PathBuf },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BackupConfig {
  pub store: BackupStoreConfig,
  /// 7-field cron spec (with seconds), interpreted in UTC.
  pub schedule: String,
  /// Maximum concurrent snapshot+upload pipelines.
  pub concurrency: usize,
  /// Nightly `epochs/<date>/` copies older than this are deleted.
  pub epoch_retain_days: u32,
  /// Whether `main.db` is part of the sweep (logs/sessions never are).
  pub include_main: bool,
}

impl BackupConfig {
  /// Reads the configuration from process environment variables. Returns
  /// `None` when no store is configured, i.e. backups are disabled.
  pub(crate) fn from_env() -> Result<Option<BackupConfig>, BackupConfigError> {
    return Self::from_lookup(&|name| std::env::var(name).ok());
  }

  pub(crate) fn from_lookup(
    lookup: &dyn Fn(&str) -> Option<String>,
  ) -> Result<Option<BackupConfig>, BackupConfigError> {
    let non_empty = |name: &str| lookup(name).filter(|v| !v.is_empty());

    let fs_dir = non_empty(ENV_FS_DIR);
    let endpoint = non_empty(ENV_S3_ENDPOINT);
    let bucket = non_empty(ENV_S3_BUCKET);

    let store = match (fs_dir, endpoint, bucket) {
      (None, None, None) => return Ok(None),
      (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
        return Err(BackupConfigError::Invalid(
          ENV_FS_DIR,
          "conflicts with TB_BACKUP_S3_*; configure exactly one store".to_string(),
        ));
      }
      (Some(dir), None, None) => BackupStoreConfig::Fs {
        dir: PathBuf::from(dir),
      },
      (None, Some(endpoint), Some(bucket)) => BackupStoreConfig::S3 {
        endpoint,
        bucket,
        region: non_empty(ENV_S3_REGION),
        access_key_id: non_empty(ENV_S3_ACCESS_KEY_ID),
        secret_access_key: non_empty(ENV_S3_SECRET_ACCESS_KEY),
      },
      (None, Some(_), None) => return Err(BackupConfigError::Incomplete(ENV_S3_BUCKET)),
      (None, None, Some(_)) => return Err(BackupConfigError::Incomplete(ENV_S3_ENDPOINT)),
    };

    let concurrency = match non_empty(ENV_CONCURRENCY) {
      None => DEFAULT_CONCURRENCY,
      Some(v) => match v.parse::<usize>() {
        Ok(n) if n >= 1 => n,
        _ => {
          return Err(BackupConfigError::Invalid(ENV_CONCURRENCY, v));
        }
      },
    };

    let epoch_retain_days = match non_empty(ENV_EPOCH_RETAIN_DAYS) {
      None => DEFAULT_EPOCH_RETAIN_DAYS,
      Some(v) => match v.parse::<u32>() {
        Ok(n) if n >= 1 => n,
        _ => {
          return Err(BackupConfigError::Invalid(ENV_EPOCH_RETAIN_DAYS, v));
        }
      },
    };

    let include_main = match non_empty(ENV_INCLUDE_MAIN).as_deref() {
      None => true,
      Some("TRUE") | Some("true") | Some("1") => true,
      Some("FALSE") | Some("false") | Some("0") => false,
      Some(v) => {
        return Err(BackupConfigError::Invalid(ENV_INCLUDE_MAIN, v.to_string()));
      }
    };

    return Ok(Some(BackupConfig {
      store,
      schedule: non_empty(ENV_SCHEDULE).unwrap_or_else(|| DEFAULT_SCHEDULE.to_string()),
      concurrency,
      epoch_retain_days,
      include_main,
    }));
  }

  pub(crate) fn build_store(&self) -> Result<Arc<dyn ObjectStore>, BackupConfigError> {
    return match &self.store {
      BackupStoreConfig::Fs { dir } => {
        std::fs::create_dir_all(dir)
          .map_err(|err| BackupConfigError::Invalid(ENV_FS_DIR, err.to_string()))?;
        Ok(Arc::new(
          object_store::local::LocalFileSystem::new_with_prefix(dir)?,
        ))
      }
      BackupStoreConfig::S3 {
        endpoint,
        bucket,
        region,
        access_key_id,
        secret_access_key,
      } => {
        let mut builder = object_store::aws::AmazonS3Builder::new()
          .with_endpoint(endpoint.clone())
          .with_bucket_name(bucket.clone())
          .with_region(region.clone().unwrap_or_else(|| "auto".to_string()));

        if let Some(key) = access_key_id {
          builder = builder.with_access_key_id(key.clone());
        }
        if let Some(secret) = secret_access_key {
          builder = builder.with_secret_access_key(secret.clone());
        }
        if endpoint.starts_with("http://") {
          // Plain-http endpoints only occur for local test setups.
          builder = builder
            .with_client_options(object_store::ClientOptions::default().with_allow_http(true));
        }

        Ok(Arc::new(builder.build()?))
      }
    };
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
      .iter()
      .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
      .collect();
    return move |name: &str| map.get(name).cloned();
  }

  #[test]
  fn test_disabled_without_store() {
    assert_eq!(None, BackupConfig::from_lookup(&lookup(&[])).expect("ok"));

    // Tuning knobs alone do not enable backups.
    let vars = lookup(&[(ENV_CONCURRENCY, "8")]);
    assert_eq!(None, BackupConfig::from_lookup(&vars).expect("ok"));
  }

  #[test]
  fn test_fs_store_with_defaults() {
    let vars = lookup(&[(ENV_FS_DIR, "/tmp/bucket")]);
    let config = BackupConfig::from_lookup(&vars).expect("ok").expect("some");

    assert_eq!(
      BackupStoreConfig::Fs {
        dir: PathBuf::from("/tmp/bucket")
      },
      config.store
    );
    assert_eq!(DEFAULT_SCHEDULE, config.schedule);
    assert_eq!(DEFAULT_CONCURRENCY, config.concurrency);
    assert_eq!(DEFAULT_EPOCH_RETAIN_DAYS, config.epoch_retain_days);
    assert!(config.include_main);
  }

  #[test]
  fn test_s3_store_with_overrides() {
    let vars = lookup(&[
      (ENV_S3_ENDPOINT, "https://acc.r2.cloudflarestorage.com"),
      (ENV_S3_BUCKET, "tb-backups"),
      (ENV_S3_ACCESS_KEY_ID, "key"),
      (ENV_S3_SECRET_ACCESS_KEY, "secret"),
      (ENV_SCHEDULE, "0 0 1 * * * *"),
      (ENV_CONCURRENCY, "4"),
      (ENV_EPOCH_RETAIN_DAYS, "30"),
      (ENV_INCLUDE_MAIN, "false"),
    ]);
    let config = BackupConfig::from_lookup(&vars).expect("ok").expect("some");

    let BackupStoreConfig::S3 {
      endpoint,
      bucket,
      region,
      access_key_id,
      secret_access_key,
    } = config.store
    else {
      panic!("expected S3 store");
    };
    assert_eq!("https://acc.r2.cloudflarestorage.com", endpoint);
    assert_eq!("tb-backups", bucket);
    assert_eq!(None, region);
    assert_eq!(Some("key".to_string()), access_key_id);
    assert_eq!(Some("secret".to_string()), secret_access_key);

    assert_eq!("0 0 1 * * * *", config.schedule);
    assert_eq!(4, config.concurrency);
    assert_eq!(30, config.epoch_retain_days);
    assert!(!config.include_main);
  }

  #[test]
  fn test_invalid_configs() {
    // Partial S3.
    let vars = lookup(&[(ENV_S3_ENDPOINT, "https://acc.r2.example.com")]);
    assert!(BackupConfig::from_lookup(&vars).is_err());

    let vars = lookup(&[(ENV_S3_BUCKET, "tb-backups")]);
    assert!(BackupConfig::from_lookup(&vars).is_err());

    // Conflicting stores.
    let vars = lookup(&[
      (ENV_FS_DIR, "/tmp/bucket"),
      (ENV_S3_ENDPOINT, "https://acc.r2.example.com"),
      (ENV_S3_BUCKET, "tb-backups"),
    ]);
    assert!(BackupConfig::from_lookup(&vars).is_err());

    // Bad numbers / booleans.
    let base = [(ENV_FS_DIR, "/tmp/bucket")];
    for (key, value) in [
      (ENV_CONCURRENCY, "0"),
      (ENV_CONCURRENCY, "nope"),
      (ENV_EPOCH_RETAIN_DAYS, "0"),
      (ENV_INCLUDE_MAIN, "yes"),
    ] {
      let mut vars: Vec<(&str, &str)> = base.to_vec();
      vars.push((key, value));
      assert!(
        BackupConfig::from_lookup(&lookup(&vars)).is_err(),
        "expected error for {key}={value}"
      );
    }
  }

  #[test]
  fn test_build_fs_store() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let dir = tmp.path().join("bucket");

    let config = BackupConfig::from_lookup(&lookup(&[(ENV_FS_DIR, dir.to_str().expect("utf-8"))]))
      .expect("ok")
      .expect("some");

    // Builds and creates the directory.
    let _store = config.build_store().expect("store");
    assert!(dir.is_dir());
  }
}
