use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Envelope codec: every RPC travels as {"ok":bool,"result":...,"error":...}
// ---------------------------------------------------------------------------

pub fn ok_envelope<T: Serialize>(v: &T) -> Result<Vec<u8>, String> {
    let result = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    let result: serde_json::Value = serde_json::from_slice(&result).map_err(|e| e.to_string())?;
    serde_json::to_vec(&serde_json::json!({"ok": true, "result": result}))
        .map_err(|e| e.to_string())
}

pub fn error_envelope(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}

/// Decode a host-returned envelope into its result value.
pub fn parse_envelope(raw: &[u8]) -> Result<serde_json::Value, String> {
    let v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if v["ok"].as_bool() != Some(true) {
        return Err("host call returned error envelope".into());
    }
    Ok(v["result"].clone())
}

// ---------------------------------------------------------------------------
// base64 helpers: Go []byte fields travel as base64 strings on both sides.
// ---------------------------------------------------------------------------

/// Case- and underscore-insensitive key lookup for host JSON payloads.
/// Go untagged fields marshal as exported names (`StorageJSON`), while tagged
/// fields use snake_case (`auth_index`); callers should not care which.
pub fn get_field<'a>(req: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    let obj = req.as_object()?;
    if let Some(v) = obj.get(name) {
        return Some(v);
    }
    let norm = |s: &str| -> String {
        s.chars().filter(|c| c.is_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
    };
    let target = norm(name);
    obj.iter().find(|(k, _)| norm(k) == target).map(|(_, v)| v)
}

pub fn b64_encode(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub fn b64_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Registration (schema v3)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Registration {
    #[serde(rename = "schema_version")]
    pub schema_version: u32,
    pub metadata: Metadata,
    pub capabilities: Capabilities,
}

/// Go has no json tags on Metadata: exported field names match exactly.
/// `GitHubRepository` must be renamed verbatim (PascalCase conversion of
/// `github_repository` would produce `GithubRepository` and silently drop it).
#[derive(Serialize)]
pub struct Metadata {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Author")]
    pub author: String,
    #[serde(rename = "GitHubRepository")]
    pub repo: String,
}

#[derive(Serialize)]
pub struct Capabilities {
    pub model_provider: bool,
    pub auth_provider: bool,
    pub executor: bool,
    #[serde(rename = "executor_model_scope")]
    pub executor_model_scope: &'static str,
    #[serde(rename = "executor_input_formats")]
    pub input_formats: Vec<&'static str>,
    #[serde(rename = "executor_output_formats")]
    pub output_formats: Vec<&'static str>,
    #[serde(rename = "management_api")]
    pub management_api: bool,
    pub scheduler: bool,
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct ModelInfo {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Object")]
    pub object: String,
    #[serde(rename = "OwnedBy")]
    pub owned_by: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "SupportedGenerationMethods")]
    pub methods: Vec<String>,
    #[serde(rename = "ContextLength")]
    pub context_length: i64,
    #[serde(rename = "MaxCompletionTokens")]
    pub max_completion_tokens: i64,
    #[serde(rename = "UserDefined")]
    pub user_defined: bool,
}

#[derive(Serialize)]
pub struct ModelResponse {
    #[serde(rename = "Provider")]
    pub provider: String,
    #[serde(rename = "Models")]
    pub models: Vec<ModelInfo>,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct AuthData {
    #[serde(rename = "Provider")]
    pub provider: String,
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "FileName")]
    pub file_name: String,
    #[serde(rename = "Label")]
    pub label: String,
    /// base64 of the StoredAuth JSON, matching Go []byte marshaling.
    #[serde(rename = "StorageJSON")]
    pub storage_json: String,
    #[serde(rename = "Metadata")]
    pub metadata: serde_json::Value,
}

#[derive(Serialize)]
pub struct AuthParseResponse {
    #[serde(rename = "Handled")]
    pub handled: bool,
    #[serde(rename = "Auth")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthData>,
}

#[derive(Serialize)]
pub struct AuthLoginStartResponse {
    #[serde(rename = "Provider")]
    pub provider: String,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "State")]
    pub state: String,
    /// RFC3339; the Go host decodes time.Time from this format.
    #[serde(rename = "ExpiresAt")]
    pub expires_at: String,
}

#[derive(Serialize)]
pub struct AuthLoginPollResponse {
    /// "pending" | "success" | "error"
    #[serde(rename = "Status")]
    pub status: &'static str,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Auth")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthData>,
}

