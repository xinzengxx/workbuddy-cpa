use crate::rpc::{b64_decode, b64_encode, ManagementRegistration, MgmtResponse};
#[cfg(test)]
use base64::Engine as _;
use serde_json::Value;
use std::collections::HashMap;

pub const PANEL_HTML: &str = include_str!("../panel.html");
pub const PROVIDER_NAME: &str = "workbuddy";

/// Routes the plugin owns. Credential lifecycle (enable/disable, delete) is
/// deliberately NOT here: the host owns it via
/// `PATCH /v0/management/auth-files/status` and `DELETE /v0/management/auth-files`,
/// and `host.auth.save` provably does not persist a `disabled` rewrite
/// (live-verified 2026-09-10). The panel calls those host endpoints directly.
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
                method: "GET",
                path: "/plugins/workbuddy/login/start".into(),
                description: "Start a login for an additional account; returns the authorization URL and state.".into(),
            },
            crate::rpc::ManagementRoute {
                method: "POST",
                path: "/plugins/workbuddy/login/poll".into(),
                description: "Poll one login ({state}); on success the new credential is saved to its own file.".into(),
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
pub fn handle(method: &str, path: &str, query: &str, body_b64: &str) -> MgmtResponse {
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
        ("GET", p) if p == format!("{base}/login/start") => json_response(200, handle_login_start()),
        ("GET", p) if p == format!("{base}/login/poll") => {
            json_response(200, handle_login_poll(query, body_b64))
        }
        ("POST", p) if p == format!("{base}/login/poll") => {
            json_response(200, handle_login_poll(query, body_b64))
        }
        _ => json_response(404, serde_json::json!({"error": format!("not found: {path}")})),
    }
}

/// Extract one field from a raw query string ("a=1&b=2"). Percent-decoding is
/// limited to the escapes a login state actually uses (`%2F`, `+`) because the
/// host passes the query through verbatim.
fn query_param(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == key {
            return v.replace("%2F", "/").replace("%2f", "/").replace('+', " ");
        }
    }
    String::new()
}

fn json_body(body_b64: &str) -> serde_json::Value {
    let body = b64_decode(body_b64);
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

/// Start one additional-account login. The host drives the polling cadence for
/// its own auth page, but this panel polls itself, so both paths coexist.
fn handle_login_start() -> serde_json::Value {
    match crate::auth::start_login() {
        Ok(r) => serde_json::json!({
            "ok": true,
            "provider": r.provider,
            "url": r.url,
            "state": r.state,
            "expires_at": r.expires_at,
        }),
        Err(e) => serde_json::json!({"error": e}),
    }
}

/// Poll one login: `state` comes from the POST body, or the query string when
/// the panel uses GET. On success the credential is saved to its own file
/// (`workbuddy-{uid6}.json`) so every account keeps an independent file.
fn handle_login_poll(query: &str, body_b64: &str) -> serde_json::Value {
    let body = json_body(body_b64);
    let mut state = crate::rpc::get_field(&body, "state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if state.is_empty() {
        state = query_param(query, "state");
    }
    if state.is_empty() {
        return serde_json::json!({"status": "error", "message": "state is required"});
    }
    let resp = match crate::auth::poll_login(&state) {
        Ok(r) => r,
        Err(e) => return serde_json::json!({"status": "error", "message": e}),
    };
    if resp.status != "success" {
        return serde_json::json!({"status": resp.status, "message": resp.message});
    }
    let auth = match resp.auth {
        Some(a) => a,
        None => return serde_json::json!({"status": "error", "message": "no auth payload"}),
    };
    let cred: serde_json::Value = match serde_json::from_slice(&b64_decode(&auth.storage_json)) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({"status": "error", "message": format!("credential parse failed: {e}")})
        }
    };
    let nickname = cred
        .pointer("/account/nickname")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let save_body = serde_json::json!({"name": auth.file_name, "json": cred});
    match crate::cabi::host_call("host.auth.save", save_body.to_string().as_bytes()) {
        Ok(_) => serde_json::json!({
            "status": "success",
            "file_name": auth.file_name,
            "label": auth.label,
            "saved": true,
            "nickname": nickname,
        }),
        Err(e) => serde_json::json!({
            "status": "success",
            "file_name": auth.file_name,
            "saved": false,
            "error": format!("host.auth.save failed: {e}"),
        }),
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
            // `name` is the on-disk file name and is what the host's own
            // /v0/management/auth-files status & delete endpoints key on.
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // One credential read serves display name and billing.
            let raw = host_auth_get(&auth_index);
            let cred_json: Option<serde_json::Value> = raw
                .as_ref()
                .ok()
                .and_then(|b| serde_json::from_slice(b).ok());
            let nickname = cred_json
                .as_ref()
                .and_then(|v| v.pointer("/account/nickname").and_then(|n| n.as_str()))
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| display_from_file(&name));
            let credits = match (raw, cred_json) {
                (Ok(bytes), Some(_)) => {
                    match serde_json::from_slice::<crate::rpc::StoredAuth>(&bytes) {
                        Ok(sa) => {
                            let mut c = crate::billing::fetch_credits(&sa);
                            c["fetched_at"] = serde_json::json!(fetched_at);
                            c
                        }
                        Err(_) => serde_json::json!({"error": "parse auth failed", "fetched_at": fetched_at}),
                    }
                }
                (Err(e), _) => serde_json::json!({"error": format!("read auth failed: {e}"), "fetched_at": fetched_at}),
                (Ok(_), None) => serde_json::json!({"error": "parse auth failed", "fetched_at": fetched_at}),
            };
            // The host owns the enabled/disabled state; its field is authoritative.
            let disabled = f.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
            // Feed the scheduler cache so the first pick after a panel visit
            // already knows each account's quota.
            if credits.get("error").is_none() && credits["total_remain"].is_i64() {
                crate::scheduler::refresh_quota_sync(&auth_index, &credits);
            }
            accounts.push(serde_json::json!({
                "auth_index": auth_index,
                "name": name,
                "file_name": name,
                "id": f.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "path": f.get("path").and_then(|v| v.as_str()).unwrap_or(""),
                "nickname": nickname,
                "label": f.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                "enabled": !disabled,
                "disabled": disabled,
                "status": f.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                "status_message": f.get("status_message").and_then(|v| v.as_str()).unwrap_or(""),
                "unavailable": f.get("unavailable").and_then(|v| v.as_bool()).unwrap_or(false),
                "failed": f.get("failed").and_then(|v| v.as_u64()).unwrap_or(0),
                "recent_requests": f.get("recent_requests").and_then(|v| v.as_u64()).unwrap_or(0),
                "credits": credits,
            }));
        }
    }
    serde_json::json!({"accounts": accounts, "fetched_at": fetched_at})
}

