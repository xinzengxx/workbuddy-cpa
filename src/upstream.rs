use crate::rpc::StoredAuth;
use std::sync::OnceLock;
use std::time::Duration;

/// Upstream API base for this realm (CN: copilot.tencent.com, Global:
/// www.workbuddy.ai). A Global JWT against the CN gateway is a 401, so the two
/// realms must never share one base.
pub fn base() -> &'static str {
    crate::realm::realm().base
}

/// Web origin for this realm, used for the Origin/Referer headers.
pub fn origin() -> &'static str {
    crate::realm::realm().origin
}

/// CLI build advertised in the channel-verification headers. The Global gateway
/// compares this string, so User-Agent and X-IDE-Version must stay in lockstep.
pub const IDE_VERSION: &str = "2.63.2";

pub fn client_ua() -> String {
    format!("CLI/{IDE_VERSION} CodeBuddy/{IDE_VERSION}")
}

/// 32 lowercase hex chars, fresh per request.
fn random_hex16() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the Global gateway calls "channel verification": without these the chat
/// endpoint answers 11128 "request illegal". The CN gateway tolerates them, so
/// both realms send the same set (same as the Go upstream).
pub fn channel_header_set() -> HeaderSet {
    HeaderSet(vec![
        ("X-IDE-Type".into(), "CLI".into()),
        ("X-IDE-Name".into(), "CLI".into()),
        ("X-IDE-Version".into(), IDE_VERSION.into()),
        ("X-Agent-Intent".into(), "craft".into()),
        ("X-Request-ID".into(), random_hex16()),
        ("X-Conversation-ID".into(), random_hex16()),
        ("X-Conversation-Request-ID".into(), random_hex16()),
        ("X-Conversation-Message-ID".into(), random_hex16()),
    ])
}

pub const ENDPOINT_AUTH_STATE: &str = "/v2/plugin/auth/state?platform=CLI";
pub const ENDPOINT_LOGIN_ACCT: &str = "/v2/plugin/login/account?state=";
pub const ENDPOINT_AUTH_TOKEN: &str = "/v2/plugin/auth/token?state=";
pub const ENDPOINT_TOKEN_REFRESH: &str = "/v2/plugin/auth/token/refresh";
pub const ENDPOINT_CHAT: &str = "/v2/chat/completions";
pub const ENDPOINT_BILLING_USER_RESOURCE: &str = "/v2/billing/meter/get-user-resource";
pub const ENDPOINT_CHECKIN_STATUS: &str = "/v2/billing/meter/checkin-activity-status";
pub const ENDPOINT_CHECKIN_DAILY: &str = "/v2/billing/meter/daily-checkin";

#[derive(Debug)]
pub enum UpstreamError {
    Http(u16),
    HttpApi(u16, i64, String),
    Api(i64, String),
    Transport(String),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Http(s) => write!(f, "http_error: upstream {s}"),
            UpstreamError::HttpApi(s, c, m) => {
                write!(f, "http_error: upstream {s} code={c} msg={m}")
            }
            UpstreamError::Api(c, m) => write!(f, "code={c} msg={m}"),
            UpstreamError::Transport(e) => write!(f, "transport: {e}"),
        }
    }
}

fn error_for(status: u16, code: i64, msg: &str) -> UpstreamError {
    if status >= 400 && code != 0 {
        UpstreamError::HttpApi(status, code, msg.to_string())
    } else if status >= 400 {
        UpstreamError::Http(status)
    } else {
        UpstreamError::Api(code, msg.to_string())
    }
}

fn decode_error_response(status: u16, raw: &str) -> UpstreamError {
    match decode_envelope(status, raw) {
        Err(e) => e,
        Ok(_) => UpstreamError::Http(status),
    }
}

