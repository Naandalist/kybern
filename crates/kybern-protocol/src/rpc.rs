//! JSON-RPC 2.0 envelope types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request id. Clients may use numbers or strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for RpcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcId::Number(n) => write!(f, "{n}"),
            RpcId::String(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Any frame a client may send.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ClientFrame {
    Request(RpcRequest),
    Notification(RpcNotification),
}

/// Any frame the daemon may send.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ServerFrame {
    Response(RpcResponse),
    Notification(RpcNotification),
}

/// The literal string "2.0".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "2.0" { Ok(JsonRpcVersion) } else { Err(serde::de::Error::custom(format!("unsupported jsonrpc version {v}"))) }
    }
}

/// Error codes. Standard JSON-RPC codes plus kybern's application range.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    pub const UNAUTHORIZED: i32 = -32001;
    pub const FORBIDDEN: i32 = -32002;
    pub const NOT_FOUND: i32 = -32003;
    pub const CONFLICT: i32 = -32004;
    pub const PROVIDER_UNAVAILABLE: i32 = -32010;
    pub const PROVIDER_ERROR: i32 = -32011;
    pub const THREAD_BUSY: i32 = -32012;
    /// The method exists in the protocol but this build does not implement it yet.
    pub const NOT_AVAILABLE: i32 = -32013;
}

/// What went wrong in terms a person can act on, sent as an RPC error's `data.problem` (and in
/// results that report per-item failures). Additive: `message` stays the readable fallback, and
/// clients that don't know a `kind` show `message`. Clients prefer their own copy keyed by
/// `kind` and `subject`; `title` and `hint` are the daemon's fallback copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Problem {
    pub kind: ProblemKind,
    /// What the problem is about, e.g. `gh` for the GitHub CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The one action that fixes it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ProblemAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<ProblemAction>,
    /// The full technical error, for a Details disclosure; never the headline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Trying the same call again can succeed once the cause is fixed.
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// A program Kybern needs isn't installed or can't be found.
    ToolMissing,
    /// Nobody is signed in where the call needs an account.
    SignedOut,
    /// The account is signed in but not allowed to do this.
    Permission,
    /// A machine or service can't be reached.
    Offline,
    Conflict,
    InvalidInput,
    NotFound,
    UpdateNeeded,
    Internal,
}

/// A button a client can offer. `id` is what the client does (`install_tool`, `sign_in`,
/// `retry`, ...); `params` carry what it needs, e.g. `{ "tool": "gh" }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProblemAction {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
    /// Attach `problem` as `data.problem`, keeping any other `data` fields.
    pub fn with_problem(mut self, problem: &Problem) -> Self {
        let mut data = match self.data.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        data.insert("problem".into(), serde_json::to_value(problem).unwrap_or(Value::Null));
        self.data = Some(Value::Object(data));
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(codes::METHOD_NOT_FOUND, format!("method not found: {method}"))
    }
    pub fn invalid_params(msg: impl std::fmt::Display) -> Self {
        Self::new(codes::INVALID_PARAMS, format!("invalid params: {msg}"))
    }
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::new(codes::INTERNAL_ERROR, msg.to_string())
    }
    pub fn unauthorized() -> Self {
        Self::new(codes::UNAUTHORIZED, "unauthorized")
    }
    pub fn forbidden(scope: &str) -> Self {
        Self::new(codes::FORBIDDEN, format!("missing scope: {scope}"))
    }
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(codes::NOT_FOUND, format!("not found: {what}"))
    }
}

impl RpcResponse {
    pub fn ok(id: RpcId, result: Value) -> Self {
        Self { jsonrpc: JsonRpcVersion, id, result: Some(result), error: None }
    }
    pub fn err(id: RpcId, error: RpcError) -> Self {
        Self { jsonrpc: JsonRpcVersion, id, result: None, error: Some(error) }
    }
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: JsonRpcVersion, method: method.into(), params }
    }
}

impl RpcRequest {
    pub fn new(id: RpcId, method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: JsonRpcVersion, id, method: method.into(), params }
    }
}
