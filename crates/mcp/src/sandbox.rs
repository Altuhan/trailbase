//! Ephemeral sandbox: a consistent snapshot of the live depot run as a
//! child `trail` process on localhost. Schema work and arbitrary SQL happen
//! there — never against the live instance — and DDL applied through the
//! admin API is recorded as migration files, the reviewable artifact
//! surfaced by `sandbox_diff`. Ported from `examples/mcp-server`'s
//! SandboxManager.
//!
//! Session/logs/queue databases and `secrets/` are intentionally NOT
//! copied: the child generates fresh signing keys, so production tokens
//! never work against the sandbox.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const SANDBOX_ADMIN_EMAIL: &str = "sandbox-admin@localhost";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SandboxError(pub String);

fn err(message: impl Into<String>) -> SandboxError {
  return SandboxError(message.into());
}

pub struct SandboxManager {
  source_data_dir: PathBuf,
  http: reqwest::Client,
  active: Option<ActiveSandbox>,
}

struct ActiveSandbox {
  dir: PathBuf,
  url: String,
  created_at_epoch_secs: u64,
  child: tokio::process::Child,
  admin_token: String,
  csrf_token: String,
  baseline_migrations: BTreeSet<String>,
  baseline_config: Option<String>,
}

impl SandboxManager {
  pub fn new(source_data_dir: PathBuf) -> Self {
    return Self {
      source_data_dir,
      http: reqwest::Client::new(),
      active: None,
    };
  }

  pub fn is_active(&self) -> bool {
    return self.active.is_some();
  }

  fn require(&self) -> Result<&ActiveSandbox, SandboxError> {
    return self
      .active
      .as_ref()
      .ok_or_else(|| err("No active sandbox. Create one with sandbox_create."));
  }

  pub async fn create(&mut self) -> Result<Value, SandboxError> {
    if let Some(ref active) = self.active {
      return Err(err(format!(
        "A sandbox is already running at {}; destroy it first (sandbox_destroy).",
        active.url
      )));
    }

    let source = self.source_data_dir.clone();
    let dir = std::env::temp_dir().join(format!("trailbase-sandbox-{}", uuid::Uuid::new_v4()));

    let result = self.create_in(&source, &dir).await;
    if result.is_err() {
      let _ = std::fs::remove_dir_all(&dir);
    }
    return result;
  }

  async fn create_in(&mut self, source: &Path, dir: &Path) -> Result<Value, SandboxError> {
    // 1. Assemble the depot copy: consistent main.db snapshot + config +
    //    migrations (blocking I/O off the async runtime).
    let (baseline_migrations, baseline_config) = {
      let source = source.to_path_buf();
      let dir = dir.to_path_buf();
      tokio::task::spawn_blocking(move || assemble_sandbox_depot(&source, &dir))
        .await
        .map_err(|e| err(format!("snapshot task panicked: {e}")))??
    };

    // 2. Bootstrap a sandbox admin via the CLI on the copy; this also
    //    applies pending migrations and generates fresh keys.
    let exe = std::env::current_exe().map_err(|e| err(format!("cannot locate own binary: {e}")))?;
    let admin_password = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    run_cli(
      &exe,
      &[
        "--data-dir",
        &dir.to_string_lossy(),
        "user",
        "add",
        SANDBOX_ADMIN_EMAIL,
        &admin_password,
      ],
    )
    .await?;
    run_cli(
      &exe,
      &[
        "--data-dir",
        &dir.to_string_lossy(),
        "admin",
        "promote",
        SANDBOX_ADMIN_EMAIL,
      ],
    )
    .await?;

    // 3. Run the sandbox server on a free localhost port.
    let port = free_port()?;
    let address = format!("127.0.0.1:{port}");
    let url = format!("http://{address}");
    let mut child = tokio::process::Command::new(&exe)
      .args([
        "--data-dir",
        &dir.to_string_lossy(),
        "run",
        "--address",
        &address,
      ])
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .kill_on_drop(true)
      .spawn()
      .map_err(|e| err(format!("failed to spawn sandbox server: {e}")))?;

    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
      if let Ok(Some(status)) = child.try_wait() {
        return Err(err(format!("Sandbox server exited early: {status}")));
      }
      if let Ok(response) = self.http.get(format!("{url}/api/healthcheck")).send().await
        && response.status().is_success()
      {
        break;
      }
      if Instant::now() >= deadline {
        let _ = child.start_kill();
        return Err(err("Sandbox server did not become healthy in time."));
      }
      tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // 4. Log in as the sandbox admin over HTTP.
    #[derive(serde::Deserialize)]
    struct LoginResponse {
      auth_token: String,
      csrf_token: String,
    }
    let login: LoginResponse = self
      .http
      .post(format!("{url}/api/auth/v1/login"))
      .json(&json!({ "email": SANDBOX_ADMIN_EMAIL, "password": admin_password }))
      .send()
      .await
      .map_err(|e| err(format!("sandbox admin login failed: {e}")))?
      .error_for_status()
      .map_err(|e| err(format!("sandbox admin login failed: {e}")))?
      .json()
      .await
      .map_err(|e| err(format!("sandbox admin login returned no tokens: {e}")))?;

    let created_at_epoch_secs = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs();
    let manifest = json!({
      "url": url,
      "data_dir": dir,
      "source_data_dir": source,
      "created_at_epoch_secs": created_at_epoch_secs,
      "admin_email": SANDBOX_ADMIN_EMAIL,
      "baseline_migrations": baseline_migrations,
    });

    self.active = Some(ActiveSandbox {
      dir: dir.to_path_buf(),
      url,
      created_at_epoch_secs,
      child,
      admin_token: login.auth_token,
      csrf_token: login.csrf_token,
      baseline_migrations,
      baseline_config,
    });
    return Ok(manifest);
  }

