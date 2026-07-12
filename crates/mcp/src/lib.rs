//! MCP server embedded into TrailBase.
//!
//! Tool calls are translated into **in-process** requests against
//! TrailBase's real axum router: the exact same auth middleware, record API
//! handlers and ACL checks run as for network traffic. Two transports share
//! the same tool set:
//!
//! - `trail mcp` — stdio; tool calls act as one dedicated user whose
//!   credentials are checked with the regular login flow at startup.
//! - `trail run --mcp` — a Streamable HTTP endpoint at `/mcp` on the running
//!   server; every tool call forwards the **caller's** `Authorization`
//!   header, so each MCP client works under its own TrailBase account and
//!   ACLs.
//!
//! Record mutations are additionally protected by write guards (budget,
//! two-phase confirmation, column redaction) ported from
//! `examples/mcp-server`.
#![forbid(unsafe_code, clippy::unwrap_used)]
#![allow(clippy::needless_return)]
#![warn(clippy::await_holding_lock, clippy::inefficient_to_string)]

mod guards;
mod sandbox;

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rmcp::{
  ErrorData as McpError, RoleServer, ServerHandler, ServiceExt as _,
  handler::server::wrapper::Parameters,
  model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
  schemars,
  service::RequestContext,
  tool, tool_handler, tool_router,
  transport::stdio,
  transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
  },
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

/// Where tool calls take their TrailBase credentials from.
enum AuthSource {
  /// stdio: full `Authorization` header value minted by the startup login.
  Fixed(String),
  /// Streamable HTTP: forwarded per request from the caller's header.
  PerRequest,
}

struct ActingUser {
  email: String,
  id: uuid::Uuid,
}

/// Shared guard/tool settings for both transports.
#[derive(Clone, Debug)]
pub struct McpSettings {
  pub mode: McpMode,
  /// Mutations allowed per session; 0 = unlimited.
  pub budget_writes: u32,
  /// Two-phase (propose/confirm) record mutations.
  pub confirm_writes: bool,
  pub confirm_timeout_secs: u64,
  /// Case-insensitive regexes; matching column names are masked in reads.
  pub redact_columns: Vec<String>,
}

pub struct McpOptions {
  pub data_dir: DataDir,
  pub public_url: Option<url::Url>,
  /// Email of the user tool calls act as; record API ACLs apply server-side.
  pub user: String,
  pub password: String,
  pub settings: McpSettings,
  /// Enable ephemeral snapshot-sandbox tools (stdio transport only).
  pub sandbox: bool,
}

/// Boots the instance from the given depot and serves MCP over stdio until
/// the client disconnects. Never binds a network listener.
pub async fn run_stdio(options: McpOptions) -> Result<(), BoxError> {
  let redact_patterns = compile_redact_patterns(&options.settings.redact_columns)?;

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

  let router = Arc::new(OnceLock::new());
  let _ = router.set(server.main_router.1.clone());

  let source_dir = state.data_dir().root().clone();
  let handler = TrailBaseMcp(Arc::new(Inner {
    router,
    state,
    auth: AuthSource::Fixed(format!("Bearer {}", tokens.auth_token)),
    acting_user: Some(ActingUser {
      email: options.user.clone(),
      id: tokens.id,
    }),
    mode: options.settings.mode,
    guards: Mutex::new(Guards::new(
      options.settings.budget_writes,
      options.settings.mode == McpMode::Records && options.settings.confirm_writes,
      options.settings.confirm_timeout_secs,
    )),
    redact_patterns,
    sandbox_enabled: options.sandbox,
    sandbox: tokio::sync::Mutex::new(sandbox::SandboxManager::new(source_dir)),
  }));

  log::info!(
    "trail mcp: stdio server ready (mode={:?}, acting user={})",
    options.settings.mode,
    options.user
  );

  let service = handler.serve(stdio()).await?;
  service.waiting().await?;
  return Ok(());
}