/// `workbuddy-abcdef.json` -> `abcdef`; anything else keeps its stem.
fn display_from_file(name: &str) -> String {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    stem.strip_prefix("workbuddy-").unwrap_or(stem).to_string()
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
        assert!(r.routes.iter().any(|x| x.path == "/plugins/workbuddy/login/start"));
        assert!(r.routes.iter().any(|x| x.path == "/plugins/workbuddy/login/poll"));
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
        assert!(body.contains("addAccount"), "panel must expose the add-account flow");
        assert!(body.contains("login/start"), "panel must call the login start API");
        assert!(body.contains("login/poll"), "panel must call the login poll API");
        assert!(body.contains("deleteAccount"), "panel must expose the delete flow");
        assert!(body.contains("toggleAccount"), "panel must expose the enable/disable flow");
        // Credential lifecycle must go through the host's authoritative API,
        // not through host.auth.save (which cannot persist `disabled`).
        assert!(
            body.contains(r#"const HOSTFL = "/v0/management/auth-files""#),
            "panel must target the host auth-files API"
        );
        assert!(body.contains("HOSTFL+\"/status\""), "toggle must PATCH the host status API");
        assert!(body.contains("method:\"DELETE\""), "delete must DELETE through the host");
        assert!(
            !body.contains("API+\"/toggle\""),
            "panel must not post to a plugin-side toggle route"
        );
        let ok2 = handle("GET", "/v0/resource/plugins/workbuddy", "", "");
        assert_eq!(ok2.status_code, 200);
        let bad = handle("GET", "/v0/resource/plugins/workbuddy/nope", "", "");
        assert_eq!(bad.status_code, 404);
    }

    #[test]
    fn login_poll_requires_state_from_body_or_query() {
        let resp = handle("GET", "/v0/management/plugins/workbuddy/login/poll", "", "");
        assert_eq!(resp.status_code, 200);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(b64_decode(&resp.body)).unwrap()).unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["message"].as_str().unwrap().contains("state"));

        // Unknown state surfaces as an error status rather than a panic.
        let body = base64::engine::general_purpose::STANDARD.encode(br#"{"state":"nope"}"#);
        let resp = handle("POST", "/v0/management/plugins/workbuddy/login/poll", "", &body);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(b64_decode(&resp.body)).unwrap()).unwrap();
        assert_eq!(v["status"], "error");
    }

    #[test]
    fn credential_lifecycle_routes_are_not_owned_by_the_plugin() {
        let reg = registration();
        assert!(!reg.routes.iter().any(|r| r.path.ends_with("/toggle")));
        assert!(!reg.routes.iter().any(|r| r.path.ends_with("/delete")));
    }

    #[test]
    fn display_name_from_file_rules() {
        assert_eq!(display_from_file("workbuddy-3a417a.json"), "3a417a");
        assert_eq!(display_from_file("workbuddy.json"), "workbuddy");
        assert_eq!(display_from_file("plain.json"), "plain");
    }

    #[test]
    fn query_param_extracts_state() {
        assert_eq!(query_param("state=abc%2Fdef&x=1", "state"), "abc/def");
        assert_eq!(query_param("x=1&state=zzz", "state"), "zzz");
        assert_eq!(query_param("x=1", "state"), "");
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
