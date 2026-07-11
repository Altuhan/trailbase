use log::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Per-database record of the last successful upload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestEntry {
  /// Milliseconds since the Unix epoch of the moment the last successful
  /// backup *started*. Comparing file mtimes against the start (not the
  /// end) keeps writes that raced the snapshot marked as dirty.
  pub last_backup_start_ms: i64,
  /// Size of the uploaded snapshot in bytes.
  pub size_bytes: u64,
}

/// Local, durable record of what has been uploaded. Losing or corrupting it
/// is harmless: affected databases are merely considered dirty and get
/// re-uploaded by the next cycle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
  #[serde(default)]
  pub entries: HashMap<String, ManifestEntry>,
}

impl Manifest {
  pub(crate) fn load(path: &Path) -> Manifest {
    let bytes = match std::fs::read(path) {
      Ok(bytes) => bytes,
      Err(err) => {
        if err.kind() != std::io::ErrorKind::NotFound {
          warn!("Failed to read backup manifest {path:?}: {err}. Starting empty.");
        }
        return Manifest::default();
      }
    };

    return match serde_json::from_slice(&bytes) {
      Ok(manifest) => manifest,
      Err(err) => {
        warn!(
          "Corrupt backup manifest {path:?}: {err}. Starting empty — affected databases will be re-uploaded."
        );
        Manifest::default()
      }
    };
  }

  /// Atomic write: temp file in the same directory, then rename.
  pub(crate) fn store(&self, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, &bytes)?;
    return std::fs::rename(&tmp, path);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_missing_file_yields_empty_manifest() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let manifest = Manifest::load(&tmp.path().join("missing.json"));
    assert!(manifest.entries.is_empty());
  }

  #[test]
  fn test_roundtrip() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let path = tmp.path().join("state").join("r2_state.json");

    let mut manifest = Manifest::default();
    manifest.entries.insert(
      "tenant-0001".to_string(),
      ManifestEntry {
        last_backup_start_ms: 1_752_000_000_000,
        size_bytes: 4096,
      },
    );
    manifest.entries.insert(
      "main".to_string(),
      ManifestEntry {
        last_backup_start_ms: 1_752_000_060_000,
        size_bytes: 65536,
      },
    );

    manifest.store(&path).expect("store");
    assert_eq!(manifest, Manifest::load(&path));

    // No temp file left behind.
    assert!(!path.with_extension("json.tmp").exists());
  }

  #[test]
  fn test_store_replaces_existing_state() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let path = tmp.path().join("r2_state.json");

    let mut manifest = Manifest::default();
    manifest.entries.insert(
      "tenant".to_string(),
      ManifestEntry {
        last_backup_start_ms: 1,
        size_bytes: 1,
      },
    );
    manifest.store(&path).expect("store");

    manifest
      .entries
      .get_mut("tenant")
      .expect("present")
      .last_backup_start_ms = 2;
    manifest.store(&path).expect("store");

    assert_eq!(
      2,
      Manifest::load(&path)
        .entries
        .get("tenant")
        .expect("present")
        .last_backup_start_ms
    );
  }

  #[test]
  fn test_corrupt_file_yields_empty_manifest() {
    let tmp = temp_dir::TempDir::new().expect("tmp");
    let path = tmp.path().join("r2_state.json");
    std::fs::write(&path, b"{not json").expect("write");

    assert!(Manifest::load(&path).entries.is_empty());
  }
}
