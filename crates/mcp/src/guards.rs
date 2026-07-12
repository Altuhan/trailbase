//! Write guards for MCP record mutations: a per-process write budget, a
//! two-phase propose/confirm queue and column redaction for read results.
//! Ported from `examples/mcp-server` (which in turn adapted the guard design
//! of the applix-fr/mcp-trailbase project).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_PENDING: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
  #[error(
    "Write budget for this session is exhausted. A human can raise --budget-writes (0 = unlimited) and restart the server."
  )]
  BudgetExhausted,
  #[error("Too many unconfirmed writes (max {MAX_PENDING}); confirm or cancel pending ones first.")]
  TooManyPending,
  #[error(
    "No pending write with id '{0}'; it may have expired, been confirmed or been cancelled already."
  )]
  UnknownPending(String),
}

/// A parked record mutation waiting for `write_confirm`: enough request state
/// to replay it against the in-process router.
#[derive(Clone, Debug)]
pub struct PendingWrite {
  pub id: uuid::Uuid,
  pub tool: String,
  pub summary: String,
  pub method: String,
  pub path: String,
  pub body: Option<Value>,
  expires_at: Instant,
}

pub struct Guards {
  confirm_writes: bool,
  confirm_timeout: Duration,
  /// Remaining write budget; `None` means unlimited.
  remaining: Option<u32>,
  pending: HashMap<uuid::Uuid, PendingWrite>,
}

impl Guards {
  pub fn new(budget_writes: u32, confirm_writes: bool, confirm_timeout_secs: u64) -> Self {
    return Self {
      confirm_writes,
      confirm_timeout: Duration::from_secs(confirm_timeout_secs),
      remaining: (budget_writes > 0).then_some(budget_writes),
      pending: HashMap::new(),
    };
  }

  pub fn confirm_writes(&self) -> bool {
    return self.confirm_writes;
  }

  pub fn confirm_timeout_secs(&self) -> u64 {
    return self.confirm_timeout.as_secs();
  }

  pub fn budget_remaining(&self) -> Option<u32> {
    return self.remaining;
  }

  /// Consumes one unit of write budget or fails when exhausted.
  pub fn use_budget(&mut self) -> Result<(), GuardError> {
    match self.remaining {
      None => Ok(()),
      Some(0) => Err(GuardError::BudgetExhausted),
      Some(n) => {
        self.remaining = Some(n - 1);
        Ok(())
      }
    }
  }

  /// Parks a mutation for explicit confirmation.
  pub fn propose(
    &mut self,
    tool: &str,
    summary: String,
    method: &str,
    path: String,
    body: Option<Value>,
  ) -> Result<PendingWrite, GuardError> {
    self.purge_expired();
    if self.pending.len() >= MAX_PENDING {
      return Err(GuardError::TooManyPending);
    }
    let entry = PendingWrite {
      id: uuid::Uuid::new_v4(),
      tool: tool.to_string(),
      summary,
      method: method.to_string(),
      path,
      body,
      expires_at: Instant::now() + self.confirm_timeout,
    };
    self.pending.insert(entry.id, entry.clone());
    return Ok(entry);
  }

  /// Removes and returns a pending write, charging the budget. A failed
  /// execution does not refund the budget.
  pub fn confirm(&mut self, pending_id: &str) -> Result<PendingWrite, GuardError> {
    let entry = self.take(pending_id)?;
    self.use_budget()?;
    return Ok(entry);
  }

  pub fn cancel(&mut self, pending_id: &str) -> Result<PendingWrite, GuardError> {
    return self.take(pending_id);
  }

  fn take(&mut self, pending_id: &str) -> Result<PendingWrite, GuardError> {
    self.purge_expired();
    let id = uuid::Uuid::parse_str(pending_id)
      .map_err(|_| GuardError::UnknownPending(pending_id.to_string()))?;
    return self
      .pending
      .remove(&id)
      .ok_or_else(|| GuardError::UnknownPending(pending_id.to_string()));
  }

