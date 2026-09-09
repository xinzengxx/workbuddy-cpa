use crate::rpc::StoredAuth;
use std::sync::OnceLock;

pub const UPSTREAM_BASE: &str = "https://copilot.tencent.com";
pub const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
pub const ORIGIN: &str = "https://www.codebuddy.cn";
pub const REFERER: &str = "https://www.codebuddy.cn/";

pub const ENDPOINT_AUTH_STATE: &str = "/v2/plugin/auth/state?platform=CLI";
pub const ENDPOINT_LOGIN_ACCT: &str = "/v2/plugin/login/account?state=";
pub const ENDPOINT_AUTH_TOKEN: &str = "/v2/plugin/auth/token?state=";
pub const ENDPOINT_TOKEN_REFRESH: &str = "/v2/plugin/auth/token/refresh";
pub const ENDPOINT_CHAT: &str = "/v2/chat/completions";
pub const ENDPOINT_BILLING_USER_RESOURCE: &str = "/v2/billing/meter/get-user-resource";

#[derive(Debug)]
pub enum UpstreamError {
    Http(u16),
    Api(i64, String),
    Transport(String),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Http(s) => write!(f, "http_error: upstream {s}"),
            UpstreamError::Api(c, m) => write!(f, "code={c} msg={m}"),
            UpstreamError::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

fn tls_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs()
        .map_err(|e| format!("load native certs: {e}"))?;
    for c in certs {
        let _ = roots.add(c);
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Shared agent with a cookie jar; used for chat, refresh and billing.
pub fn shared_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| build_agent().expect("build shared agent"))
}

/// Isolated agent with its own cookie jar so one browser login flow never
/// leaks cookies into another.
pub fn new_login_agent() -> ureq::Agent {
    build_agent().expect("build login agent")
}

fn build_agent() -> Result<ureq::Agent, String> {
    let tls = tls_config()?;
    let store = cookie_store::CookieStore::new(None);
    Ok(ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .tls_config(std::sync::Arc::new(tls))
        .cookie_store(store)
        .build())
}

/// Ordered header bag so tests can assert exact X-No-* branching.
pub struct HeaderSet(pub Vec<(String, String)>);

impl HeaderSet {
    pub fn apply_to(&self, req: ureq::Request) -> ureq::Request {
        let mut r = req;
        for (k, v) in &self.0 {
            r = r.set(k, v);
        }
        r
    }
}

