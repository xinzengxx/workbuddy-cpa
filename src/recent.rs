use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Size of the "recent calls" ring shown in the panel.
pub const CAP: usize = 10;

/// One completed chat call as observed by the executor. In-memory only: the
/// buffer lives in the plugin (host process) and resets when CPA restarts.
/// CodeBuddy exposes no per-call credit breakdown, so consumption is tracked
/// as token usage; account-level credits stay in the billing zones.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub ts: u64,
    pub auth_id: String,
    pub nickname: String,
    pub model: String,
    pub stream: bool,
    pub ok: bool,
    pub error: String,
    pub duration_ms: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

static RING: Mutex<VecDeque<Call>> = Mutex::new(VecDeque::new());

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn record(call: Call) {
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    guard.push_back(call);
    while guard.len() > CAP {
        guard.pop_front();
    }
}

/// Newest first (index 0 = most recent call).
pub fn snapshot() -> Vec<Call> {
    let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    guard.iter().rev().cloned().collect()
}

/// Record one chat call. `usage` is the upstream usage object
/// ({"prompt_tokens":..,"completion_tokens":..,"total_tokens":..}) when known.
pub fn record_call(
    auth_id: &str,
    nickname: &str,
    model: &str,
    stream: bool,
    ok: bool,
    error: &str,
    duration_ms: u64,
    usage: Option<&Value>,
) {
    let (prompt, completion, total) = usage
        .map(|u| {
            (
                u["prompt_tokens"].as_i64().unwrap_or(0),
                u["completion_tokens"].as_i64().unwrap_or(0),
                u["total_tokens"].as_i64().unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));
    record(Call {
        ts: now(),
        auth_id: auth_id.to_string(),
        nickname: nickname.to_string(),
        model: model.to_string(),
        stream,
        ok,
        error: error.to_string(),
        duration_ms,
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    });
}

/// Pull the usage object from a finished aggregate chat.completion payload.
pub fn usage_from_completion(payload: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    if v["usage"].is_object() {
        Some(v["usage"].clone())
    } else {
        None
    }
}

/// Scan cleaned SSE chunk JSON strings (newest last) for the final usage
/// object. Streaming upstreams carry usage on a late chunk.
pub fn usage_from_chunks(chunks: &[String]) -> Option<Value> {
    for raw in chunks.iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        if v["usage"].is_object() && v["usage"]["total_tokens"].is_i64() {
            return Some(v["usage"].clone());
        }
    }
    None
}

/// Response body for GET /v0/management/plugins/workbuddy/recent.
pub fn json() -> Value {
    let calls: Vec<Value> = snapshot()
        .iter()
        .map(|c| {
            serde_json::json!({
                "ts": c.ts,
                "auth_id": c.auth_id,
                "nickname": c.nickname,
                "model": c.model,
                "stream": c.stream,
                "ok": c.ok,
                "error": c.error,
                "duration_ms": c.duration_ms,
                "prompt_tokens": c.prompt_tokens,
                "completion_tokens": c.completion_tokens,
                "total_tokens": c.total_tokens,
            })
        })
        .collect();
    serde_json::json!({ "calls": calls })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(model: &str, ok: bool) -> Call {
        Call {
            ts: now(),
            auth_id: "workbuddy-7b80b5".into(),
            nickname: "心远".into(),
            model: model.into(),
            stream: false,
            ok,
            error: if ok { String::new() } else { "upstream 400".into() },
            duration_ms: 1200,
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        }
    }

    #[test]
    fn ring_keeps_newest_ten() {
        for i in 0..13 {
            record(call(&format!("m{i}"), true));
        }
        let snap = snapshot();
        assert_eq!(snap.len(), CAP);
        assert_eq!(snap[0].model, "m12", "newest first");
        assert_eq!(snap[9].model, "m3");
    }

    #[test]
    fn record_call_extracts_usage() {
        record_call("a", "n", "glm-5.3-flash", true, true, "", 5, Some(&serde_json::json!({"total_tokens": 77, "prompt_tokens": 7, "completion_tokens": 70})));
        let snap = snapshot();
        assert_eq!(snap[0].total_tokens, 77);
        assert!(snap[0].stream);
    }

    #[test]
    fn usage_from_completion_parses() {
        let body = serde_json::to_vec(&serde_json::json!({"id":"c1","usage":{"total_tokens":5}})).unwrap();
        assert_eq!(usage_from_completion(&body).unwrap()["total_tokens"], 5);
        assert!(usage_from_completion(b"not json").is_none());
    }

    #[test]
    fn usage_from_chunks_takes_last_with_usage() {
        let chunks = vec![
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#.to_string(),
            r#"{"usage":{"total_tokens":11}}"#.to_string(),
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#.to_string(),
            r#"{"usage":{"total_tokens":42}}"#.to_string(),
        ];
        assert_eq!(usage_from_chunks(&chunks).unwrap()["total_tokens"], 42);
        assert!(usage_from_chunks(&[]).is_none());
        assert!(usage_from_chunks(&["junk".to_string()]).is_none());
    }

    #[test]
    fn json_shape() {
        record(call("glm-5.3-flash", false));
        let v = json();
        let first = v["calls"][0].clone();
        assert_eq!(first["model"], "glm-5.3-flash");
        assert_eq!(first["ok"], false);
        assert_eq!(first["error"], "upstream 400");
        assert!(first["total_tokens"].is_i64());
    }
}