  fn purge_expired(&mut self) {
    let now = Instant::now();
    self.pending.retain(|_, entry| entry.expires_at > now);
  }
}

/// Recursively masks values of columns whose name matches any pattern,
/// including rows nested through foreign-key expansion.
pub fn redact(value: Value, patterns: &[regex::Regex]) -> Value {
  if patterns.is_empty() {
    return value;
  }
  return walk(value, patterns);
}

fn walk(value: Value, patterns: &[regex::Regex]) -> Value {
  match value {
    Value::Array(items) => {
      return Value::Array(items.into_iter().map(|item| walk(item, patterns)).collect());
    }
    Value::Object(map) => {
      return Value::Object(
        map
          .into_iter()
          .map(|(key, val)| {
            if patterns.iter().any(|p| p.is_match(&key)) {
              return (key, Value::String("[REDACTED]".to_string()));
            }
            return (key, walk(val, patterns));
          })
          .collect(),
      );
    }
    other => other,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn budget_exhaustion_and_unlimited() {
    let mut guards = Guards::new(2, false, 60);
    assert!(guards.use_budget().is_ok());
    assert!(guards.use_budget().is_ok());
    assert!(matches!(
      guards.use_budget(),
      Err(GuardError::BudgetExhausted)
    ));

    let mut unlimited = Guards::new(0, false, 60);
    for _ in 0..500 {
      assert!(unlimited.use_budget().is_ok());
    }
    assert_eq!(unlimited.budget_remaining(), None);
  }

  #[test]
  fn propose_confirm_is_single_use_and_charges_budget() {
    let mut guards = Guards::new(5, true, 60);
    let pending = guards
      .propose("records_create", "create".into(), "POST", "/x".into(), None)
      .expect("propose");

    let confirmed = guards.confirm(&pending.id.to_string()).expect("confirm");
    assert_eq!(confirmed.path, "/x");
    assert_eq!(guards.budget_remaining(), Some(4));

    assert!(matches!(
      guards.confirm(&pending.id.to_string()),
      Err(GuardError::UnknownPending(_))
    ));
  }

  #[test]
  fn cancel_does_not_charge() {
    let mut guards = Guards::new(5, true, 60);
    let pending = guards
      .propose(
        "records_delete",
        "delete".into(),
        "DELETE",
        "/x".into(),
        None,
      )
      .expect("propose");
    guards.cancel(&pending.id.to_string()).expect("cancel");
    assert_eq!(guards.budget_remaining(), Some(5));
  }

  #[test]
  fn pending_writes_expire_and_cap() {
    let mut guards = Guards::new(5, true, 0);
    let pending = guards
      .propose(
        "records_update",
        "update".into(),
        "PATCH",
        "/x".into(),
        None,
      )
      .expect("propose");
    // Zero timeout: expired immediately.
    assert!(matches!(
      guards.confirm(&pending.id.to_string()),
      Err(GuardError::UnknownPending(_))
    ));

    let mut capped = Guards::new(5, true, 3600);
    for i in 0..MAX_PENDING {
      capped
        .propose("records_create", format!("c{i}"), "POST", "/x".into(), None)
        .expect("propose");
    }
    assert!(matches!(
      capped.propose("records_create", "over".into(), "POST", "/x".into(), None),
      Err(GuardError::TooManyPending)
    ));
  }

  #[test]
  fn redact_masks_matching_columns_recursively() {
    let patterns = vec![
      regex::Regex::new("(?i)password").expect("regex"),
      regex::Regex::new("(?i)^secret").expect("regex"),
    ];
    let redacted = redact(
      json!({
        "records": [{
          "id": 1,
          "password_hash": "abc",
          "author": {"name": "x", "secret_note": "y"},
        }],
        "cursor": "c",
      }),
      &patterns,
    );
    assert_eq!(
      redacted,
      json!({
        "records": [{
          "id": 1,
          "password_hash": "[REDACTED]",
          "author": {"name": "x", "secret_note": "[REDACTED]"},
        }],
        "cursor": "c",
      })
    );
  }
}