  pub async fn status(&mut self) -> Value {
    let Some(ref mut active) = self.active else {
      return json!({ "active": false });
    };
    let exited = matches!(active.child.try_wait(), Ok(Some(_)));
    let healthy = !exited
      && match self
        .http
        .get(format!("{}/api/healthcheck", active.url))
        .send()
        .await
      {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
      };
    return json!({
      "active": true,
      "url": active.url,
      "data_dir": active.dir,
      "created_at_epoch_secs": active.created_at_epoch_secs,
      "healthy": healthy,
    });
  }

  /// Sends one authenticated request to the sandbox's admin API. Only ever
  /// used against the ephemeral child instance.
  pub async fn admin_call(
    &self,
    method: reqwest::Method,
    admin_path: &str,
    body: Option<&Value>,
  ) -> Result<(u16, Value), SandboxError> {
    let active = self.require()?;
    let mut request = self
      .http
      .request(method, format!("{}/api/_admin{admin_path}", active.url))
      .header("Authorization", format!("Bearer {}", active.admin_token))
      .header("CSRF-Token", &active.csrf_token);
    if let Some(body) = body {
      request = request.json(body);
    }
    let response = request
      .send()
      .await
      .map_err(|e| err(format!("sandbox admin request failed: {e}")))?;
    let status = response.status().as_u16();
    let text = response
      .text()
      .await
      .map_err(|e| err(format!("sandbox admin response unreadable: {e}")))?;
    let value = if text.is_empty() {
      Value::Null
    } else {
      serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    return Ok((status, value));
  }

  /// What changed relative to sandbox creation: newly recorded migration
  /// files (the reviewable artifact to apply to production) and
  /// config.textproto changes.
  pub fn diff(&self) -> Result<Value, SandboxError> {
    let active = self.require()?;

    let mut new_migrations = Vec::new();
    for file in list_main_migrations(&active.dir) {
      if !active.baseline_migrations.contains(&file) {
        let path = active.dir.join("migrations").join("main").join(&file);
        let content = std::fs::read_to_string(&path)
          .map_err(|e| err(format!("cannot read migration {file}: {e}")))?;
        new_migrations.push(json!({
          "file": format!("migrations/main/{file}"),
          "content": content,
        }));
      }
    }

    let config_after = read_config(&active.dir);
    let config_changed = config_after != active.baseline_config;
    let mut result = json!({
      "new_migrations": new_migrations,
      "config_changed": config_changed,
    });
    if config_changed {
      result["config_before"] = Value::from(active.baseline_config.clone());
      result["config_after"] = Value::from(config_after);
    }
    return Ok(result);
  }

  pub async fn destroy(&mut self, keep_dir: bool) -> Result<Value, SandboxError> {
    let mut active = self
      .active
      .take()
      .ok_or_else(|| err("No active sandbox. Create one with sandbox_create."))?;

    let _ = active.child.start_kill();
    let _ = active.child.wait().await;

    if !keep_dir {
      std::fs::remove_dir_all(&active.dir)
        .map_err(|e| err(format!("failed to remove sandbox dir: {e}")))?;
    }
    return Ok(json!({ "data_dir": active.dir, "removed": !keep_dir }));
  }
}

async fn run_cli(exe: &Path, args: &[&str]) -> Result<(), SandboxError> {
  let output = tokio::process::Command::new(exe)
    .args(args)
    .stdin(std::process::Stdio::null())
    .output()
    .await
    .map_err(|e| err(format!("failed to run {exe:?}: {e}")))?;
  if !output.status.success() {
    return Err(err(format!(
      "'trail {}' failed: {}",
      args.join(" "),
      String::from_utf8_lossy(&output.stderr)
    )));
  }
  return Ok(());
}

/// Takes a transactionally consistent snapshot of a (potentially live, WAL)
/// SQLite database via the Online Backup API — page-level copying that,
/// unlike `VACUUM INTO`, never re-executes schema SQL. That matters because
/// TrailBase schemas reference extension functions (`jsonschema`,
/// `is_email`, ...) unknown to a plain connection.
fn snapshot_sqlite_db(source: &Path, dest: &Path) -> Result<(), SandboxError> {
  let src = rusqlite::Connection::open_with_flags(
    source,
    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
  )
  .map_err(|e| err(format!("cannot open source db {source:?}: {e}")))?;
  let mut dst = rusqlite::Connection::open(dest)
    .map_err(|e| err(format!("cannot create snapshot {dest:?}: {e}")))?;
  let backup = rusqlite::backup::Backup::new(&src, &mut dst)
    .map_err(|e| err(format!("backup init failed: {e}")))?;
  backup
    .run_to_completion(64, Duration::from_millis(10), None)
    .map_err(|e| err(format!("backup failed: {e}")))?;
  return Ok(());
}

/// Depot copy: main.db snapshot + config + migrations. No sessions, logs or
/// secrets.
fn assemble_sandbox_depot(
  source: &Path,
  sandbox_dir: &Path,
) -> Result<(BTreeSet<String>, Option<String>), SandboxError> {
  let source_db = source.join("data").join("main.db");
  if !source_db.exists() {
    return Err(err(format!(
      "'{}' does not exist; is '{}' a TrailBase data dir?",
      source_db.display(),
      source.display()
    )));
  }

  std::fs::create_dir_all(sandbox_dir.join("data"))
    .map_err(|e| err(format!("cannot create sandbox dir: {e}")))?;
  snapshot_sqlite_db(&source_db, &sandbox_dir.join("data").join("main.db"))?;

  let source_config = source.join("config.textproto");
  if source_config.exists() {
    std::fs::copy(&source_config, sandbox_dir.join("config.textproto"))
      .map_err(|e| err(format!("cannot copy config: {e}")))?;
  }

  let source_migrations = source.join("migrations");
  if source_migrations.exists() {
    copy_dir_all(&source_migrations, &sandbox_dir.join("migrations"))?;
  }

  return Ok((
    list_main_migrations(sandbox_dir).into_iter().collect(),
    read_config(sandbox_dir),
  ));
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), SandboxError> {
  std::fs::create_dir_all(dst).map_err(|e| err(format!("mkdir {dst:?}: {e}")))?;
  let entries = std::fs::read_dir(src).map_err(|e| err(format!("readdir {src:?}: {e}")))?;
  for entry in entries {
    let entry = entry.map_err(|e| err(format!("readdir {src:?}: {e}")))?;
    let target = dst.join(entry.file_name());
    let file_type = entry
      .file_type()
      .map_err(|e| err(format!("stat {:?}: {e}", entry.path())))?;
    if file_type.is_dir() {
      copy_dir_all(&entry.path(), &target)?;
    } else if file_type.is_file() {
      std::fs::copy(entry.path(), &target)
        .map_err(|e| err(format!("copy {:?}: {e}", entry.path())))?;
    }
  }
  return Ok(());
}

fn list_main_migrations(data_dir: &Path) -> Vec<String> {
  let dir = data_dir.join("migrations").join("main");
  let Ok(entries) = std::fs::read_dir(&dir) else {
    return vec![];
  };
  let mut files: Vec<String> = entries
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .filter(|name| name.ends_with(".sql"))
    .collect();
  files.sort();
  return files;
}

fn read_config(data_dir: &Path) -> Option<String> {
  return std::fs::read_to_string(data_dir.join("config.textproto")).ok();
}

fn free_port() -> Result<u16, SandboxError> {
  let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
    .map_err(|e| err(format!("cannot allocate port: {e}")))?;
  let port = listener
    .local_addr()
    .map_err(|e| err(format!("cannot read allocated port: {e}")))?
    .port();
  return Ok(port);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn migration_listing_sorts_and_filters() {
    let dir = std::env::temp_dir().join(format!("tb-mcp-sbx-test-{}", uuid::Uuid::new_v4()));
    let main = dir.join("migrations").join("main");
    std::fs::create_dir_all(&main).expect("mkdir");
    std::fs::write(main.join("U2__b.sql"), "b").expect("write");
    std::fs::write(main.join("U1__a.sql"), "a").expect("write");
    std::fs::write(main.join("notes.txt"), "x").expect("write");

    assert_eq!(list_main_migrations(&dir), vec!["U1__a.sql", "U2__b.sql"]);
    std::fs::remove_dir_all(&dir).expect("cleanup");
  }

  #[test]
  fn snapshot_copies_pages_without_reexecuting_schema() {
    let dir = std::env::temp_dir().join(format!("tb-mcp-sbx-snap-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let source = dir.join("src.db");
    let dest = dir.join("dst.db");

    let conn = rusqlite::Connection::open(&source).expect("open");
    conn
      .execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t (v) VALUES ('x');",
      )
      .expect("seed");
    drop(conn);

    snapshot_sqlite_db(&source, &dest).expect("snapshot");

    let copy = rusqlite::Connection::open(&dest).expect("open copy");
    let count: i64 = copy
      .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
      .expect("count");
    assert_eq!(count, 1);
    drop(copy);
    std::fs::remove_dir_all(&dir).expect("cleanup");
  }
}
