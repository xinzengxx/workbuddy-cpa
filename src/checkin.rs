use crate::rpc::{b64_decode, b64_encode, StoredAuth};
use crate::upstream::{
    backend_header_set, post_envelope_with_timeout, shared_agent, UpstreamError,
    ENDPOINT_CHECKIN_DAILY, ENDPOINT_CHECKIN_STATUS,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECKIN_CALL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
struct Status {
    active: bool,
    today_checked_in: bool,
    streak_days: i64,
    total_credits: i64,
    daily_credit: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct Claim {
    credit: i64,
    streak_days: i64,
    is_streak_day: bool,
}

#[derive(Clone)]
struct AccountRow {
    auth_index: String,
    file_name: String,
    path: String,
    nickname: String,
    disabled: bool,
    sa: Option<StoredAuth>,
    load_error: String,
}

enum ClaimOutcome {
    Claimed(Claim),
    Already,
}

static RUNNING: AtomicBool = AtomicBool::new(false);

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn payload(data: &Value) -> &Value {
    if data.get("credit").is_some()
        || data.get("today_checked_in").is_some()
        || data.get("active").is_some()
    {
        data
    } else {
        &data["data"]
    }
}

fn parse_status(data: &Value) -> Status {
    let d = payload(data);
    Status {
        active: d["active"].as_bool().unwrap_or(false),
        today_checked_in: d["today_checked_in"].as_bool().unwrap_or(false),
        streak_days: d["streak_days"].as_i64().unwrap_or(0),
        total_credits: d["total_credits"].as_i64().unwrap_or(0),
        daily_credit: d["daily_credit"].as_i64().unwrap_or(0),
    }
}

fn parse_claim(data: &Value) -> Claim {
    let d = payload(data);
    Claim {
        credit: d["credit"].as_i64().unwrap_or(0),
        streak_days: d["streak_days"].as_i64().unwrap_or(0),
        is_streak_day: d["is_streak_day"].as_bool().unwrap_or(false),
    }
}

fn is_already_checked_in(err: &UpstreamError) -> bool {
    matches!(
        err,
        UpstreamError::Api(10001, _) | UpstreamError::HttpApi(_, 10001, _)
    )
}

fn http_status(err: &UpstreamError) -> Option<u16> {
    match err {
        UpstreamError::Http(s) | UpstreamError::HttpApi(s, _, _) => Some(*s),
        _ => None,
    }
}

fn needs_refresh(expires_at: i64, now: i64) -> bool {
    expires_at <= now + 60
}

fn is_realm_entry(provider: &str, ftype: &str, file_name: &str) -> bool {
    let r = crate::realm::realm();
    provider == r.provider
        || ftype == r.credential_type
        || (provider.is_empty() && ftype.is_empty() && r.owns_credential_file(file_name))
}

fn account_key(sa: &StoredAuth, file_name: &str) -> String {
    let uid = sa.account.uid.trim();
    if uid.is_empty() {
        format!("file:{file_name}")
    } else {
        format!("uid:{uid}")
    }
}

fn dedupe_accounts(rows: Vec<AccountRow>) -> Vec<AccountRow> {
    let mut out: Vec<AccountRow> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let key = match &row.sa {
            Some(sa) => account_key(sa, &row.file_name),
            None => format!("file:{}", row.file_name),
        };
        if let Some(&i) = positions.get(&key) {
            if out[i].disabled && !row.disabled {
                out[i] = row;
            }
            continue;
        }
        positions.insert(key, out.len());
        out.push(row);
    }
    out
}

fn status_error_message(err: &UpstreamError) -> String {
    match http_status(err) {
        Some(401) => "登录已过期，点击「批量签到」会自动刷新".into(),
        _ => err.to_string(),
    }
}

fn run_error_message(err: &UpstreamError) -> String {
    match http_status(err) {
        Some(401) => "登录已过期，自动刷新失败，请重新登录".into(),
        _ => err.to_string(),
    }
}

fn retry_error_message(err: &UpstreamError) -> String {
    match http_status(err) {
        Some(401) => "刷新成功但 token 仍被拒绝，请重新登录".into(),
        _ => run_error_message(err),
    }
}

fn fetch_status(sa: &StoredAuth) -> Result<Status, UpstreamError> {
    let data = post_envelope_with_timeout(
        shared_agent(),
        ENDPOINT_CHECKIN_STATUS,
        backend_header_set(sa),
        "{}",
        CHECKIN_CALL_TIMEOUT,
    )?;
    Ok(parse_status(&data))
}

fn fetch_claim(sa: &StoredAuth) -> Result<Claim, UpstreamError> {
    let data = post_envelope_with_timeout(
        shared_agent(),
        ENDPOINT_CHECKIN_DAILY,
        backend_header_set(sa),
        "{}",
        CHECKIN_CALL_TIMEOUT,
    )?;
    Ok(parse_claim(&data))
}

fn persist_refreshed_credential(account: &AccountRow, cred: &Value) -> Result<(), String> {
    let file_name = account.file_name.trim();
    if file_name.is_empty() {
        return Err("无法定位凭据文件，不能刷新 token".into());
    }
    let host_path = account.path.trim();
    if host_path.is_empty() {
        return crate::management::write_credential_file(file_name, cred);
    }
    let path = std::path::Path::new(host_path);
    let name_matches = path.file_name().and_then(|n| n.to_str()) == Some(file_name);
    if !name_matches {
        return Err(format!("宿主凭据路径与文件名不一致：{host_path}"));
    }
    crate::management::write_credential_path(path, cred)
}

fn refresh_credential(account: &AccountRow, sa: &StoredAuth) -> Result<StoredAuth, String> {
    let storage = b64_encode(&serde_json::to_vec(sa).map_err(|e| e.to_string())?);
    let resp = crate::auth::refresh(&storage)?;
    let cred: Value = serde_json::from_slice(&b64_decode(&resp.auth.storage_json))
        .map_err(|e| format!("刷新响应解析失败: {e}"))?;
    persist_refreshed_credential(account, &cred)?;
    serde_json::from_value(cred).map_err(|e| format!("刷新凭据解析失败: {e}"))
}

fn account_auth(account: &AccountRow, allow_refresh: bool) -> Result<StoredAuth, String> {
    let sa = account.sa.clone().ok_or_else(|| {
        if account.load_error.is_empty() {
            "凭据不可用".to_string()
        } else {
            account.load_error.clone()
        }
    })?;
    if allow_refresh && needs_refresh(sa.auth.expires_at, now_unix()) {
        return refresh_credential(account, &sa).map_err(|e| format!("token 刷新失败：{e}"));
    }
    Ok(sa)
}

fn call_with_refresh_using<T>(
    sa: &mut StoredAuth,
    mut call: impl FnMut(&StoredAuth) -> Result<T, UpstreamError>,
    refresh: impl FnOnce(&StoredAuth) -> Result<StoredAuth, String>,
) -> Result<T, String> {
    match call(sa) {
        Ok(v) => Ok(v),
        Err(e) if http_status(&e) == Some(401) => {
            let refreshed = refresh(sa).map_err(|e| format!("登录已过期，自动刷新失败：{e}"))?;
            *sa = refreshed;
            call(sa).map_err(|e| retry_error_message(&e))
        }
        Err(e) => Err(run_error_message(&e)),
    }
}

fn call_with_refresh<T>(
    account: &AccountRow,
    sa: &mut StoredAuth,
    call: impl FnMut(&StoredAuth) -> Result<T, UpstreamError>,
) -> Result<T, String> {
    call_with_refresh_using(sa, call, |s| refresh_credential(account, s))
}

fn claim_with_refresh(account: &AccountRow, sa: &mut StoredAuth) -> Result<ClaimOutcome, String> {
    match fetch_claim(sa) {
        Ok(c) => Ok(ClaimOutcome::Claimed(c)),
        Err(e) if is_already_checked_in(&e) => Ok(ClaimOutcome::Already),
        Err(e) if http_status(&e) == Some(401) => {
            let refreshed = refresh_credential(account, sa)
                .map_err(|e| format!("登录已过期，自动刷新失败：{e}"))?;
            *sa = refreshed;
            match fetch_claim(sa) {
                Ok(c) => Ok(ClaimOutcome::Claimed(c)),
                Err(e) if is_already_checked_in(&e) => Ok(ClaimOutcome::Already),
                Err(e) => Err(retry_error_message(&e)),
            }
        }
        Err(e) => Err(run_error_message(&e)),
    }
}

fn str_field(v: &Value, name: &str) -> String {
    crate::rpc::get_field(v, name)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn load_accounts() -> Result<Vec<AccountRow>, String> {
    let files = crate::management::list_auth_files()?;
    let list = match files.as_array() {
        Some(a) => a,
        None if files.is_null() => return Ok(Vec::new()),
        None => return Err("host.auth.list returned a non-array".into()),
    };
    let mut rows = Vec::new();
    for f in list {
        let provider = str_field(f, "provider");
        let ftype = str_field(f, "type");
        let file_name = str_field(f, "name");
        if !is_realm_entry(&provider, &ftype, &file_name) {
            continue;
        }
        let auth_index = str_field(f, "auth_index");
        let path = str_field(f, "path");
        let disabled = crate::rpc::get_field(f, "disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut sa = None;
        let mut load_error = String::new();
        if auth_index.is_empty() {
            load_error = "缺少 auth_index".into();
        } else {
            match crate::management::host_auth_get(&auth_index) {
                Ok(bytes) => match serde_json::from_slice::<StoredAuth>(&bytes) {
                    Ok(v) => sa = Some(v),
                    Err(e) => load_error = format!("凭据解析失败: {e}"),
                },
                Err(e) => load_error = format!("读取凭据失败: {e}"),
            }
        }
        let nickname = sa
            .as_ref()
            .map(|s| s.account.nickname.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::management::display_from_file(&file_name));
        rows.push(AccountRow {
            auth_index,
            file_name,
            path,
            nickname,
            disabled,
            sa,
            load_error,
        });
    }
    Ok(dedupe_accounts(rows))
}

fn status_json(account: &AccountRow, status: Option<&Status>, error: &str) -> Value {
    let mut v = json!({
        "auth_index": account.auth_index,
        "file_name": account.file_name,
        "nickname": account.nickname,
        "disabled": account.disabled,
        "error": error,
        "active": Value::Null,
        "today_checked_in": Value::Null,
        "streak_days": Value::Null,
        "total_credits": Value::Null,
        "daily_credit": Value::Null,
    });
    if let Some(st) = status {
        v["active"] = json!(st.active);
        v["today_checked_in"] = json!(st.today_checked_in);
        v["streak_days"] = json!(st.streak_days);
        v["total_credits"] = json!(st.total_credits);
        v["daily_credit"] = json!(st.daily_credit);
    }
    v
}

fn result_json(
    account: &AccountRow,
    status: &str,
    credit: Option<i64>,
    st: Option<&Status>,
    error: &str,
) -> Value {
    let mut v = json!({
        "auth_index": account.auth_index,
        "file_name": account.file_name,
        "nickname": account.nickname,
        "status": status,
        "error": error,
    });
    if let Some(c) = credit {
        v["credit"] = json!(c);
    }
    if let Some(st) = st {
        v["streak_days"] = json!(st.streak_days);
        v["total_credits"] = json!(st.total_credits);
        v["daily_credit"] = json!(st.daily_credit);
    }
    v
}

fn summarize(results: &[Value]) -> Value {
    let mut success = 0;
    let mut already = 0;
    let mut inactive = 0;
    let mut error = 0;
    for r in results {
        match r["status"].as_str().unwrap_or("") {
            "success" => success += 1,
            "already" => already += 1,
            "inactive" => inactive += 1,
            _ => error += 1,
        }
    }
    json!({
        "success": success,
        "already": already,
        "inactive": inactive,
        "error": error,
    })
}

fn status_for_account(account: &AccountRow) -> Value {
    let Some(sa) = account.sa.as_ref() else {
        return status_json(account, None, &account.load_error);
    };
    match fetch_status(sa) {
        Ok(st) => status_json(account, Some(&st), ""),
        Err(e) => status_json(account, None, &status_error_message(&e)),
    }
}

pub(crate) fn status_all() -> Value {
    let fetched_at = now_rfc3339();
    let accounts = match load_accounts() {
        Ok(a) => a,
        Err(e) => {
            return json!({"accounts": [], "error": e, "running": RUNNING.load(Ordering::SeqCst), "fetched_at": fetched_at});
        }
    };
    let out: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<(&AccountRow, _)> = accounts
            .iter()
            .map(|account| (account, scope.spawn(move || status_for_account(account))))
            .collect();
        handles
            .into_iter()
            .map(|(account, handle)| {
                handle
                    .join()
                    .unwrap_or_else(|_| status_json(account, None, "签到状态查询线程异常"))
            })
            .collect()
    });
    json!({
        "accounts": out,
        "running": RUNNING.load(Ordering::SeqCst),
        "fetched_at": fetched_at,
    })
}

struct RunningGuard;

impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

pub(crate) fn run_all() -> Value {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return json!({"ok": false, "error": "签到任务正在运行", "running": true, "results": []});
    }
    let _guard = RunningGuard;
    run_all_inner()
}

