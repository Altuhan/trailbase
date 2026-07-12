//! MCP server embedded into TrailBase.
//!
//! `trail mcp` speaks the Model Context Protocol over stdio and translates
//! tool calls into **in-process** requests against TrailBase's real axum
//! router: the exact same auth middleware, record API handlers and ACL
//! checks run as for network traffic, but no socket is ever bound. Tool
//! calls act as a dedicated (non-admin) user whose credentials are checked
//! with the regular login flow at startup, so everything the agent can see
//! or change is enforced server-side.
//!
//! Record mutations are additionally protected by write guards (budget,
//! two-phase confirmation, column redaction) ported from
//! `examples/mcp-server`.
#![forbid(unsafe_code, clippy::unwrap_used)]
#![allow(clippy::needless_return)]
#![warn(clippy::await_holding_lock, clippy::inefficient_to_string)]

mod guards;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rmcp::{
  ErrorData as McpError, ServerHandler, ServiceExt as _,
  handler::server::wrapper::Parameters,
  model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
  schemars, tool, tool_handler, tool_router,
  transport::stdio,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use trailbase::api::UserIdentifier;
use trailbase::{AppState, DataDir, InitArgs, Server, ServerOptions};

use crate::guards::{Guards, redact};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const RECORDS_BASE: &str = "/api/records/v1";
/// Generous cap for in-process response bodies handed to the model.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpMode {
  /// Listing/reading tools only.
  ReadOnly,
  /// Additionally exposes guarded record mutations.
  Records,
}

pub struct McpOptions {
  pub data_dir: DataDir,
  pub public_url: Option<url::Url>,
  /// Email of the user tool calls act as; record API ACLs apply server-side.
  pub user: String,
  pub password: String,
  pub mode: McpMode,
  /// Mutations allowed per process; 0 = unlimited.
  pub budget_writes: u32,
  /// Two-phase (propose/confirm) record mutations.
  pub confirm_writes: bool,
  pub confirm_timeout_secs: u64,
  /// Case-insensitive regexes; matching column names are masked in reads.
  pub redact_columns: Vec<String>,
}

/// Boots the instance from the given depot and serves MCP over stdio until
/// the client disconnects. Never binds a network listener.
pub async fn run_stdio(options: McpOptions) -> Result<(), BoxError> {
  let redact_patterns = compile_redact_patterns(&options.redact_columns)?;

  let (_new, state) = AppState::init(InitArgs {
    data_dir: options.data_dir.clone(),
    public_url: options.public_url.clone(),
    ..Default::default()
  })
  .await?;

  // A real login (password check + rate limiting): tool calls then carry the
  // minted token through the very same auth middleware as network requests.
  let tokens = trailbase::api::login_with_password_for_test(
    &state,
    UserIdentifier::Email(options.user.clone()),
    &options.password,
  )
  .await
  .map_err(|err| format!("Login as '{}' failed: {err}", options.user))?
  .ok_or("Login requires a second factor; MFA users are not supported by `trail mcp`.")?;

  // Build the full router without ever serving it; `log_responses: false`
  // keeps stdout clean for the MCP protocol (request logs go to `_logs`).
  let server = Server::init(
    state.clone(),
    ServerOptions {
      address: "localhost:0".to_string(),
      admin_address: None,
      public_dir: None,
      public_dir_spa: false,
      log_responses: false,
      cors_allowed_origins: vec![],
      tls_cert: None,
      tls_key: None,
      custom_router: None,
    },
  )
  .await?;

  let handler = TrailBaseMcp(Arc::new(Inner {
    router: server.main_router.1.clone(),
    state,
    auth_token: tokens.auth_token,
    user_email: options.user,
    user_id: tokens.id,
    mode: options.mode,
    guards: Mutex::new(Guards::new(
      options.budget_writes,
      options.mode == McpMode::Records && options.confirm_writes,
      options.confirm_timeout_secs,
    )),
    redact_patterns,
  }));

  log::info!(
    "trail mcp: stdio server ready (mode={:?}, acting user={})",
    options.mode,
    handler.0.user_email
  );

  let service = handler.serve(stdio()).await?;
  service.waiting().await?;
  return Ok(());
}

fn compile_redact_patterns(patterns: &[String]) -> Result<Vec<regex::Regex>, BoxError> {
  return patterns
    .iter()
    .filter(|p| !p.trim().is_empty())
    .map(|p| {
      regex::Regex::new(&format!("(?i){}", p.trim()))
        .map_err(|err| format!("Invalid --redact-columns pattern '{p}': {err}").into())
    })
    .collect();
}