/// Builds the Streamable HTTP tower service for mounting at `/mcp` on the
/// running server (`trail run --mcp`). The `router` slot is filled by the
/// caller once `Server::init` has produced the final router — the service
/// only dereferences it per tool call.
///
/// Each MCP session gets its own handler instance (and thus its own write
/// budget and pending-confirmation queue); credentials are taken from each
/// request's `Authorization` header, so callers act as themselves.
pub fn http_service(
  state: AppState,
  router: Arc<OnceLock<axum::Router>>,
  settings: &McpSettings,
  allowed_hosts: &[String],
) -> Result<StreamableHttpService<TrailBaseMcp, LocalSessionManager>, BoxError> {
  let redact_patterns = compile_redact_patterns(&settings.redact_columns)?;
  let settings = settings.clone();

  let mut config = StreamableHttpServerConfig::default();
  if !allowed_hosts.is_empty() {
    config.allowed_hosts = allowed_hosts.to_vec();
  }

  let factory = move || {
    let source_dir = state.data_dir().root().clone();
    return Ok(TrailBaseMcp(Arc::new(Inner {
      router: router.clone(),
      state: state.clone(),
      auth: AuthSource::PerRequest,
      acting_user: None,
      mode: settings.mode,
      guards: Mutex::new(Guards::new(
        settings.budget_writes,
        settings.mode == McpMode::Records && settings.confirm_writes,
        settings.confirm_timeout_secs,
      )),
      redact_patterns: redact_patterns.clone(),
      // Spawning sandbox children from network-triggered sessions is not
      // supported; use the stdio transport for sandbox work.
      sandbox_enabled: false,
      sandbox: tokio::sync::Mutex::new(sandbox::SandboxManager::new(source_dir)),
    })));
  };

  return Ok(StreamableHttpService::new(
    factory,
    Arc::new(LocalSessionManager::default()),
    config,
  ));
}

fn compile_redact_patterns(patterns: &[String]) -> Result<Vec<regex::Regex>, BoxError> {
  return patterns
    .iter()
    .filter(|p| !p.trim().is_empty())
    .map(|p| {
      regex::Regex::new(&format!("(?i){}", p.trim()))
        .map_err(|err| format!("Invalid redact-columns pattern '{p}': {err}").into())
    })
    .collect();
}

#[derive(Clone)]
pub struct TrailBaseMcp(Arc<Inner>);