pub fn common_header_set() -> HeaderSet {
    HeaderSet(vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json, text/plain, */*".into()),
        ("X-Requested-With".into(), "XMLHttpRequest".into()),
        ("Origin".into(), ORIGIN.into()),
        ("Referer".into(), format!("{ORIGIN}/")),
        ("User-Agent".into(), CLIENT_UA.into()),
    ])
}

/// Common headers plus auth-derived ones. Empty credential fields are
/// signalled via the X-No-* convention CodeBuddy expects.
pub fn backend_header_set(sa: &StoredAuth) -> HeaderSet {
    let mut hs = common_header_set();
    if !sa.auth.access_token.is_empty() {
        hs.0.push(("Authorization".into(), format!("Bearer {}", sa.auth.access_token)));
    } else {
        hs.0.push(("X-No-Authorization".into(), "1".into()));
    }
    if !sa.account.uid.is_empty() {
        hs.0.push(("X-User-Id".into(), sa.account.uid.clone()));
    } else {
        hs.0.push(("X-No-User-Id".into(), "1".into()));
    }
    if !sa.account.enterprise_id.is_empty() {
        hs.0.push(("X-Enterprise-Id".into(), sa.account.enterprise_id.clone()));
    } else {
        hs.0.push(("X-No-Enterprise-Id".into(), "1".into()));
    }
    if !sa.auth.refresh_token.is_empty() {
        hs.0.push(("X-Refresh-Token".into(), sa.auth.refresh_token.clone()));
    }
    if !sa.auth.domain.is_empty() {
        hs.0.push(("X-Domain".into(), sa.auth.domain.clone()));
    } else {
        hs.0.push(("X-No-Department-Info".into(), "1".into()));
    }
    hs.0.push(("X-Product".into(), "SaaS".into()));
    hs
}

/// POST `{}` with the given headers and decode the {code,msg,data} envelope.
/// Returns the inner `data` value. HTTP >= 400 and code != 0 are errors.
pub fn post_envelope(
    agent: &ureq::Agent,
    path: &str,
    headers: HeaderSet,
    body: &str,
) -> Result<serde_json::Value, UpstreamError> {
    let url = format!("{UPSTREAM_BASE}{path}");
    let req = headers.apply_to(agent.post(&url));
    let resp = req.send_string(body).map_err(|e| match e {
        ureq::Error::Status(s, _) => UpstreamError::Http(s),
        other => UpstreamError::Transport(other.to_string()),
    })?;
    let status = resp.status();
    let raw = resp.into_string().map_err(|e| UpstreamError::Transport(e.to_string()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| UpstreamError::Transport(format!("parse: {e}")))?;
    if status >= 400 {
        return Err(UpstreamError::Http(status));
    }
    let code = v["code"].as_i64().unwrap_or(0);
    if code != 0 {
        return Err(UpstreamError::Api(code, v["msg"].as_str().unwrap_or("").into()));
    }
    Ok(v["data"].clone())
}

/// GET with headers, same envelope decoding as post_envelope.
pub fn get_envelope(
    agent: &ureq::Agent,
    path: &str,
    headers: HeaderSet,
) -> Result<serde_json::Value, UpstreamError> {
    let url = format!("{UPSTREAM_BASE}{path}");
    let req = headers.apply_to(agent.get(&url));
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(s, _) => UpstreamError::Http(s),
        other => UpstreamError::Transport(other.to_string()),
    })?;
    let status = resp.status();
    let raw = resp.into_string().map_err(|e| UpstreamError::Transport(e.to_string()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| UpstreamError::Transport(format!("parse: {e}")))?;
    if status >= 400 {
        return Err(UpstreamError::Http(status));
    }
    let code = v["code"].as_i64().unwrap_or(0);
    if code != 0 {
        return Err(UpstreamError::Api(code, v["msg"].as_str().unwrap_or("").into()));
    }
    Ok(v["data"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{StoredAccount, StoredTokens};

    fn empty_account_stored_auth() -> StoredAuth {
        StoredAuth {
            auth: StoredTokens {
                access_token: "testtoken".into(),
                refresh_token: "r".into(),
                expires_at: 1,
                domain: "".into(),
            },
            account: StoredAccount {
                uid: "".into(),
                enterprise_id: "".into(),
                nickname: "".into(),
            },
        }
    }

    #[test]
    fn backend_headers_empty_fields_use_no_headers() {
        let hs = backend_header_set(&empty_account_stored_auth());
        assert!(hs.0.iter().any(|(k, v)| k == "X-No-User-Id" && v == "1"));
        assert!(hs.0.iter().any(|(k, v)| k == "Authorization" && v == "Bearer testtoken"));
        assert!(!hs.0.iter().any(|(k, _)| k == "X-User-Id"));
        assert!(hs.0.iter().any(|(k, v)| k == "X-No-Enterprise-Id" && v == "1"));
        assert!(hs.0.iter().any(|(k, v)| k == "X-Product" && v == "SaaS"));
    }

    #[test]
    fn backend_headers_full_fields_set_all() {
        let sa = StoredAuth {
            auth: StoredTokens {
                access_token: "t".into(),
                refresh_token: "rt".into(),
                expires_at: 1,
                domain: "cn".into(),
            },
            account: StoredAccount {
                uid: "u1".into(),
                enterprise_id: "ent1".into(),
                nickname: "nick".into(),
            },
        };
        let hs = backend_header_set(&sa);
        assert!(hs.0.iter().any(|(k, v)| k == "X-User-Id" && v == "u1"));
        assert!(hs.0.iter().any(|(k, v)| k == "X-Enterprise-Id" && v == "ent1"));
        assert!(hs.0.iter().any(|(k, v)| k == "X-Refresh-Token" && v == "rt"));
        assert!(hs.0.iter().any(|(k, v)| k == "X-Domain" && v == "cn"));
        assert!(!hs.0.iter().any(|(k, _)| k == "X-No-Authorization"));
    }

    #[test]
    fn common_headers_carry_ua_and_origin() {
        let hs = common_header_set();
        assert!(hs.0.iter().any(|(k, v)| k == "User-Agent" && v == CLIENT_UA));
        assert!(hs.0.iter().any(|(k, v)| k == "Origin" && v == ORIGIN));
    }

    /// Real-network smoke test: must reach the upstream over TLS. Any HTTP
    /// status is fine; a TLS handshake failure means root certs are broken.
    #[test]
    #[ignore]
    fn tls_smoke() {
        let agent = shared_agent();
        let result = get_envelope(agent, ENDPOINT_AUTH_STATE, common_header_set());
        match result {
            Ok(_) => {}
            Err(UpstreamError::Http(_)) => {}
            Err(UpstreamError::Api(_, _)) => {}
            Err(other) => panic!("tls smoke failed: {other}"),
        }
    }
}