#[derive(Serialize)]
pub struct AuthRefreshResponse {
    #[serde(rename = "Auth")]
    pub auth: AuthData,
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ExecutorExecResponse {
    #[serde(rename = "Payload")]
    pub payload: String,
    #[serde(rename = "Headers")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::HashMap<String, Vec<String>>>,
}

#[derive(Serialize)]
pub struct StreamChunk {
    #[serde(rename = "Payload")]
    pub payload: String,
}

#[derive(Serialize)]
pub struct ExecutorStreamResponse {
    #[serde(rename = "Headers")]
    pub headers: std::collections::HashMap<String, Vec<String>>,
    #[serde(rename = "chunks")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<StreamChunk>,
}

// ---------------------------------------------------------------------------
// Management
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ManagementRoute {
    pub method: &'static str,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Serialize)]
pub struct ResourceRoute {
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub menu: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Serialize)]
pub struct ManagementRegistration {
    #[serde(rename = "routes")]
    pub routes: Vec<ManagementRoute>,
    #[serde(rename = "resources")]
    pub resources: Vec<ResourceRoute>,
}

#[derive(Serialize)]
pub struct MgmtResponse {
    #[serde(rename = "StatusCode")]
    pub status_code: u16,
    #[serde(rename = "Headers")]
    pub headers: std::collections::HashMap<String, Vec<String>>,
    #[serde(rename = "Body")]
    pub body: String,
}

// ---------------------------------------------------------------------------
// Credential storage (on-disk workbuddy.json shape; camelCase tags).
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredAuth {
    pub auth: StoredTokens,
    pub account: StoredAccount,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredTokens {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    pub domain: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredAccount {
    pub uid: String,
    #[serde(rename = "enterpriseId")]
    pub enterprise_id: String,
    pub nickname: String,
}

// ---------------------------------------------------------------------------
// CodeBuddy upstream shapes ({code,msg,data} envelope).
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CodeBuddyEnvelope<T> {
    pub code: i64,
    pub msg: String,
    pub data: Option<T>,
}

#[derive(Deserialize, Clone)]
pub struct TokenData {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresIn")]
    pub expires_in: i64,
    #[serde(default)]
    #[serde(rename = "refreshExpiresIn")]
    pub refresh_expires_in: i64,
    #[serde(default)]
    pub domain: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct AccountData {
    #[serde(default)]
    pub uid: String,
    #[serde(default, rename = "enterpriseId")]
    pub enterprise_id: String,
    #[serde(default)]
    pub nickname: String,
}

/// Per-account credential file name: workbuddy-{last 6 chars of uid}.json.
/// Re-login of the same account lands on the same file (host overwrites by
/// name), so repeated QR scans are idempotent. Empty uid falls back to the
/// legacy shared name.
pub fn credential_file_name(sa: &StoredAuth) -> String {
    let uid = sa.account.uid.trim();
    if uid.is_empty() {
        return "workbuddy.json".into();
    }
    let chars: Vec<char> = uid.chars().collect();
    let tail: String = if chars.len() >= 6 {
        chars[chars.len() - 6..].iter().collect()
    } else {
        uid.to_string()
    };
    format!("workbuddy-{tail}.json")
}

pub fn auth_data_from_stored(sa: &StoredAuth, file_name: &str) -> AuthData {
    AuthData {
        provider: "workbuddy".into(),
        id: "workbuddy".into(),
        file_name: file_name.into(),
        label: "WorkBuddy".into(),
        storage_json: b64_encode(&serde_json::to_vec(sa).unwrap_or_default()),
        metadata: serde_json::json!({"type": "workbuddy"}),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_json_keys() {
        let caps = Capabilities {
            model_provider: true,
            auth_provider: true,
            executor: true,
            executor_model_scope: "both",
            input_formats: vec!["chat-completions"],
            output_formats: vec!["chat-completions"],
            management_api: true,
            scheduler: true,
        };
        let r = Registration {
            schema_version: 3,
            metadata: Metadata {
                name: "workbuddy".into(),
                version: "0.2.0".into(),
                author: "x".into(),
                repo: "https://example.invalid".into(),
            },
            capabilities: caps,
        };
        let env = ok_envelope(&r).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&env).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["schema_version"], 3);
        assert_eq!(v["result"]["capabilities"]["executor_model_scope"], "both");
        assert_eq!(v["result"]["capabilities"]["management_api"], true);
        assert_eq!(v["result"]["capabilities"]["scheduler"], true);
    assert!(v["result"]["capabilities"].get("model_registrar").is_none());
        assert_eq!(v["result"]["metadata"]["GitHubRepository"], "https://example.invalid");
    }

    #[test]
    fn storage_json_roundtrip() {
        let sa = StoredAuth {
            auth: StoredTokens {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 1,
                domain: "d".into(),
            },
            account: StoredAccount {
                uid: "u".into(),
                enterprise_id: "e".into(),
                nickname: "n".into(),
            },
        };
        let ad = auth_data_from_stored(&sa, "workbuddy.json");
        assert_eq!(b64_decode(&ad.storage_json), serde_json::to_vec(&sa).unwrap());
        assert_eq!(ad.file_name, "workbuddy.json");
        let back: StoredAuth = serde_json::from_slice(&b64_decode(&ad.storage_json)).unwrap();
        assert_eq!(back.auth.access_token, "a");
        assert_eq!(back.account.enterprise_id, "e");
    }

    #[test]
    fn credential_file_name_rules() {
        let mut sa = StoredAuth {
            auth: StoredTokens { access_token: "a".into(), refresh_token: "r".into(), expires_at: 1, domain: "d".into() },
            account: StoredAccount { uid: String::new(), enterprise_id: String::new(), nickname: String::new() },
        };
        assert_eq!(credential_file_name(&sa), "workbuddy.json");
        sa.account.uid = "3a417abcdef".into();
        assert_eq!(credential_file_name(&sa), "workbuddy-abcdef.json");
        sa.account.uid = "abc".into();
        assert_eq!(credential_file_name(&sa), "workbuddy-abc.json");
    }

    #[test]
    fn parse_envelope_error() {
        assert!(parse_envelope(b"{\"ok\":false}").is_err());
        let good = ok_envelope(&serde_json::json!({"x": 1})).unwrap();
        assert_eq!(parse_envelope(&good).unwrap()["x"], 1);
    }
}
