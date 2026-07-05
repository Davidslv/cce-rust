//! # mcp::protocol — JSON-RPC 2.0 framing for the MCP stdio transport
//!
//! **Why this file exists:** SPEC-MCP requires MCP over stdio, which is
//! newline-delimited JSON-RPC 2.0 on stdin/stdout. Rather than pull in an unvetted
//! MCP SDK, CCE hand-rolls the tiny slice of JSON-RPC it needs with `serde_json`
//! (already a dependency) — the same choice the rest of the engine makes for its
//! hand-rolled HTTP/YAML writers, keeping every wire byte under our control and the
//! dependency set pinned and minimal.
//!
//! **What it is / does:** Parses a single request line into a `Request` (method +
//! optional id + params), distinguishes requests from notifications (no `id`), and
//! renders success/error responses as compact JSON strings.
//!
//! **Responsibilities:**
//! - Own request parsing and the success/error response encoders.
//! - Own the JSON-RPC error-code constants used by the dispatcher.
//! - It does NOT dispatch methods or run tools — that is `server`/`tools`.

use serde_json::{json, Value};

/// The JSON-RPC protocol tag echoed on every message.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC: invalid JSON was received.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC: the method does not exist.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: invalid method parameters.
pub const INVALID_PARAMS: i64 = -32602;

/// A parsed JSON-RPC request or notification.
#[derive(Debug, Clone)]
pub struct Request {
    /// The request id (any JSON scalar). `None` marks a *notification*, which by
    /// spec must receive no response.
    pub id: Option<Value>,
    /// The method name (e.g. `initialize`, `tools/call`).
    pub method: String,
    /// The method params object (`Value::Null` when omitted).
    pub params: Value,
}

impl Request {
    /// A notification carries no `id` and must not be answered.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Parse one line of input into a `Request`. Returns `Err` for non-JSON or a JSON
/// value that is not an object with a string `method` (a malformed request).
pub fn parse_request(line: &str) -> Result<Request, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let method = v
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| "request is missing a string `method`".to_string())?
        .to_string();
    // A present-but-null id is treated as absent (notification), per JSON-RPC.
    let id = v.get("id").cloned().filter(|i| !i.is_null());
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    Ok(Request { id, method, params })
}

/// Encode a successful response carrying `result` for request `id`.
pub fn success(id: &Value, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    }))
    .unwrap_or_default()
}

/// Encode an error response with `code`/`message` for request `id` (use
/// `Value::Null` for the id when the request could not be parsed).
pub fn error(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request_with_id_and_params() {
        let r = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#).unwrap();
        assert_eq!(r.method, "ping");
        assert_eq!(r.id, Some(json!(1)));
        assert!(!r.is_notification());
    }

    #[test]
    fn a_missing_or_null_id_is_a_notification() {
        let r = parse_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(r.is_notification());
        let r2 = parse_request(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
        assert!(r2.is_notification());
    }

    #[test]
    fn params_default_to_null_when_absent() {
        let r = parse_request(r#"{"id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(r.params, Value::Null);
    }

    #[test]
    fn rejects_non_json_and_methodless() {
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"id":1}"#).is_err());
    }

    #[test]
    fn success_and_error_encode_the_envelope() {
        let s = success(&json!(1), json!({"ok": true}));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["ok"], true);

        let e = error(json!(2), METHOD_NOT_FOUND, "nope");
        let v: Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "nope");
    }
}
