//! JSON-RPC protocol DTOs for the semantic daemon (v1).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const JSONRPC_VERSION: &str = "2.0";
pub const DAEMON_API_VERSION: u32 = 1;

pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;
pub const ERR_INCOMPATIBLE_API_VERSION: i64 = -32010;
pub const ERR_DAEMON_UNAVAILABLE: i64 = -32020;
pub const ERR_VAULT_NOT_READY: i64 = -32030;
pub const ERR_BOOTSTRAP_REQUIRED: i64 = -32040;

fn default_params() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default = "default_params")]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn error_with_data(
        id: Option<Value>,
        code: i64,
        message: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: Some(data),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct HealthParams {
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_version: Option<String>,
    #[serde(default)]
    pub min_api_version: Option<u32>,
    #[serde(default)]
    pub max_api_version: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct HealthResult {
    pub daemon_version: String,
    pub daemon_api_version: u32,
    #[serde(default)]
    pub pid: u32,
    pub status: String,
    pub uptime_ms: u64,
    pub model_name: String,
    pub semantic_home: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ShutdownParams {}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ShutdownResult {
    pub accepted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EnsureVaultParams {
    pub vault_root: String,
    #[serde(default)]
    pub watch: Option<bool>,
    #[serde(default)]
    pub model_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SemanticPhase {
    Warming,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SemanticStatus {
    pub phase: SemanticPhase,
    pub ready: bool,
    pub indexed_notes: usize,
    pub total_notes: usize,
    pub pending_notes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EnsureVaultResult {
    pub vault_id: String,
    pub ready: bool,
    pub watch_enabled: bool,
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<SemanticPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_notes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_notes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_notes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchSemanticParams {
    pub vault_root: String,
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub include_content: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchHybridParams {
    pub vault_root: String,
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub prefetch: Option<usize>,
    #[serde(default)]
    pub alpha: Option<f32>,
    #[serde(default)]
    pub include_content: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct OpenHintParams {
    pub vault_root: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct OpenHintResult {
    pub path: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchResult {
    pub results: Vec<SemanticHit>,
}

/// A note's best-matching passage, carried alongside a semantic hit.
///
/// Mirrors the `best_chunk` the in-process path returns, so a daemon-served
/// query and a locally-served one describe a result the same way.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct SemanticChunk {
    /// 0-based index of the passage within the note.
    pub index: usize,
    /// Heading trail, outermost first. Empty means the passage sits above the
    /// note's first heading.
    pub heading_path: Vec<String>,
    /// The passage text, as embedded.
    pub passage: String,
    /// This chunk's raw cosine similarity — not the note's ranking `score`,
    /// which may have come from the summary arm instead.
    pub score: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SemanticHit {
    pub path: String,
    pub title: String,
    pub score: f32,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    /// Which representation produced `score`: `"chunk"`, `"summary"` or
    /// `"note"`. Absent when the rank is not attributable to one — the hybrid
    /// path blends two, and an older daemon does not report it at all.
    ///
    /// These three fields are additive and default to `None`, so a new client
    /// talking to an old daemon simply sees no provenance, exactly as before.
    /// `SemanticHit` does not deny unknown fields, so the reverse also holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_type: Option<String>,
    /// The note's closest passage, supplied whenever the note has chunks —
    /// including when `match_type` is `"summary"`, where it is evidence rather
    /// than cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_chunk: Option<SemanticChunk>,
    /// The weighted summary-arm score, for comparison against
    /// `best_chunk.score`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_score: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that predates provenance omits the three fields entirely. That
    /// has to keep deserializing, or upgrading the client would break every
    /// query against an older daemon rather than merely losing provenance.
    #[test]
    fn semantic_hit_from_a_daemon_without_provenance_still_parses() {
        let hit: SemanticHit =
            serde_json::from_str(r#"{"path":"a.md","title":"A","score":0.5,"tags":[]}"#)
                .expect("a pre-provenance hit must still deserialize");

        assert_eq!(hit.match_type, None);
        assert_eq!(hit.best_chunk, None);
        assert_eq!(hit.summary_score, None);
    }

    /// And the fields must survive a full round trip when they are present,
    /// since the whole point is that a daemon-served result now describes
    /// itself the same way an in-process one does.
    #[test]
    fn semantic_hit_round_trips_provenance() {
        let hit = SemanticHit {
            path: "a.md".into(),
            title: "A".into(),
            score: 0.9,
            tags: vec![],
            snippet: None,
            content: None,
            subpath: None,
            match_type: Some("summary".into()),
            best_chunk: Some(SemanticChunk {
                index: 3,
                heading_path: vec!["Design".into(), "Retry policy".into()],
                passage: "after testing we settled on five attempts".into(),
                score: 0.62,
            }),
            summary_score: Some(0.9),
        };

        let wire = serde_json::to_string(&hit).expect("serialize");
        let back: SemanticHit = serde_json::from_str(&wire).expect("deserialize");

        assert_eq!(back.match_type.as_deref(), Some("summary"));
        assert_eq!(back.summary_score, Some(0.9));
        let chunk = back.best_chunk.expect("best_chunk survives the round trip");
        assert_eq!(chunk.index, 3);
        assert_eq!(chunk.heading_path, ["Design", "Retry policy"]);
    }

    /// Omitted rather than serialized as null, so an older client that does not
    /// know these fields sees exactly the payload it always saw.
    #[test]
    fn absent_provenance_is_omitted_from_the_wire() {
        let hit = SemanticHit {
            path: "a.md".into(),
            title: "A".into(),
            score: 0.9,
            tags: vec![],
            snippet: None,
            content: None,
            subpath: None,
            match_type: None,
            best_chunk: None,
            summary_score: None,
        };

        let wire = serde_json::to_string(&hit).expect("serialize");

        assert!(!wire.contains("match_type"), "wire: {wire}");
        assert!(!wire.contains("best_chunk"), "wire: {wire}");
        assert!(!wire.contains("summary_score"), "wire: {wire}");
    }

    #[test]
    fn request_defaults_params_to_object() {
        let req: RpcRequest = serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"health"}"#)
            .expect("request should deserialize");
        assert!(req.params.is_object());
    }

    #[test]
    fn response_error_has_expected_shape() {
        let response = RpcResponse::error(Some(Value::from(1)), ERR_INVALID_PARAMS, "bad params");
        let value = serde_json::to_value(response).expect("response should serialize");
        assert_eq!(value["error"]["code"], Value::from(ERR_INVALID_PARAMS));
    }

    #[test]
    fn ensure_vault_decodes_legacy_v1_without_progress_fields() {
        let result: EnsureVaultResult = serde_json::from_value(serde_json::json!({
            "vault_id": "legacy",
            "ready": true,
            "watch_enabled": true,
            "model_name": "legacy-model"
        }))
        .unwrap();

        assert!(result.ready);
        assert_eq!(result.phase, None);
        assert_eq!(result.indexed_notes, None);
        assert_eq!(result.total_notes, None);
        assert_eq!(result.pending_notes, None);
        assert_eq!(result.last_error, None);
    }

    #[test]
    fn ensure_vault_round_trips_additive_progress_fields() {
        let expected = EnsureVaultResult {
            vault_id: "current".into(),
            ready: false,
            watch_enabled: true,
            model_name: "current-model".into(),
            phase: Some(SemanticPhase::Warming),
            indexed_notes: Some(12),
            total_notes: Some(20),
            pending_notes: Some(8),
            last_error: None,
        };

        let encoded = serde_json::to_value(&expected).unwrap();
        let decoded: EnsureVaultResult = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.ready);
        assert_eq!(decoded.phase, Some(SemanticPhase::Warming));
        assert_eq!(decoded.indexed_notes, Some(12));
        assert_eq!(decoded.total_notes, Some(20));
        assert_eq!(decoded.pending_notes, Some(8));
    }
}