fn process_account(account: &AccountRow) -> Value {
    let mut sa = match account_auth(account, true) {
        Ok(sa) => sa,
        Err(e) => return result_json(account, "error", None, None, &e),
    };
    let status = match call_with_refresh(account, &mut sa, fetch_status) {
        Ok(st) => st,
        Err(e) => return result_json(account, "error", None, None, &e),
    };
    if status.today_checked_in {
        return result_json(account, "already", None, Some(&status), "");
    }
    if !status.active {
        return result_json(account, "inactive", None, Some(&status), "");
    }
    match claim_with_refresh(account, &mut sa) {
        Ok(ClaimOutcome::Already) => {
            let st = call_with_refresh(account, &mut sa, fetch_status).unwrap_or(status);
            result_json(account, "already", None, Some(&st), "")
        }
        Ok(ClaimOutcome::Claimed(claim)) => {
            let mut streak = claim.streak_days;
            let mut total = status.total_credits.saturating_add(claim.credit);
            if let Ok(after) = call_with_refresh(account, &mut sa, fetch_status) {
                streak = after.streak_days;
                total = after.total_credits;
            }
            json!({
                "auth_index": account.auth_index,
                "file_name": account.file_name,
                "nickname": account.nickname,
                "status": "success",
                "credit": claim.credit,
                "streak_days": streak,
                "is_streak_day": claim.is_streak_day,
                "total_credits": total,
                "error": "",
            })
        }
        Err(e) => result_json(account, "error", None, None, &e),
    }
}

