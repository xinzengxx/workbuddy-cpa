use crate::rpc::{
    auth_data_from_stored, AccountData, AuthLoginStartResponse, AuthLoginPollResponse,
    AuthParseResponse, AuthRefreshResponse, StoredAccount, StoredAuth, StoredTokens, TokenData,
};
use crate::upstream::{
    common_header_set, get_envelope, new_login_agent, post_envelope, shared_agent, HeaderSet,
    ENDPOINT_AUTH_STATE, ENDPOINT_AUTH_TOKEN, ENDPOINT_LOGIN_ACCT, ENDPOINT_TOKEN_REFRESH,
    CLIENT_UA, ORIGIN, UPSTREAM_BASE,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const LOGIN_TTL_SECS: u64 = 300;

/// One in-flight browser login. CodeBuddy associates the login with the state
/// issued at auth/state, so the same cookie jar (Arc'd inside the Agent,
/// Clone shares it) must be used for the state request and all polls.
struct LoginCtx {
    agent: ureq::Agent,
    expires_at: u64,
}

static LOGIN_STATES: Mutex<Option<HashMap<String, LoginCtx>>> = Mutex::new(None);

fn with_states<F, R>(f: F) -> R
where
    F: FnOnce(&mut HashMap<String, LoginCtx>) -> R,
{
    let mut guard = LOGIN_STATES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rfc3339_now_plus(secs: u64) -> String {
    let now = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs as i64);
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| format!("{now}"))
}

/// POST /v2/plugin/auth/state: returns the QR login URL + state to poll.
pub fn start_login() -> Result<AuthLoginStartResponse, String> {
    let agent = new_login_agent();
    let data = post_envelope(&agent, ENDPOINT_AUTH_STATE, common_header_set(), "{}")
        .map_err(|e| format!("auth state failed: {e}"))?;
    let state = data["state"].as_str().unwrap_or("").to_string();
    let auth_url = data["authUrl"].as_str().unwrap_or("").to_string();
    if state.is_empty() || auth_url.is_empty() {
        return Err("auth state: missing state or authUrl".into());
    }
    let expires_at = now_unix() + LOGIN_TTL_SECS;
    with_states(|m| {
        m.insert(state.clone(), LoginCtx { agent, expires_at });
    });
    Ok(AuthLoginStartResponse {
        provider: "workbuddy".into(),
        url: auth_url,
        state,
        expires_at: rfc3339_now_plus(LOGIN_TTL_SECS),
    })
}

/// GET /v2/plugin/auth/token?state= returns code 11217 ("login ing") while
/// the user has not scanned; code 0 with the token bundle once complete.
/// login/account sits behind the gateway and 401s until login finishes, so
/// probe token first and only fetch the account once we hold a bearer.
pub fn poll_login(state: &str) -> Result<AuthLoginPollResponse, String> {
    let expired = with_states(|m| match m.get(state) {
        None => None,
        Some(ctx) => Some(now_unix() > ctx.expires_at),
    });
    match expired {
        None => return Err("poll: unknown state (restart login)".into()),
        Some(true) => {
            with_states(|m| {
                m.remove(state);
            });
            return Err("poll: login expired".into());
        }
        Some(false) => {}
    }

    // Single-shot poll per RPC: the host drives the cadence.
    let token_data = match poll_token_once(state) {
        Ok(d) => d,
        Err(_) => {
            return Ok(AuthLoginPollResponse {
                status: "pending",
                message: "waiting for login".into(),
                auth: None,
            })
        }
    };
    if token_data.access_token.is_empty() {
        return Ok(AuthLoginPollResponse {
            status: "pending",
            message: "waiting for login".into(),
            auth: None,
        });
    }

    let mut account = AccountData::default();
    if let Ok(acct) = fetch_account_once(state, &token_data.access_token) {
        account = acct;
    }

    let sa = StoredAuth {
        auth: StoredTokens {
            access_token: token_data.access_token,
            refresh_token: token_data.refresh_token,
            expires_at: (now_unix() as i64) + token_data.expires_in,
            domain: token_data.domain,
        },
        account: StoredAccount {
            uid: account.uid,
            enterprise_id: account.enterprise_id,
            nickname: account.nickname,
        },
    };
    with_states(|m| {
        m.remove(state);
    });
    Ok(AuthLoginPollResponse {
        status: "success",
        message: String::new(),
        auth: Some(auth_data_from_stored(&sa, &crate::rpc::credential_file_name(&sa))),
    })
}

fn poll_token_once(state: &str) -> Result<TokenData, String> {
    let agent = state_agent(state)?;
    let data = get_envelope(
        &agent,
        &format!("{ENDPOINT_AUTH_TOKEN}{state}"),
        common_header_set(),
    )
    .map_err(|e| e.to_string())?;
    serde_json::from_value(data).map_err(|e| format!("token parse: {e}"))
}

