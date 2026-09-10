use crate::rpc::{b64_encode, ManagementRegistration, MgmtResponse};
use serde_json::Value;
use std::collections::HashMap;

pub const PANEL_HTML: &str = include_str!("../panel.html");
pub const PROVIDER_NAME: &str = "workbuddy";

pub fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: vec![
            crate::rpc::ManagementRoute {
                method: "GET",
                path: "/plugins/workbuddy/accounts".into(),
                description: "List WorkBuddy accounts with plan, credits and cycle windows.".into(),
            },
            crate::rpc::ManagementRoute {
                method: "POST",
                path: "/plugins/workbuddy/refresh".into(),
                description: "Force refresh credits for all accounts.".into(),
            },
        ],
        resources: vec![crate::rpc::ResourceRoute {
            path: "/panel".into(),
            menu: "WorkBuddy".into(),
            description: "WorkBuddy dashboard: credits, plan, cycle windows.".into(),
        }],
    }
}

fn json_response(status_code: u16, v: serde_json::Value) -> MgmtResponse {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), vec!["application/json; charset=utf-8".into()]);
    MgmtResponse { status_code, headers, body: b64_encode(v.to_string().as_bytes()) }
}

fn html_response(body: &str) -> MgmtResponse {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), vec!["text/html; charset=utf-8".into()]);
    MgmtResponse { status_code: 200, headers, body: b64_encode(body.as_bytes()) }
}

/// Handle one management request. Paths arrive under /v0/management or
/// /v0/resource; authentication is done by the host before dispatch.
pub fn handle(method: &str, path: &str, _query: &str, _body_b64: &str) -> MgmtResponse {
    let path = path.trim_end_matches('/');

    // Browser UI resource routes.
    let res_prefix = format!("/v0/resource/plugins/{PROVIDER_NAME}");
    if method == "GET" && path.starts_with(&res_prefix) {
        let sub = &path[res_prefix.len()..];
        return match sub {
            "" | "/" | "/panel" | "/panel.html" => html_response(PANEL_HTML),
            _ => json_response(404, serde_json::json!({"error": "not found"})),
        };
    }

    let base = format!("/v0/management/plugins/{PROVIDER_NAME}");
    match (method, path) {
        ("GET", p) if p == base || p == format!("{base}/accounts") => {
            json_response(200, build_accounts_dashboard())
        }
        ("POST", p) if p == format!("{base}/refresh") => {
            json_response(200, build_accounts_dashboard())
        }
        _ => json_response(404, serde_json::json!({"error": format!("not found: {path}")})),
    }
}

/// List every workbuddy credential the host knows about and attach a fresh
/// credits snapshot to each.
fn build_accounts_dashboard() -> serde_json::Value {
    let fetched_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let files = match list_auth_files() {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({"error": format!("host.auth.list failed: {e}"), "accounts": [], "fetched_at": fetched_at})
        }
    };

    let mut accounts = Vec::new();
    if let Some(list) = files.as_array() {
        for f in list {
            // HostAuthFileEntry uses snake_case json tags.
            let provider = f.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let ftype = f.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !provider.is_empty() && provider != PROVIDER_NAME && ftype != PROVIDER_NAME {
                continue;
            }
            let auth_index = f
                .get("auth_index")
                .or_else(|| f.get("AuthIndex"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let credits = host_auth_get(&auth_index).map_or_else(
                |e| serde_json::json!({"error": format!("read auth failed: {e}"), "fetched_at": fetched_at}),
                |raw| match serde_json::from_slice::<crate::rpc::StoredAuth>(&raw) {
                    Ok(sa) => {
                        let mut c = crate::billing::fetch_credits(&sa);
                        c["fetched_at"] = serde_json::json!(fetched_at);
                        c
                    }
                    Err(_) => serde_json::json!({"error": "parse auth failed", "fetched_at": fetched_at}),
                },
            );

            let nickname = credits
                .pointer("/packages/0/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            accounts.push(serde_json::json!({
                "auth_index": auth_index,
                "name": name,
                "nickname": "",
                "credits": credits,
            }));
            let _ = nickname;
        }
    }
    serde_json::json!({"accounts": accounts, "fetched_at": fetched_at})
}

fn host_auth_get(auth_index: &str) -> Result<Vec<u8>, String> {
    let body = serde_json::json!({"auth_index": auth_index});
    let raw = crate::cabi::host_call("host.auth.get", body.to_string().as_bytes())?;
    let result = crate::rpc::parse_envelope(&raw)?;
    // Host field is json.RawMessage: marshals as RAW JSON, not base64.
    let json_value = result
        .get("JSON")
        .or_else(|| result.get("json"))
        .cloned()
        .ok_or_else(|| "host.auth.get: missing json".to_string())?;
    Ok(serde_json::to_vec(&json_value).map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn registration_routes_shape() {
        let r = registration();
        assert_eq!(r.routes[0].path, "/plugins/workbuddy/accounts");
        assert_eq!(r.routes[0].method, "GET");
        assert_eq!(r.routes[1].method, "POST");
        assert_eq!(r.routes[1].path, "/plugins/workbuddy/refresh");
        assert_eq!(r.resources[0].menu, "WorkBuddy");
        assert_eq!(r.resources[0].path, "/panel");
    }

    #[test]
    fn panel_served_and_404() {
        let ok = handle("GET", "/v0/resource/plugins/workbuddy/panel", "", "");
        assert_eq!(ok.status_code, 200);
        let body = String::from_utf8(b64_decode(&ok.body)).unwrap();
        assert!(body.contains("总积分额度"), "panel must contain v2 total card");
        let ok2 = handle("GET", "/v0/resource/plugins/workbuddy", "", "");
        assert_eq!(ok2.status_code, 200);
        let bad = handle("GET", "/v0/resource/plugins/workbuddy/nope", "", "");
        assert_eq!(bad.status_code, 404);
    }

    #[test]
    fn mgmt_unknown_path_404() {
        assert_eq!(handle("GET", "/v0/management/plugins/workbuddy/zzz", "", "").status_code, 404);
    }
}

#[allow(dead_code)]
fn list_auth_files() -> Result<serde_json::Value, String> {
    let raw = crate::cabi::host_call("host.auth.list", b"")?;
    let result = crate::rpc::parse_envelope(&raw)?;
    // The host may double-encode the result (string containing JSON) when the
    // response struct is marshaled as an opaque value; unwrap that layer.
    // The host can double-encode: result itself is a JSON string, or
    // result.files is a JSON string. Unwrap whichever layer appears.
    let unwrap_str = |v: &serde_json::Value| -> serde_json::Value {
        match v {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        }
    };
    let files_value = if result["files"].is_null() {
        unwrap_str(&result)
    } else if result["files"].is_string() {
        unwrap_str(&result["files"])
    } else {
        result["files"].clone()
    };
    Ok(files_value)
}