fn decode_envelope(status: u16, raw: &str) -> Result<serde_json::Value, UpstreamError> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        if status >= 400 {
            UpstreamError::Http(status)
        } else {
            UpstreamError::Transport(format!("parse: {e}"))
        }
    })?;
    let code = v["code"].as_i64().unwrap_or(0);
    let msg = v["msg"].as_str().unwrap_or("");
    if status >= 400 || code != 0 {
        return Err(error_for(status, code, msg));
    }
    Ok(v["data"].clone())
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
    // IMPORTANT: no agent-level overall `.timeout()`. In ureq 2.x that caps
    // the WHOLE request including reading the response body, which severed
    // chat SSE streams at 120s mid-generation (the "deep thinking dies at
    // ~2 minutes" bug). Bounds are instead: connect timeout + per-read idle
    // timeout, so a stream lives as long as upstream keeps sending data.
    Ok(ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IDLE_READ_TIMEOUT)
        .tls_config(std::sync::Arc::new(tls))
        .cookie_store(store)
        .build())
}

/// Fail fast on unreachable upstreams (default 30s, made explicit).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-read idle timeout: a healthy SSE stream emits chunks far more often
/// than this, so only a truly stalled upstream trips it (self-heals instead
/// of blocking the pump thread forever).
pub const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(600);
/// Overall cap for the small JSON envelope endpoints (auth state, login
/// polling, token refresh, billing). Chat streaming deliberately gets NO
/// overall timeout — see build_agent.
pub const ENVELOPE_TIMEOUT: Duration = Duration::from_secs(60);

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
        ("Origin".into(), origin().into()),
        ("Referer".into(), format!("{}/", origin())),
        ("User-Agent".into(), client_ua()),
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
    hs.0.extend(channel_header_set().0);
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
    post_envelope_with_timeout(agent, path, headers, body, ENVELOPE_TIMEOUT)
}

/// Same as post_envelope, with an explicit per-request timeout. Small
/// endpoints (check-in) use a shorter bound so a slow account cannot hold a
/// management request for the full envelope timeout.
pub fn post_envelope_with_timeout(
    agent: &ureq::Agent,
    path: &str,
    headers: HeaderSet,
    body: &str,
    timeout: Duration,
) -> Result<serde_json::Value, UpstreamError> {
    let url = format!("{}{path}", base());
    let req = headers.apply_to(agent.post(&url).timeout(timeout));
    let resp = match req.send_string(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(status, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            return Err(decode_error_response(status, &raw));
        }
        Err(other) => return Err(UpstreamError::Transport(other.to_string())),
    };
    let status = resp.status();
    let raw = resp.into_string().map_err(|e| UpstreamError::Transport(e.to_string()))?;
    decode_envelope(status, &raw)
}