fn fetch_account_once(state: &str, bearer: &str) -> Result<AccountData, String> {
    let agent = state_agent(state)?;
    let hs = HeaderSet(vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json".into()),
        ("User-Agent".into(), CLIENT_UA.into()),
        ("Origin".into(), ORIGIN.into()),
        ("Referer".into(), format!("{}/", ORIGIN)),
        ("Authorization".into(), format!("Bearer {bearer}")),
    ]);
    let data = get_envelope(
        &agent,
        &format!("{ENDPOINT_LOGIN_ACCT}{state}"),
        hs,
    )
    .map_err(|e| e.to_string())?;
    serde_json::from_value(data).map_err(|e| format!("account parse: {e}"))
}

fn state_agent(state: &str) -> Result<ureq::Agent, String> {
    // Agent Clone shares the same Arc'd cookie jar, so polls hit the gateway
    // with exactly the cookies the state request set.
    let h = LOGIN_STATES.lock().unwrap_or_else(|e| e.into_inner());
    let map = h.as_ref().ok_or("poll: no login states")?;
    let ctx = map.get(state).ok_or("poll: unknown state (restart login)")?;
    Ok(ctx.agent.clone())
}

/// auth.parse: recognize workbuddy credentials among auth files on disk.
pub fn parse_auth(raw_json_b64: &str) -> AuthParseResponse {
    let raw = crate::rpc::b64_decode(raw_json_b64);
    if raw.is_empty() {
        return AuthParseResponse { handled: false, auth: None };
    }
    let sa: StoredAuth = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return AuthParseResponse { handled: false, auth: None },
    };
    if sa.auth.access_token.is_empty() {
        return AuthParseResponse { handled: false, auth: None };
    }
    AuthParseResponse { handled: true, auth: Some(auth_data_from_stored(&sa, "workbuddy.json")) }
}

/// POST /v2/plugin/auth/token/refresh with the X-Auth-Refresh-Source marker.
pub fn refresh(storage_json_b64: &str) -> Result<AuthRefreshResponse, String> {
    let raw = crate::rpc::b64_decode(storage_json_b64);
    let mut sa: StoredAuth =
        serde_json::from_slice(&raw).map_err(|e| format!("refresh: storage_parse_error: {e}"))?;
    let mut hs = common_header_set();
    hs.0.push(("X-Refresh-Token".into(), sa.auth.refresh_token.clone()));
    if !sa.account.enterprise_id.is_empty() {
        hs.0.push(("X-Enterprise-Id".into(), sa.account.enterprise_id.clone()));
    }
    hs.0.push(("X-Auth-Refresh-Source".into(), "workbuddy".into()));
    let data = post_envelope(shared_agent(), ENDPOINT_TOKEN_REFRESH, hs, "{}")
        .map_err(|e| format!("refresh: {e}"))?;
    let tok: TokenData = serde_json::from_value(data).map_err(|e| format!("refresh: parse: {e}"))?;
    if tok.access_token.is_empty() {
        return Err("refresh_failed: no accessToken".into());
    }
    sa.auth.access_token = tok.access_token;
    if !tok.refresh_token.is_empty() {
        sa.auth.refresh_token = tok.refresh_token;
    }
    if !tok.domain.is_empty() {
        sa.auth.domain = tok.domain;
    }
    sa.auth.expires_at = (now_unix() as i64) + tok.expires_in;
    Ok(AuthRefreshResponse { auth: auth_data_from_stored(&sa, &crate::rpc::credential_file_name(&sa)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::b64_encode;

    #[test]
    fn poll_unknown_state_errors() {
        assert!(poll_login("nope").is_err());
    }

    #[test]
    fn parse_auth_rejects_garbage() {
        assert_eq!(parse_auth(&b64_encode(b"not json")).handled, false);
    }

    #[test]
    fn parse_auth_accepts_stored() {
        let sa = StoredAuth {
            auth: StoredTokens {
                access_token: "tok".into(),
                refresh_token: "r".into(),
                expires_at: 99,
                domain: "cn".into(),
            },
            account: StoredAccount {
                uid: "u".into(),
                enterprise_id: "e".into(),
                nickname: "n".into(),
            },
        };
        let resp = parse_auth(&b64_encode(&serde_json::to_vec(&sa).unwrap()));
        assert_eq!(resp.handled, true);
        let ad = resp.auth.unwrap();
        let back: StoredAuth =
            serde_json::from_slice(&crate::rpc::b64_decode(&ad.storage_json)).unwrap();
        assert_eq!(back.auth.access_token, "tok");
    }

    #[test]
    fn rfc3339_format_is_parseable() {
        let s = rfc3339_now_plus(300);
        assert!(
            time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).is_ok()
        );
    }
}
