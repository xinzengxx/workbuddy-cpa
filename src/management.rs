use crate::rpc::{b64_decode, b64_encode, ManagementRegistration, MgmtResponse};
#[cfg(test)]
use base64::Engine as _;
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
            crate::rpc::ManagementRoute {
                method: "POST",
                path: "/plugins/workbuddy/toggle".into(),
                description: "Enable or disable one account for scheduling (auth_index, enabled).".into(),
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
        ("POST", p) if p == format!("{base}/toggle") => json_response(200, handle_toggle(_body_b64)),
        _ => json_response(404, serde_json::json!({"error": format!("not found: {path}")})),
    }
}

/// Enable/disable one account's participation in scheduling.
/// Body (base64 JSON): {"auth_index": "...", "enabled": bool}
fn handle_toggle(body_b64: &str) -> serde_json::Value {
    let body = b64_decode(body_b64);
    let req: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error": format!("bad json: {e}")}),
    };
    let auth_index = req
        .get("auth_index")
        .or_else(|| req.get("AuthIndex"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if auth_index.is_empty() {
        return serde_json::json!({"error": "auth_index is required"});
    }
    let enabled = req
        .get("enabled")
        .or_else(|| req.get("Enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if let Err(e) = crate::state::set_disabled(&auth_index, !enabled) {
        return serde_json::json!({"error": format!("persist failed: {e}")});
    }
    serde_json::json!({"ok": true, "auth_index": auth_index, "enabled": enabled, "disabled": crate::state::load_disabled()})
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

            let nickname_source = credits
                .pointer("/packages/0/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = nickname_source;
            let display_name = name
                .strip_prefix("workbuddy-")
                .and_then(|s| s.strip_suffix(".json"))
                .unwrap_or(&name)
                .to_string();
            let enabled = !crate::state::load_disabled().iter().any(|d| d == &auth_index);
            // Feed the scheduler cache so the first pick after a panel visit
            // already knows each account's quota.
            if let Some(total_remain) = credits["total_remain"].as_i64() {
                if credits.get("error").is_none() {
                    crate::scheduler::refresh_quota_sync(&auth_index, &credits);
                }
                let _ = total_remain;
            }
            accounts.push(serde_json::json!({
                "auth_index": auth_index,
                "name": name,
                "nickname": display_name,
                "enabled": enabled,
                "credits": credits,
            }));
        }
    }
    { let mut r = serde_json::json!({"accounts": accounts, "fetched_at": fetched_at}); r["_dbg_files"] = files.clone(); r }
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
        assert!(body.contains("账号配额总览"), "panel must contain zone-1 title");
        assert!(body.contains("账号额度明细"), "panel must contain zone-2 title");
        assert!(body.contains("toggleAccount"), "panel must call toggle API");
        let ok2 = handle("GET", "/v0/resource/plugins/workbuddy", "", "");
        assert_eq!(ok2.status_code, 200);
        let bad = handle("GET", "/v0/resource/plugins/workbuddy/nope", "", "");
        assert_eq!(bad.status_code, 404);
    }

    #[test]
    fn toggle_endpoint_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("wb-state-test-{}.json", std::process::id()));
        std::env::set_var("WORKBUDDY_STATE_FILE", &tmp);
        let body = base64::engine::general_purpose::STANDARD.encode(br#"{"auth_index":"idx9","enabled":false}"#);
        let resp = handle("POST", "/v0/management/plugins/workbuddy/toggle", "", &body);
        assert_eq!(resp.status_code, 200);
        let out = String::from_utf8(b64_decode(&resp.body)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["enabled"], false);
        assert!(crate::state::load_disabled().contains(&"idx9".to_string()));
        let body2 = base64::engine::general_purpose::STANDARD.encode(br#"{"auth_index":"idx9","enabled":true}"#);
        let resp2 = handle("POST", "/v0/management/plugins/workbuddy/toggle", "", &body2);
        assert_eq!(resp2.status_code, 200);
        assert!(!crate::state::load_disabled().contains(&"idx9".to_string()));
        let _ = std::fs::remove_file(&tmp);
        std::env::remove_var("WORKBUDDY_STATE_FILE");
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