#[derive(Clone)]
pub struct TrailBaseMcp(Arc<Inner>);

struct Inner {
  router: axum::Router,
  state: AppState,
  auth_token: String,
  user_email: String,
  user_id: uuid::Uuid,
  mode: McpMode,
  guards: Mutex<Guards>,
  redact_patterns: Vec<regex::Regex>,
}

fn internal(err: impl std::fmt::Display) -> McpError {
  return McpError::internal_error(err.to_string(), None);
}

fn error_result(message: impl Into<String>) -> Result<CallToolResult, McpError> {
  return Ok(CallToolResult::error(vec![ContentBlock::text(
    message.into(),
  )]));
}

fn json_result(value: &Value) -> Result<CallToolResult, McpError> {
  return Ok(CallToolResult::success(vec![ContentBlock::text(
    serde_json::to_string_pretty(value).map_err(internal)?,
  )]));
}

/// Percent-encodes one path segment (API name or record id) so it cannot
/// change the path shape before it reaches the router.
fn encode_segment(segment: &str) -> Result<String, McpError> {
  if segment.is_empty() {
    return Err(McpError::invalid_params("empty path segment", None));
  }
  let mut encoded = String::with_capacity(segment.len());
  for byte in segment.bytes() {
    match byte {
      b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
        encoded.push(byte as char);
      }
      _ => {
        encoded.push_str(&format!("%{byte:02X}"));
      }
    }
  }
  return Ok(encoded);
}