/// GET with headers, same envelope decoding as post_envelope.
pub fn get_envelope(
    agent: &ureq::Agent,
    path: &str,
    headers: HeaderSet,
) -> Result<serde_json::Value, UpstreamError> {
    let url = format!("{}{path}", base());
    let req = headers.apply_to(agent.get(&url).timeout(ENVELOPE_TIMEOUT));
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(status, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            return Err(decode_error_response(status, &raw));
        }
        Err(other) => return Err(UpstreamError::Transport(other.to_string())),
    };
    let status = resp.status();
    let raw = resp.into_string().map_err(|e| UpstreamError::Transport(e.to_string()))?;
    decode_envelope(status, &raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::realm;
    use crate::rpc::{StoredAccount, StoredTokens};

    #[test]
    fn backend_headers_carry_channel_verification() {
        let hs = backend_header_set(&empty_account_stored_auth());
        let get = |k: &str| hs.0.iter().find(|(hk, _)| hk == k).map(|(_, v)| v.clone());
        assert_eq!(get("X-IDE-Type").as_deref(), Some("CLI"));
        assert_eq!(get("X-IDE-Name").as_deref(), Some("CLI"));
        assert_eq!(get("X-IDE-Version").as_deref(), Some(IDE_VERSION));
        assert_eq!(get("X-Agent-Intent").as_deref(), Some("craft"));
        for k in [
            "X-Request-ID",
            "X-Conversation-ID",
            "X-Conversation-Request-ID",
            "X-Conversation-Message-ID",
        ] {
            let v = get(k).unwrap_or_else(|| panic!("missing {k}"));
            assert_eq!(v.len(), 32, "{k} must be 32 chars, got {v}");
            assert!(
                v.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{k} must be lowercase hex"
            );
        }
    }

    #[test]
    fn channel_request_ids_are_fresh_per_call() {
        let rid = |hs: &HeaderSet| {
            hs.0.iter()
                .find(|(k, _)| k == "X-Request-ID")
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_ne!(
            rid(&backend_header_set(&empty_account_stored_auth())),
            rid(&backend_header_set(&empty_account_stored_auth()))
        );
    }

    #[test]
    fn base_and_origin_follow_realm() {
        assert_eq!(base(), realm().base);
        assert_eq!(origin(), realm().origin);
    }

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
        assert!(hs.0.iter().any(|(k, v)| k == "User-Agent" && v == &client_ua()));
        assert!(hs.0.iter().any(|(k, v)| k == "Origin" && v == origin()));
    }

    #[test]
    fn error_for_keeps_http_status_and_api_code() {
        match error_for(400, 10001, "already") {
            UpstreamError::HttpApi(400, 10001, msg) => assert_eq!(msg, "already"),
            other => panic!("expected HttpApi, got {other:?}"),
        }
        assert!(matches!(
            error_for(500, 0, ""),
            UpstreamError::Http(500)
        ));
        match error_for(200, 500, "boom") {
            UpstreamError::Api(500, msg) => assert_eq!(msg, "boom"),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn status_error_body_preserves_code_10001() {
        match decode_error_response(400, r#"{"code":10001,"msg":"今天已签到"}"#) {
            UpstreamError::HttpApi(400, 10001, msg) => assert_eq!(msg, "今天已签到"),
            other => panic!("expected HttpApi, got {other:?}"),
        }
        assert!(matches!(
            decode_error_response(500, "not json"),
            UpstreamError::Http(500)
        ));
    }

    #[test]
    fn decode_envelope_returns_data_on_success() {
        let v = decode_envelope(200, r#"{"code":0,"msg":"OK","data":{"x":1}}"#).unwrap();
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn checkin_endpoints_use_v2_billing_paths() {
        assert_eq!(
            ENDPOINT_CHECKIN_STATUS,
            "/v2/billing/meter/checkin-activity-status"
        );
        assert_eq!(ENDPOINT_CHECKIN_DAILY, "/v2/billing/meter/daily-checkin");
    }

    /// Pins ureq 2.x semantics that justify the agent config: an overall
    /// `.timeout()` caps the whole request INCLUDING reading the body, so it
    /// kills a slow-but-flowing SSE stream mid-flight. Never add one to the
    /// shared agent again.
    #[test]
    fn agent_overall_timeout_cuts_midstream_reads() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for i in 0..8 {
                if conn
                    .write_all(format!("data: {i}\n\n").as_bytes())
                    .and_then(|_| conn.flush())
                    .is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });

        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(1)).build();
        let started = Instant::now();
        let resp = agent.post(&format!("http://{addr}/")).send_string("").unwrap();
        let mut n = 0;
        for line in std::io::BufReader::new(resp.into_reader()).lines().map_while(Result::ok) {
            if line.starts_with("data:") {
                n += 1;
            }
        }
        let took = started.elapsed();
        assert!(
            n < 8,
            "overall timeout must cut a slow-but-flowing stream mid-flight, got all {n} chunks"
        );
        assert!(took < Duration::from_millis(2300), "died too late: {took:?}");
    }

    /// The new agent shape (connect + per-read idle timeout, no overall cap)
    /// lets a stream outlive any wall-clock bound as long as data keeps flowing.
    #[test]
    fn connect_and_idle_timeouts_allow_long_flowing_streams() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for i in 0..6 {
                if conn
                    .write_all(format!("data: {i}\n\n").as_bytes())
                    .and_then(|_| conn.flush())
                    .is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IDLE_READ_TIMEOUT)
            .build();
        let resp = agent.post(&format!("http://{addr}/")).send_string("").unwrap();
        let n = std::io::BufReader::new(resp.into_reader())
            .lines()
            .map_while(Result::ok)
            .filter(|l| l.starts_with("data:"))
            .count();
        assert_eq!(n, 6, "idle-read timeout must not cut a flowing stream");
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
            Err(UpstreamError::HttpApi(_, _, _)) => {}
            Err(UpstreamError::Api(_, _)) => {}
            Err(other) => panic!("tls smoke failed: {other}"),
        }
    }
}
