use crate::realm::realm;
use crate::rpc::{b64_decode, b64_encode, ManagementRegistration, MgmtResponse};
#[cfg(test)]
use base64::Engine as _;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;

pub const PANEL_HTML: &str = include_str!("../panel.html");

/// Routes the plugin owns. Credential lifecycle (enable/disable, delete) is
/// deliberately NOT here: the host owns it via
/// `PATCH /v0/management/auth-files/status` and `DELETE /v0/management/auth-files`,
/// and `host.auth.save` provably does not persist a `disabled` rewrite
/// (live-verified 2026-09-10). The panel calls those host endpoints directly.
pub fn registration() -> ManagementRegistration {
    let p = realm().provider;
    let mut routes = vec![
        crate::rpc::ManagementRoute {
            method: "GET",
            path: format!("/plugins/{p}/accounts"),
            description: "List WorkBuddy accounts with plan, credits and cycle windows.".into(),
        },
        crate::rpc::ManagementRoute {
            method: "POST",
            path: format!("/plugins/{p}/refresh"),
            description: "Force refresh credits for all accounts.".into(),
        },
        crate::rpc::ManagementRoute {
            method: "GET",
            path: format!("/plugins/{p}/recent"),
            description: "Recent chat calls (last 10): account, model, token usage, status.".into(),
        },
        crate::rpc::ManagementRoute {
            method: "GET",
            path: format!("/plugins/{p}/login/start"),
            description: "Start a login for an additional account; returns the authorization URL and state.".into(),
        },
        crate::rpc::ManagementRoute {
            method: "POST",
            path: format!("/plugins/{p}/login/poll"),
            description: "Poll one login ({state}); on success the new credential is saved to its own file.".into(),
        },
    ];
    // The daily check-in is a CN-only campaign; the Global realm has none.
    if realm().checkin {
        routes.push(crate::rpc::ManagementRoute {
            method: "GET",
            path: format!("/plugins/{p}/checkin"),
            description: "Today's check-in status for all WorkBuddy accounts.".into(),
        });
        routes.push(crate::rpc::ManagementRoute {
            method: "POST",
            path: format!("/plugins/{p}/checkin"),
            description: "Claim the daily check-in for all WorkBuddy accounts.".into(),
        });
    }
    ManagementRegistration {
        routes,
        resources: vec![crate::rpc::ResourceRoute {
            path: "/panel".into(),
            menu: realm().panel_menu.into(),
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
    // 面板是内嵌在 dylib 里的：不禁止缓存的话，升级后浏览器会继续跑旧 JS，
    // 表现为"改了代码但行为没变"（2026-10-19 登录链接修复时踩过）。
    headers.insert("Cache-Control".into(), vec!["no-store, no-cache, must-revalidate, max-age=0".into()]);
    headers.insert("Pragma".into(), vec!["no-cache".into()]);
    MgmtResponse { status_code: 200, headers, body: b64_encode(body.as_bytes()) }
}

/// The embedded panel with this realm's identity substituted in. The HTML is
/// shared by both plugins, so every realm-specific string is a placeholder that
/// is replaced here — serving it unsubstituted would point the panel at the
/// other plugin's management routes.
fn panel_html() -> String {
    let r = realm();
    PANEL_HTML
        .replace("__WB_PROVIDER__", r.provider)
        .replace("__WB_TITLE__", r.panel_title)
        .replace("__WB_CRED_PREFIX__", r.credential_prefix)
        .replace("__WB_CHECKIN__", if r.checkin { "true" } else { "false" })
}

/// Handle one management request. Paths arrive under /v0/management or
/// /v0/resource; authentication is done by the host before dispatch.
pub fn handle(method: &str, path: &str, query: &str, body_b64: &str) -> MgmtResponse {
    let path = path.trim_end_matches('/');

    // Browser UI resource routes. The host injects the CURRENT plugin ID into
    // the served path (/v0/resource/plugins/<plugin-id>/...), and the plugin
    // id is the dylib file name — deliberately different from the provider
    // name so the plugin-store stops matching us to the registry's
    // "workbuddy" entry (zidanefaqih's CodeBuddy Intl CPA). Only routes we
    // declared are dispatched here, so accept any id segment.
    let res_prefix = "/v0/resource/plugins/";
    if method == "GET" && path.starts_with(res_prefix) {
        let rest = &path[res_prefix.len()..];
        let sub = match rest.split_once('/') {
            Some((_, tail)) => format!("/{tail}"),
            None => String::new(), // exactly /v0/resource/plugins/<id>
        };
        return match sub.as_str() {
            "" | "/" | "/panel" | "/panel.html" => html_response(&panel_html()),
            _ => json_response(404, serde_json::json!({"error": "not found"})),
        };
    }

    let base = format!("/v0/management/plugins/{}", realm().provider);
    match (method, path) {
        ("GET", p) if p == base || p == format!("{base}/accounts") => {
            json_response(200, build_accounts_dashboard())
        }
        ("GET", p) if p == format!("{base}/recent") => json_response(200, crate::recent::json()),
        ("POST", p) if p == format!("{base}/refresh") => {
            json_response(200, build_accounts_dashboard())
        }
        ("GET", p) if p == format!("{base}/checkin") => {
            json_response(200, crate::checkin::status_all())
        }
        ("POST", p) if p == format!("{base}/checkin") => {
            json_response(200, crate::checkin::run_all())
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
        Ok(r) => {
            // 宿主转发时会把响应里的 `&` 转义成 `&amp;`，authUrl 的 state 参数
            // 因此在浏览器侧丢失（登录页报"链接不完整"）。额外给出去掉 state 的
            // 前缀，面板用 url_base + "&state=" + state 自行拼装，不依赖原串转义。
            let url_base = r
                .url
                .split("&state=")
                .next()
                .unwrap_or(&r.url)
                .to_string();
            serde_json::json!({
                "ok": true,
                "provider": r.provider,
                "url": r.url,
                "url_base": url_base,
                "state": r.state,
                "expires_at": r.expires_at,
            })
        }
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
    // 不走 `host.auth.save`：宿主 8.0.30 会把插件送来的凭据登记成别的 provider
    // （2026-10-20 实测：workbuddy 的凭据落盘后 provider/type 双双击成 `qoderwork`，
    // 于是面板按 provider 过滤时看不见、路由也交给了另一个插件）。直接按宿主能识别
    // 的最小信封写文件，宿主文件监听会立刻把它注册为 workbuddy。
    match write_credential_file(&auth.file_name, &cred) {
        Ok(()) => serde_json::json!({
            "status": "success",
            "file_name": auth.file_name,
            "label": auth.label,
            "saved": true,
            "nickname": nickname,
        }),
        Err(e) => {
            // 兜底：写盘失败时仍走宿主接口，宁可标签不对也不能丢凭据
            let save_body = serde_json::json!({"name": auth.file_name, "json": cred});
            match crate::cabi::host_call("host.auth.save", save_body.to_string().as_bytes()) {
                Ok(_) => serde_json::json!({
                    "status": "success",
                    "file_name": auth.file_name,
                    "label": auth.label,
                    "saved": true,
                    "nickname": nickname,
                    "warning": format!("direct write failed, saved through host: {e}"),
                }),
                Err(e2) => serde_json::json!({
                    "status": "success",
                    "file_name": auth.file_name,
                    "saved": false,
                    "error": format!("write failed: {e}; host.auth.save failed: {e2}"),
                }),
            }
        }
    }
}

/// 凭据目录。插件自己的状态文件就在 `~/.cli-proxy-api` 下，凭据与它同目录；
/// 宿主的 `auth-dir` 若被改过，可用 `CPA_AUTH_DIR` 覆盖。
fn auth_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("CPA_AUTH_DIR") {
        if !d.trim().is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".cli-proxy-api")
}

/// 按宿主能识别的形态落盘：`{account, auth, disabled, type}`，**不写 `provider`**。
/// 宿主优先读 `provider`，缺省时按 `type` 推导；写入 provider 反而会被带偏。
/// 原子写（.tmp + rename），并保留旧文件里的 note/logo/disabled——重新登录/刷新
/// 不该抹掉用户改过的备注，也不该悄悄把已停用的账号重新启用。
pub(crate) fn write_credential_file(file_name: &str, cred: &serde_json::Value) -> Result<(), String> {
    validate_credential_file_name(file_name)?;
    let path = auth_dir().join(file_name.trim());
    write_credential_path(&path, cred)
}

fn validate_credential_file_name(file_name: &str) -> Result<(), String> {
    let name = file_name.trim();
    if name.is_empty() || name.contains('\\') || name.contains("..") {
        return Err(format!("invalid credential file name: {file_name}"));
    }
    let mut components = std::path::Path::new(name).components();
    let valid = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !valid {
        return Err(format!("invalid credential file name: {file_name}"));
    }
    Ok(())
}

pub(crate) fn write_credential_path(
    path: &std::path::Path,
    cred: &serde_json::Value,
) -> Result<(), String> {
    let prev: Option<serde_json::Value> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());

    let mut out = serde_json::Map::new();
    if let Some(m) = cred.as_object() {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    // 放在 cred 之后：刷新/重新登录不能覆盖宿主的 disabled/备注/头像。
    if let Some(serde_json::Value::Object(m)) = &prev {
        for k in ["note", "logo", "disabled"] {
            if let Some(v) = m.get(k) {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    out.insert("type".into(), serde_json::json!(realm().credential_type));
    out.remove("provider");

    let body = serde_json::to_vec(&serde_json::Value::Object(out)).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = write_private_file(&tmp, &body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {}: {e}", path.display())
    })
}

fn write_private_file(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    f.write_all(body).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

/// Does one `host.auth.list` row belong to this realm?
///
/// An explicit provider/type wins. When both are empty (older host versions
/// leave them blank) fall back to the on-disk name — without that fallback the
/// old `if !provider.is_empty() && ...` test let every foreign account through,
/// so the international panel would list CN/codex/qoder credentials and query
/// their credits against the Global base.
pub(crate) fn row_belongs_to_realm(f: &Value) -> bool {
    let r = realm();
    let provider = f.get("provider").and_then(|v| v.as_str()).unwrap_or("").trim();
    let ftype = f.get("type").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !provider.is_empty() || !ftype.is_empty() {
        return provider == r.provider || ftype == r.credential_type;
    }
    let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
    r.owns_credential_file(name)
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
            if !row_belongs_to_realm(f) {
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
pub(crate) fn display_from_file(name: &str) -> String {
    crate::realm::display_name_from_file(name)
}

pub(crate) fn host_auth_get(auth_index: &str) -> Result<Vec<u8>, String> {
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
    fn served_panel_has_no_placeholders_and_targets_realm() {
        let body = panel_html();
        assert!(!body.contains("__WB_"), "placeholder leaked into the served panel");
        assert!(body.contains(&format!(
            r#"const API = "/v0/management/plugins/{}""#,
            realm().provider
        )));
        assert!(body.contains(&format!("<title>{}</title>", realm().panel_title)));
        assert!(body.contains(r#"id="checkinCard""#), "card markup stays; JS decides visibility");
        assert!(body.contains(&format!("const CHECKIN_ENABLED = {};", realm().checkin)));
    }

    #[test]
    fn panel_keeps_host_credential_lifecycle_wiring() {
        let body = panel_html();
        assert!(body.contains(r#"const HOSTFL = "/v0/management/auth-files""#));
        assert!(body.contains("HOSTFL+\"/status\""));
    }

    #[test]
    fn routes_follow_realm_provider() {
        let reg = registration();
        let p = realm().provider;
        assert!(reg.routes.iter().any(|r| r.path == format!("/plugins/{p}/accounts")));
        assert!(reg.routes.iter().all(|r| r.path.starts_with(&format!("/plugins/{p}/"))));
        assert_eq!(reg.routes.iter().any(|r| r.path.ends_with("/checkin")), realm().checkin);
        assert_eq!(reg.resources[0].menu, realm().panel_menu);
    }

    #[test]
    fn dashboard_row_filter_is_realm_scoped() {
        let rows = [
            serde_json::json!({"provider": "", "type": "", "name": "workbuddy-intl-abc.json"}),
            serde_json::json!({"provider": "", "type": "", "name": "workbuddy-abc.json"}),
            serde_json::json!({"provider": "codex", "type": "codex", "name": "codex-x.json"}),
        ];
        let kept: Vec<&str> = rows
            .iter()
            .filter(|r| row_belongs_to_realm(r))
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].starts_with(realm().credential_prefix));
    }

    #[test]
    fn registration_routes_shape() {
        let r = registration();
        let p = realm().provider;
        assert_eq!(r.routes[0].path, format!("/plugins/{p}/accounts"));
        assert_eq!(r.routes[0].method, "GET");
        assert_eq!(r.routes[1].method, "POST");
        assert_eq!(r.routes[1].path, format!("/plugins/{p}/refresh"));
        assert!(r.routes.iter().any(|x| x.path == format!("/plugins/{p}/login/start")));
        assert!(r.routes.iter().any(|x| x.path == format!("/plugins/{p}/login/poll")));
        assert!(r.routes.iter().any(|x| x.path == format!("/plugins/{p}/recent")));
        assert_eq!(r.resources[0].menu, realm().panel_menu);
        assert_eq!(r.resources[0].path, "/panel");
    }

    #[test]
    fn recent_route_returns_calls_array() {
        let path = format!("/v0/management/plugins/{}/recent", realm().provider);
        let resp = handle("GET", &path, "", "");
        assert_eq!(resp.status_code, 200);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(b64_decode(&resp.body)).unwrap()).unwrap();
        assert!(v["calls"].is_array());
    }

    #[test]
    fn panel_served_and_404() {
        let ok = handle("GET", "/v0/resource/plugins/workbuddy/panel", "", "");
        assert_eq!(ok.status_code, 200);
        let body = String::from_utf8(b64_decode(&ok.body)).unwrap();
        assert!(body.contains("总积分额度"), "panel must contain v2 total card");
        assert!(body.contains("点徽章启停"), "zone 1 must be a plain account list (no selection)");
        assert!(body.contains("最近调用"), "panel must contain the recent-calls zone");
        assert!(body.contains("recentTable"), "panel must render the recent-calls table");
        assert!(body.contains("账号额度明细"), "panel must contain the detail zone");
        assert!(body.contains("ddPick") && body.contains("ddPop"), "detail zone must use the custom dropdown");
        assert!(!body.contains("<select"), "native select must be replaced by the custom dropdown");
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
    fn credential_write_is_private_and_preserves_flags() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!(
            "workbuddy-cred-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CPA_AUTH_DIR", &dir);
        let path = dir.join("workbuddy-test.json");
        let prev = serde_json::json!({
            "account": {"uid": "u", "enterpriseId": "e", "nickname": "n"},
            "auth": {"accessToken": "old", "refreshToken": "oldr", "expiresAt": 1, "domain": "www.codebuddy.cn"},
            "note": "keep", "logo": "x", "disabled": true, "type": "workbuddy"
        });
        std::fs::write(&path, prev.to_string()).unwrap();
        let cred = serde_json::json!({
            "account": {"uid": "u", "enterpriseId": "e", "nickname": "n"},
            "auth": {"accessToken": "new", "refreshToken": "newr", "expiresAt": 2, "domain": "www.codebuddy.cn"},
            "disabled": false
        });
        write_credential_file("workbuddy-test.json", &cred).unwrap();
        std::env::remove_var("CPA_AUTH_DIR");

        let out: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(out["auth"]["accessToken"], "new");
        assert_eq!(out["note"], "keep");
        assert_eq!(out["logo"], "x");
        assert_eq!(out["disabled"], true, "refresh must not re-enable a disabled account");
        assert_eq!(out["type"], realm().credential_type);
        assert!(out.get("provider").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "credential must not be group/world readable, got {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_file_name_rejects_traversal_and_windows_paths() {
        for bad in [
            "",
            "../x.json",
            "a/b.json",
            "C:\\evil.json",
            "C:/evil.json",
            "..",
            "a\\b.json",
        ] {
            assert!(
                validate_credential_file_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(validate_credential_file_name("workbuddy-abc.json").is_ok());
    }

    #[test]
    fn checkin_routes_are_declared_and_handled() {
        let reg = registration();
        let p = realm().provider;
        let path = format!("/plugins/{p}/checkin");
        assert_eq!(reg.routes.iter().any(|r| r.method == "GET" && r.path == path), realm().checkin);
        assert_eq!(reg.routes.iter().any(|r| r.method == "POST" && r.path == path), realm().checkin);
        if !realm().checkin {
            return; // Global realm has no check-in campaign at all
        }
        let resp = handle("GET", &format!("/v0/management/plugins/{p}/checkin"), "", "");
        assert_eq!(resp.status_code, 200);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(b64_decode(&resp.body)).unwrap()).unwrap();
        assert!(v["accounts"].is_array(), "checkin status must return an accounts array");
    }

    #[test]
    fn panel_contains_checkin_controls() {
        let ok = handle("GET", "/v0/resource/plugins/workbuddy/panel", "", "");
        let body = String::from_utf8(b64_decode(&ok.body)).unwrap();
        assert!(body.contains("每日签到"), "panel must contain the check-in card");
        assert!(body.contains("checkinCard"), "check-in card must render into #checkinCard");
        assert!(body.contains("runCheckin"), "panel must wire the batch check-in button");
        assert!(body.contains(r#"API+"/checkin""#), "panel must call the check-in API");
        assert!(body.contains("批量签到"), "panel must label the batch check-in action");
    }

    #[test]
    fn login_poll_requires_state_from_body_or_query() {
        let resp = handle("GET", &format!("/v0/management/plugins/{}/login/poll", realm().provider), "", "");
        assert_eq!(resp.status_code, 200);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(b64_decode(&resp.body)).unwrap()).unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["message"].as_str().unwrap().contains("state"));

        // Unknown state surfaces as an error status rather than a panic.
        let body = base64::engine::general_purpose::STANDARD.encode(br#"{"state":"nope"}"#);
        let resp = handle("POST", &format!("/v0/management/plugins/{}/login/poll", realm().provider), "", &body);
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
        let p = realm().credential_prefix;
        assert_eq!(display_from_file(&format!("{p}3a417a.json")), "3a417a");
        let legacy_stem = p.trim_end_matches('-');
        assert_eq!(display_from_file(&format!("{legacy_stem}.json")), legacy_stem);
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
        assert_eq!(handle("GET", &format!("/v0/management/plugins/{}/zzz", realm().provider), "", "").status_code, 404);
    }
}

/// List every auth file entry the host knows about (envelope-unwrapped).
/// Used by the accounts dashboard and by the scheduler's ID->auth_index map.
pub(crate) fn list_auth_files() -> Result<serde_json::Value, String> {
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