// Tool parameter types. Doc comments become the JSON schema descriptions.

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsSchemaParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsListParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
  /// Maximum number of records to return.
  pub limit: Option<u32>,
  /// Cursor from a previous response.
  pub cursor: Option<String>,
  /// Pagination offset.
  pub offset: Option<u32>,
  /// Columns to order by; prefix with '-' for descending.
  pub order: Option<Vec<String>>,
  /// Include the total row count.
  pub count: Option<bool>,
  /// Foreign-key columns to expand inline.
  pub expand: Option<Vec<String>>,
  /// Raw trailbase-qs filter expression, e.g. `filter[status][$eq]=done`
  /// or `filter[age][$gte]=18&filter[name][$like]=%25doe`.
  pub filter: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsReadParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
  /// Record id (integer or UUID primary key), passed as a string.
  pub id: String,
  /// Foreign-key columns to expand inline.
  pub expand: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsCreateParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
  /// Column/value map for the new record.
  pub record: serde_json::Map<String, Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsUpdateParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
  /// Record id (integer or UUID primary key), passed as a string.
  pub id: String,
  /// Column/value map of fields to update.
  pub record: serde_json::Map<String, Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordsDeleteParams {
  /// Name of the record API as configured in TrailBase.
  pub api: String,
  /// Record id (integer or UUID primary key), passed as a string.
  pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PendingParams {
  /// `pending_id` returned by a records_* mutation.
  pub pending_id: String,
}

#[tool_router]
impl TrailBaseMcp {
  /// Sends one request through the in-process router with the acting user's
  /// token attached; auth middleware and ACLs run exactly as over the network.
  async fn call(
    &self,
    method: &str,
    path_and_query: &str,
    body: Option<&Value>,
  ) -> Result<(StatusCode, Value), McpError> {
    let mut builder = Request::builder()
      .method(method)
      .uri(path_and_query)
      .header(
        header::AUTHORIZATION,
        format!("Bearer {}", self.0.auth_token),
      );
    if body.is_some() {
      builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = match body {
      Some(value) => builder.body(Body::from(serde_json::to_vec(value).map_err(internal)?)),
      None => builder.body(Body::empty()),
    }
    .map_err(internal)?;

    let response = self
      .0
      .router
      .clone()
      .oneshot(request)
      .await
      .map_err(internal)?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
      .await
      .map_err(internal)?;
    let value: Value = if bytes.is_empty() {
      Value::Null
    } else {
      serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    return Ok((status, value));
  }

  fn respond(
    &self,
    status: StatusCode,
    value: Value,
    redact_reads: bool,
  ) -> Result<CallToolResult, McpError> {
    if !status.is_success() {
      return error_result(format!("HTTP {status}: {value}"));
    }
    let value = if redact_reads {
      redact(value, &self.0.redact_patterns)
    } else {
      value
    };
    return json_result(&value);
  }

  /// Runs a mutation through the write guards: parked for confirmation, or
  /// executed directly against the budget.
  async fn guarded_write(
    &self,
    tool: &str,
    summary: String,
    method: &str,
    path: String,
    body: Option<Value>,
  ) -> Result<CallToolResult, McpError> {
    if self.0.mode != McpMode::Records {
      return error_result(
        "Mutations are disabled in read-only mode; restart `trail mcp` with `--mode records`.",
      );
    }

    enum Next {
      Pending(guards::PendingWrite),
      Execute,
    }
    let next = {
      let mut guards = self.0.guards.lock().expect("poisoned");
      if guards.confirm_writes() {
        match guards.propose(tool, summary.clone(), method, path.clone(), body.clone()) {
          Ok(pending) => Next::Pending(pending),
          Err(err) => return error_result(err.to_string()),
        }
      } else {
        if let Err(err) = guards.use_budget() {
          return error_result(err.to_string());
        }
        Next::Execute
      }
    };

    match next {
      Next::Pending(pending) => {
        let timeout = {
          let guards = self.0.guards.lock().expect("poisoned");
          guards.confirm_timeout_secs()
        };
        return json_result(&json!({
          "status": "pending_confirmation",
          "pending_id": pending.id.to_string(),
          "tool": pending.tool,
          "action": summary,
          "expires_in_secs": timeout,
          "next": "Nothing was written yet. Call write_confirm with this pending_id to execute, or write_cancel to discard.",
        }));
      }
      Next::Execute => {
        let (status, value) = self.call(method, &path, body.as_ref()).await?;
        return self.respond(status, value, false);
      }
    }
  }

  #[tool(
    description = "Lists the record APIs configured on this TrailBase instance (API name and backing table). Access to each API is still enforced per-user by the server."
  )]
  async fn records_apis(&self) -> Result<CallToolResult, McpError> {
    let apis: Vec<Value> = self.0.state.access_config(|config| {
      return config
        .record_apis
        .iter()
        .map(|api| {
          json!({
            "name": api.name,
            "table": api.table_name,
          })
        })
        .collect();
    });
    return json_result(&json!({ "record_apis": apis }));
  }

  #[tool(description = "Fetches the JSON schema describing records of the given record API.")]
  async fn records_schema(
    &self,
    params: Parameters<RecordsSchemaParams>,
  ) -> Result<CallToolResult, McpError> {
    let api = encode_segment(&params.0.api)?;
    let (status, value) = self
      .call("GET", &format!("{RECORDS_BASE}/{api}/schema"), None)
      .await?;
    return self.respond(status, value, false);
  }

  #[tool(
    description = "Lists records of a TrailBase record API with optional filters, ordering and cursor/offset pagination. Access is enforced server-side by the API's ACLs for the acting user."
  )]
  async fn records_list(
    &self,
    params: Parameters<RecordsListParams>,
  ) -> Result<CallToolResult, McpError> {
    let p = params.0;
    let api = encode_segment(&p.api)?;

    // Scoped so the (non-Send) serializer is dropped before any await.
    let query = {
      let mut serializer = url::form_urlencoded::Serializer::new(String::new());
      if let Some(limit) = p.limit {
        serializer.append_pair("limit", &limit.to_string());
      }
      if let Some(ref cursor) = p.cursor {
        serializer.append_pair("cursor", cursor);
      }
      if let Some(offset) = p.offset {
        serializer.append_pair("offset", &offset.to_string());
      }
      if let Some(ref order) = p.order
        && !order.is_empty()
      {
        serializer.append_pair("order", &order.join(","));
      }
      if p.count == Some(true) {
        serializer.append_pair("count", "true");
      }
      if let Some(ref expand) = p.expand
        && !expand.is_empty()
      {
        serializer.append_pair("expand", &expand.join(","));
      }
      let mut query = serializer.finish();
      if let Some(ref filter) = p.filter
        && !filter.is_empty()
      {
        if !query.is_empty() {
          query.push('&');
        }
        query.push_str(filter);
      }
      query
    };

    let path = if query.is_empty() {
      format!("{RECORDS_BASE}/{api}")
    } else {
      format!("{RECORDS_BASE}/{api}?{query}")
    };
    let (status, value) = self.call("GET", &path, None).await?;
    return self.respond(status, value, true);
  }

  #[tool(description = "Reads a single record by id from a TrailBase record API.")]
  async fn records_read(
    &self,
    params: Parameters<RecordsReadParams>,
  ) -> Result<CallToolResult, McpError> {
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let id = encode_segment(&p.id)?;
    let mut path = format!("{RECORDS_BASE}/{api}/{id}");
    if let Some(ref expand) = p.expand
      && !expand.is_empty()
    {
      // Scoped so the (non-Send) serializer is dropped before any await.
      let query = {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("expand", &expand.join(","));
        serializer.finish()
      };
      path = format!("{path}?{query}");
    }
    let (status, value) = self.call("GET", &path, None).await?;
    return self.respond(status, value, true);
  }

  #[tool(
    description = "Creates a record via a TrailBase record API. Write access is enforced server-side; in records mode the mutation may require write_confirm."
  )]
  async fn records_create(
    &self,
    params: Parameters<RecordsCreateParams>,
  ) -> Result<CallToolResult, McpError> {
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let columns = p.record.keys().cloned().collect::<Vec<_>>().join(", ");
    return self
      .guarded_write(
        "records_create",
        format!("create record in '{}' with columns [{columns}]", p.api),
        "POST",
        format!("{RECORDS_BASE}/{api}"),
        Some(Value::Object(p.record)),
      )
      .await;
  }

  #[tool(description = "Partially updates an existing record by id.")]
  async fn records_update(
    &self,
    params: Parameters<RecordsUpdateParams>,
  ) -> Result<CallToolResult, McpError> {
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let id = encode_segment(&p.id)?;
    let columns = p.record.keys().cloned().collect::<Vec<_>>().join(", ");
    return self
      .guarded_write(
        "records_update",
        format!(
          "update record '{}' in '{}', columns [{columns}]",
          p.id, p.api
        ),
        "PATCH",
        format!("{RECORDS_BASE}/{api}/{id}"),
        Some(Value::Object(p.record)),
      )
      .await;
  }

  #[tool(description = "Deletes a record by id.")]
  async fn records_delete(
    &self,
    params: Parameters<RecordsDeleteParams>,
  ) -> Result<CallToolResult, McpError> {
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let id = encode_segment(&p.id)?;
    return self
      .guarded_write(
        "records_delete",
        format!("delete record '{}' from '{}'", p.id, p.api),
        "DELETE",
        format!("{RECORDS_BASE}/{api}/{id}"),
        None,
      )
      .await;
  }

  #[tool(
    description = "Executes a record mutation previously parked by records_create/update/delete. Confirming charges the write budget."
  )]
  async fn write_confirm(
    &self,
    params: Parameters<PendingParams>,
  ) -> Result<CallToolResult, McpError> {
    let pending = {
      let mut guards = self.0.guards.lock().expect("poisoned");
      match guards.confirm(&params.0.pending_id) {
        Ok(pending) => pending,
        Err(err) => return error_result(err.to_string()),
      }
    };
    let (status, value) = self
      .call(&pending.method, &pending.path, pending.body.as_ref())
      .await?;
    return self.respond(status, value, false);
  }

  #[tool(
    description = "Discards a parked record mutation without executing it or charging the budget."
  )]
  async fn write_cancel(
    &self,
    params: Parameters<PendingParams>,
  ) -> Result<CallToolResult, McpError> {
    let cancelled = {
      let mut guards = self.0.guards.lock().expect("poisoned");
      guards.cancel(&params.0.pending_id)
    };
    return match cancelled {
      Ok(pending) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
        "Cancelled without executing: {}",
        pending.summary
      ))])),
      Err(err) => error_result(err.to_string()),
    };
  }

  #[tool(
    description = "Reports the embedded instance, access mode, acting user and remaining write budget."
  )]
  async fn auth_status(&self) -> Result<CallToolResult, McpError> {
    let (confirm, remaining) = {
      let guards = self.0.guards.lock().expect("poisoned");
      (guards.confirm_writes(), guards.budget_remaining())
    };
    return json_result(&json!({
      "transport": "in-process (no network listener)",
      "mode": match self.0.mode {
        McpMode::ReadOnly => "read-only",
        McpMode::Records => "records",
      },
      "user": { "email": self.0.user_email, "id": self.0.user_id.to_string() },
      "write_guards": {
        "confirm_writes": confirm,
        "budget_remaining": remaining
          .map(Value::from)
          .unwrap_or_else(|| Value::from("unlimited")),
      },
    }));
  }
}

#[tool_handler]
impl ServerHandler for TrailBaseMcp {
  fn get_info(&self) -> ServerInfo {
    let mut info = ServerInfo::default();
    info.capabilities = ServerCapabilities::builder().enable_tools().build();
    info.instructions = Some(
      "TrailBase record tools running inside the `trail` binary. All access \
       is checked server-side against the acting user's record API ACLs. \
       Mutations may return a pending_id: nothing is written until you call \
       write_confirm with it (write_cancel discards)."
        .to_string(),
    );
    return info;
  }
}