fn run_all_inner() -> Value {
    let fetched_at = now_rfc3339();
    let accounts = match load_accounts() {
        Ok(a) => a,
        Err(e) => return json!({"ok": false, "error": e, "results": [], "fetched_at": fetched_at}),
    };
    let results: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<(&AccountRow, _)> = accounts
            .iter()
            .map(|account| (account, scope.spawn(move || process_account(account))))
            .collect();
        handles
            .into_iter()
            .map(|(account, handle)| {
                handle
                    .join()
                    .unwrap_or_else(|_| result_json(account, "error", None, None, "签到线程异常"))
            })
            .collect()
    });
    let summary = summarize(&results);
    crate::cabi::host_log("info", &format!("checkin batch: {summary}"));
    json!({"ok": true, "results": results, "summary": summary, "fetched_at": fetched_at})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::realm;
    use crate::rpc::{StoredAccount, StoredTokens};

    fn stored(uid: &str) -> StoredAuth {
        StoredAuth {
            auth: StoredTokens {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: 2_000_000_000,
                domain: "www.codebuddy.cn".into(),
            },
            account: StoredAccount {
                uid: uid.into(),
                enterprise_id: "ent".into(),
                nickname: "nick".into(),
            },
        }
    }

    fn row(uid: &str, file_name: &str, disabled: bool) -> AccountRow {
        AccountRow {
            auth_index: format!("idx-{file_name}"),
            file_name: file_name.into(),
            path: String::new(),
            nickname: "nick".into(),
            disabled,
            sa: Some(stored(uid)),
            load_error: String::new(),
        }
    }

    #[test]
    fn parse_status_reads_direct_data() {
        let data = serde_json::json!({
            "active": true,
            "today_checked_in": false,
            "streak_days": 17,
            "total_credits": 1700,
            "daily_credit": 100
        });
        assert_eq!(
            parse_status(&data),
            Status {
                active: true,
                today_checked_in: false,
                streak_days: 17,
                total_credits: 1700,
                daily_credit: 100,
            }
        );
    }

    #[test]
    fn parse_status_unwraps_nested_data() {
        let data = serde_json::json!({
            "data": {
                "active": true,
                "today_checked_in": true,
                "streak_days": 18,
                "total_credits": 1800,
                "daily_credit": 100
            }
        });
        let st = parse_status(&data);
        assert!(st.today_checked_in);
        assert_eq!(st.streak_days, 18);
        assert_eq!(st.total_credits, 1800);
    }

    #[test]
    fn parse_claim_reads_credit_and_streak() {
        let data = serde_json::json!({
            "credit": 100,
            "streak_days": 18,
            "is_streak_day": false
        });
        assert_eq!(
            parse_claim(&data),
            Claim {
                credit: 100,
                streak_days: 18,
                is_streak_day: false,
            }
        );
    }

    #[test]
    fn already_checked_in_matches_code_10001() {
        assert!(is_already_checked_in(&UpstreamError::Api(
            10001,
            "今日已签到".into()
        )));
        assert!(is_already_checked_in(&UpstreamError::HttpApi(
            400,
            10001,
            "今天已签到，请明天再来".into()
        )));
        assert!(!is_already_checked_in(&UpstreamError::Api(500, "x".into())));
        assert!(!is_already_checked_in(&UpstreamError::Http(401)));
    }

    #[test]
    fn refresh_is_due_when_expiring_within_a_minute() {
        assert!(needs_refresh(1060, 1000));
        assert!(needs_refresh(1000, 1000));
        assert!(!needs_refresh(1061, 1000));
    }

    #[test]
    fn realm_entry_filter_accepts_own_and_rejects_foreign() {
        let p = realm().provider;
        let t = realm().credential_type;
        assert!(is_realm_entry(p, "", "x.json"));
        assert!(is_realm_entry("", t, "x.json"));
        assert!(is_realm_entry("", "", &format!("{}abcdef.json", realm().credential_prefix)));
        let foreign = if p == "workbuddy" { "workbuddy-intl" } else { "workbuddy" };
        assert!(!is_realm_entry(foreign, "", &format!("{}abcdef.json", realm().credential_prefix)));
        assert!(!is_realm_entry("codex", "", "codex.json"));
        assert!(!is_realm_entry("", "", "qoderwork.json"));
    }

    #[test]
    fn dedupe_keeps_one_entry_per_uid_and_prefers_enabled() {
        let rows = vec![
            row("uid-1", "workbuddy-old.json", true),
            row("uid-1", "workbuddy-new.json", false),
            row("uid-2", "workbuddy-two.json", false),
        ];
        let out = dedupe_accounts(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].file_name, "workbuddy-new.json");
        assert_eq!(out[1].file_name, "workbuddy-two.json");
    }

    #[test]
    fn dedupe_keeps_distinct_uids() {
        let rows = vec![
            row("uid-1", "workbuddy-1.json", true),
            row("uid-2", "workbuddy-2.json", true),
        ];
        assert_eq!(dedupe_accounts(rows).len(), 2);
    }

    #[test]
    fn account_key_falls_back_to_file_name() {
        let mut empty = stored("");
        empty.account.uid.clear();
        assert_eq!(account_key(&empty, "workbuddy.json"), "file:workbuddy.json");
        assert_eq!(account_key(&stored("u1"), "x.json"), "uid:u1");
    }

    #[test]
    fn auth_errors_are_actionable() {
        assert!(status_error_message(&UpstreamError::Http(401)).contains("自动刷新"));
        assert!(
            status_error_message(&UpstreamError::HttpApi(401, 401, "expired".into()))
                .contains("自动刷新")
        );
        assert!(run_error_message(&UpstreamError::Http(401)).contains("重新登录"));
        assert!(
            run_error_message(&UpstreamError::HttpApi(401, 401, "expired".into()))
                .contains("重新登录")
        );
        assert!(retry_error_message(&UpstreamError::Http(401)).contains("仍被拒绝"));
    }

    #[test]
    fn parse_claim_unwraps_nested_data() {
        let data = serde_json::json!({
            "data": {"credit": 100, "streak_days": 19, "is_streak_day": true}
        });
        assert_eq!(
            parse_claim(&data),
            Claim {
                credit: 100,
                streak_days: 19,
                is_streak_day: true,
            }
        );
    }

    #[test]
    fn retries_once_after_401_with_refreshed_token() {
        let mut sa = stored("u");
        let mut calls = 0;
        let mut refreshes = 0;
        let refreshed = {
            let mut s = stored("u");
            s.auth.access_token = "new-at".into();
            s
        };
        let result = call_with_refresh_using(
            &mut sa,
            |s| {
                calls += 1;
                if calls == 1 {
                    Err(UpstreamError::Http(401))
                } else {
                    assert_eq!(s.auth.access_token, "new-at");
                    Ok("ok")
                }
            },
            |_| {
                refreshes += 1;
                Ok(refreshed.clone())
            },
        );
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls, 2);
        assert_eq!(refreshes, 1);
        assert_eq!(sa.auth.access_token, "new-at");
    }

    #[test]
    fn refresh_failure_is_reported() {
        let mut sa = stored("u");
        let result: Result<(), String> = call_with_refresh_using(
            &mut sa,
            |_| Err(UpstreamError::Http(401)),
            |_| Err("refresh down".into()),
        );
        assert!(result.unwrap_err().contains("自动刷新失败"));
    }

    #[test]
    fn process_account_reports_missing_credential() {
        let account = AccountRow {
            auth_index: "idx".into(),
            file_name: "workbuddy-x.json".into(),
            path: String::new(),
            nickname: "x".into(),
            disabled: false,
            sa: None,
            load_error: "读取凭据失败".into(),
        };
        let v = process_account(&account);
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("读取凭据失败"));
    }

    #[test]
    fn status_error_json_has_stable_null_fields() {
        let account = row("u", "workbuddy-test.json", false);
        let v = status_json(&account, None, "boom");
        assert_eq!(v["error"], "boom");
        assert!(v["active"].is_null());
        assert!(v["today_checked_in"].is_null());
        assert!(v["streak_days"].is_null());
        assert!(v["total_credits"].is_null());
        assert!(v["daily_credit"].is_null());
    }

    #[test]
    fn persisted_refresh_uses_host_path_and_rejects_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "workbuddy-refresh-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workbuddy-test.json");
        let mut account = row("u", "workbuddy-test.json", false);
        account.path = path.to_string_lossy().to_string();
        let cred = serde_json::json!({
            "account": {"uid": "u", "enterpriseId": "e", "nickname": "n"},
            "auth": {"accessToken": "new", "refreshToken": "newr", "expiresAt": 2, "domain": "www.codebuddy.cn"}
        });
        persist_refreshed_credential(&account, &cred).unwrap();
        assert!(path.exists());

        account.file_name = "workbuddy-other.json".into();
        assert!(persist_refreshed_credential(&account, &cred).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_counts_each_status() {
        let results = vec![
            serde_json::json!({"status": "success"}),
            serde_json::json!({"status": "already"}),
            serde_json::json!({"status": "inactive"}),
            serde_json::json!({"status": "error"}),
            serde_json::json!({"status": "unexpected"}),
        ];
        let s = summarize(&results);
        assert_eq!(s["success"], 1);
        assert_eq!(s["already"], 1);
        assert_eq!(s["inactive"], 1);
        assert_eq!(s["error"], 2);
    }

    fn live_cred() -> StoredAuth {
        let path = std::env::var("WORKBUDDY_LIVE_CRED")
            .expect("set WORKBUDDY_LIVE_CRED to a workbuddy credential json");
        let raw = std::fs::read(path).expect("read credential");
        serde_json::from_slice(&raw).expect("parse credential")
    }

    #[test]
    #[ignore]
    fn live_status_from_env() {
        let st = fetch_status(&live_cred()).expect("checkin status");
        println!("{st:?}");
        assert!(st.active, "check-in activity should be active");
        assert!(st.daily_credit > 0, "daily credit should be positive");
        assert!(st.streak_days >= 0);
        assert!(st.total_credits >= 0);
    }

    #[test]
    #[ignore]
    fn live_claim_from_env() {
        match fetch_claim(&live_cred()) {
            Ok(c) => {
                println!("claimed: {c:?}");
                assert!(c.credit >= 0);
            }
            Err(e) if is_already_checked_in(&e) => println!("already checked in"),
            Err(e) => panic!("claim failed: {e}"),
        }
    }
}