struct Inner {
  /// The final router from `Server::init`; a slot because in HTTP mode the
  /// MCP service itself is part of that router (set right after init).
  router: Arc<OnceLock<axum::Router>>,
  state: AppState,
  auth: AuthSource,
  acting_user: Option<ActingUser>,
  mode: McpMode,
  guards: Mutex<Guards>,
  redact_patterns: Vec<regex::Regex>,
  sandbox_enabled: bool,
  sandbox: tokio::sync::Mutex<sandbox::SandboxManager>,
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SandboxQueryParams {
  /// SQL statement to execute on the sandbox instance.
  pub query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SandboxDdlParams {
  /// One of: create_table, alter_table, drop_table, create_index, drop_index.
  pub action: String,
  /// Request body for the corresponding sandbox admin endpoint.
  pub payload: serde_json::Map<String, Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SandboxDestroyParams {
  /// Keep the sandbox directory on disk instead of deleting it.
  pub keep_dir: Option<bool>,
}

#[tool_router]
impl TrailBaseMcp {
  /// Resolves the `Authorization` header value for this call: the startup
  /// login's token (stdio) or the caller's own header (HTTP). The error arm
  /// is a ready-to-return tool result instructing the client to
  /// authenticate.
  fn auth_header(&self, ctx: &RequestContext<RoleServer>) -> Result<String, CallToolResult> {
    match &self.0.auth {
      AuthSource::Fixed(header_value) => Ok(header_value.clone()),
      AuthSource::PerRequest => ctx
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.headers.get(header::AUTHORIZATION))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| {
          CallToolResult::error(vec![ContentBlock::text(
            "Not authenticated: connect with an 'Authorization: Bearer <TrailBase auth token>' \
             header (obtain one via /api/auth/v1/login).",
          )])
        }),
    }
  }

  /// Sends one request through the in-process router; auth middleware and
  /// ACLs run exactly as over the network.
  async fn call(
    &self,
    auth_header: &str,
    method: &str,
    path_and_query: &str,
    body: Option<&Value>,
  ) -> Result<(StatusCode, Value), McpError> {
    let router = self
      .0
      .router
      .get()
      .ok_or_else(|| internal("router not initialized yet"))?
      .clone();

    let mut builder = Request::builder()
      .method(method)
      .uri(path_and_query)
      .header(header::AUTHORIZATION, auth_header);
    if body.is_some() {
      builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = match body {
      Some(value) => builder.body(Body::from(serde_json::to_vec(value).map_err(internal)?)),
      None => builder.body(Body::empty()),
    }
    .map_err(internal)?;

    let response = router.oneshot(request).await.map_err(internal)?;
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
    auth_header: &str,
    tool: &str,
    summary: String,
    method: &str,
    path: String,
    body: Option<Value>,
  ) -> Result<CallToolResult, McpError> {
    if self.0.mode != McpMode::Records {
      return error_result(
        "Mutations are disabled in read-only mode; the server must be restarted with mode 'records'.",
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
        let (status, value) = self.call(auth_header, method, &path, body.as_ref()).await?;
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

  #[tool(
    description = "Lists tables and views of the instance (schema introspection): kind, name, hidden flag and the CREATE statement. Read-only server-side metadata; independent of record API ACLs."
  )]
  async fn schema_tables(&self) -> Result<CallToolResult, McpError> {
    #[derive(Deserialize)]
    struct SchemaRow {
      r#type: String,
      name: String,
      sql: Option<String>,
    }

    let conn = self
      .0
      .state
      .connection_manager()
      .main_entry()
      .connection
      .clone();
    let rows: Vec<SchemaRow> = conn
      .read_query_values::<SchemaRow>(
        r#"SELECT type, name, sql FROM main.sqlite_schema
           WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
           ORDER BY type, name"#,
        (),
      )
      .await
      .map_err(internal)?;

    let objects: Vec<Value> = rows
      .into_iter()
      .map(|row| {
        json!({
          "kind": row.r#type,
          "name": row.name,
          // TrailBase convention: leading underscore marks internal tables.
          "hidden": row.name.starts_with('_'),
          "sql": row.sql,
        })
      })
      .collect();
    return json_result(&json!({ "objects": objects }));
  }

  #[tool(
    description = "Reports TrailBase version, data directory and the number of configured record APIs."
  )]
  async fn instance_info(&self) -> Result<CallToolResult, McpError> {
    let version = self.0.state.version();
    let record_apis = self.0.state.access_config(|c| c.record_apis.len());
    return json_result(&json!({
      "version": version.git_version_tag,
      "commit_date": version.git_commit_date.map(|d| d.trim().to_string()),
      "data_dir": self.0.state.data_dir().root(),
      "record_apis": record_apis,
    }));
  }

  #[tool(description = "Fetches the JSON schema describing records of the given record API.")]
  async fn records_schema(
    &self,
    params: Parameters<RecordsSchemaParams>,
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
    let api = encode_segment(&params.0.api)?;
    let (status, value) = self
      .call(&auth, "GET", &format!("{RECORDS_BASE}/{api}/schema"), None)
      .await?;
    return self.respond(status, value, false);
  }

  #[tool(
    description = "Lists records of a TrailBase record API with optional filters, ordering and cursor/offset pagination. Access is enforced server-side by the API's ACLs for the calling user."
  )]
  async fn records_list(
    &self,
    params: Parameters<RecordsListParams>,
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
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
    let (status, value) = self.call(&auth, "GET", &path, None).await?;
    return self.respond(status, value, true);
  }

  #[tool(description = "Reads a single record by id from a TrailBase record API.")]
  async fn records_read(
    &self,
    params: Parameters<RecordsReadParams>,
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
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
    let (status, value) = self.call(&auth, "GET", &path, None).await?;
    return self.respond(status, value, true);
  }

  #[tool(
    description = "Creates a record via a TrailBase record API. Write access is enforced server-side; in records mode the mutation may require write_confirm."
  )]
  async fn records_create(
    &self,
    params: Parameters<RecordsCreateParams>,
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let columns = p.record.keys().cloned().collect::<Vec<_>>().join(", ");
    return self
      .guarded_write(
        &auth,
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
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let id = encode_segment(&p.id)?;
    let columns = p.record.keys().cloned().collect::<Vec<_>>().join(", ");
    return self
      .guarded_write(
        &auth,
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
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
    let p = params.0;
    let api = encode_segment(&p.api)?;
    let id = encode_segment(&p.id)?;
    return self
      .guarded_write(
        &auth,
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
    ctx: RequestContext<RoleServer>,
  ) -> Result<CallToolResult, McpError> {
    let auth = match self.auth_header(&ctx) {
      Ok(auth) => auth,
      Err(result) => return Ok(result),
    };
    let pending = {
      let mut guards = self.0.guards.lock().expect("poisoned");
      match guards.confirm(&params.0.pending_id) {
        Ok(pending) => pending,
        Err(err) => return error_result(err.to_string()),
      }
    };
    let (status, value) = self
      .call(&auth, &pending.method, &pending.path, pending.body.as_ref())
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

  /// Ready-to-return error when sandbox tools are disabled.
  fn sandbox_disabled(&self) -> Option<CallToolResult> {
    if self.0.sandbox_enabled {
      return None;
    }
    return Some(CallToolResult::error(vec![ContentBlock::text(
      "Sandbox tools are disabled; start `trail mcp` with --sandbox (stdio transport only).",
    )]));
  }

  #[tool(
    description = "Creates an ephemeral sandbox: a consistent snapshot of the live depot (data + config + migrations, fresh keys) served by a child trail process on localhost. All sandbox_* tools target it; production is never touched."
  )]
  async fn sandbox_create(&self) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let mut sandbox = self.0.sandbox.lock().await;
    return match sandbox.create().await {
      Ok(manifest) => json_result(&manifest),
      Err(e) => error_result(e.to_string()),
    };
  }

  #[tool(description = "Reports whether a sandbox is running and healthy.")]
  async fn sandbox_status(&self) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let mut sandbox = self.0.sandbox.lock().await;
    let status = sandbox.status().await;
    return json_result(&status);
  }

  #[tool(
    description = "Executes arbitrary SQL on the SANDBOX instance (admin endpoint of the ephemeral child; runs on its writer connection). Prefer sandbox_ddl for schema changes so they are recorded as migration files."
  )]
  async fn sandbox_query(
    &self,
    params: Parameters<SandboxQueryParams>,
  ) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let sandbox = self.0.sandbox.lock().await;
    let result = sandbox
      .admin_call(
        reqwest::Method::POST,
        "/query",
        Some(&json!({ "query": params.0.query })),
      )
      .await;
    drop(sandbox);
    return match result {
      Ok((status, value)) => self.respond(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        value,
        false,
      ),
      Err(e) => error_result(e.to_string()),
    };
  }

  #[tool(
    description = "Applies DDL to the SANDBOX through its admin API so the change is recorded as a migration file. Actions map to the admin endpoints: create_table/alter_table/drop_table (POST/PATCH/DELETE /table) and create_index/drop_index (POST/DELETE /index). `payload` is the endpoint's request body, e.g. {\"schema\": <Table>, \"dry_run\": false} for create_table — inspect schema_tables/admin shapes first."
  )]
  async fn sandbox_ddl(
    &self,
    params: Parameters<SandboxDdlParams>,
  ) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let p = params.0;
    let (method, path) = match p.action.as_str() {
      "create_table" => (reqwest::Method::POST, "/table"),
      "alter_table" => (reqwest::Method::PATCH, "/table"),
      "drop_table" => (reqwest::Method::DELETE, "/table"),
      "create_index" => (reqwest::Method::POST, "/index"),
      "drop_index" => (reqwest::Method::DELETE, "/index"),
      other => {
        return error_result(format!(
          "Unknown action '{other}'; expected create_table|alter_table|drop_table|create_index|drop_index."
        ));
      }
    };
    let sandbox = self.0.sandbox.lock().await;
    let result = sandbox
      .admin_call(method, path, Some(&Value::Object(p.payload)))
      .await;
    drop(sandbox);
    return match result {
      Ok((status, value)) => self.respond(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        value,
        false,
      ),
      Err(e) => error_result(e.to_string()),
    };
  }

  #[tool(
    description = "Shows what changed in the sandbox since creation: newly recorded migration files (the reviewable artifact to apply to production through your normal deploy) and config changes. Never applies anything to production."
  )]
  async fn sandbox_diff(&self) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let sandbox = self.0.sandbox.lock().await;
    return match sandbox.diff() {
      Ok(diff) => json_result(&diff),
      Err(e) => error_result(e.to_string()),
    };
  }

  #[tool(
    description = "Stops the sandbox instance and removes its directory (pass keep_dir=true to keep the files on disk for inspection)."
  )]
  async fn sandbox_destroy(
    &self,
    params: Parameters<SandboxDestroyParams>,
  ) -> Result<CallToolResult, McpError> {
    if let Some(disabled) = self.sandbox_disabled() {
      return Ok(disabled);
    }
    let mut sandbox = self.0.sandbox.lock().await;
    return match sandbox.destroy(params.0.keep_dir.unwrap_or(false)).await {
      Ok(result) => json_result(&result),
      Err(e) => error_result(e.to_string()),
    };
  }

  #[tool(
    description = "Reports transport, access mode, acting user and remaining write budget of this MCP session."
  )]
  async fn auth_status(&self) -> Result<CallToolResult, McpError> {
    let (confirm, remaining) = {
      let guards = self.0.guards.lock().expect("poisoned");
      (guards.confirm_writes(), guards.budget_remaining())
    };
    let sandbox_active = self.0.sandbox_enabled && self.0.sandbox.lock().await.is_active();
    let user = match (&self.0.auth, &self.0.acting_user) {
      (AuthSource::Fixed(_), Some(user)) => {
        json!({ "email": user.email, "id": user.id.to_string() })
      }
      _ => Value::String("per-request (from the caller's Authorization header)".to_string()),
    };
    return json_result(&json!({
      "transport": match self.0.auth {
        AuthSource::Fixed(_) => "stdio (in-process router, no network listener)",
        AuthSource::PerRequest => "streamable-http /mcp (in-process router)",
      },
      "mode": match self.0.mode {
        McpMode::ReadOnly => "read-only",
        McpMode::Records => "records",
      },
      "user": user,
      "write_guards": {
        "confirm_writes": confirm,
        "budget_remaining": remaining
          .map(Value::from)
          .unwrap_or_else(|| Value::from("unlimited")),
      },
      "sandbox": {
        "enabled": self.0.sandbox_enabled,
        "active": sandbox_active,
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
       is checked server-side against the calling user's record API ACLs. \
       Mutations may return a pending_id: nothing is written until you call \
       write_confirm with it (write_cancel discards)."
        .to_string(),
    );
    return info;
  }
}
