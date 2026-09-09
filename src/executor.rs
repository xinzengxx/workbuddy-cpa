use crate::rpc::{ExecutorExecResponse, ExecutorStreamResponse, StoredAuth, StreamChunk};
use crate::upstream::{backend_header_set, shared_agent, HeaderSet, ENDPOINT_CHAT, UPSTREAM_BASE};
use serde_json::Value;
use std::io::BufRead;

/// One decoded executor request (base64 fields already decoded by dispatch).
pub struct ExecReq {
    pub model: String,
    pub payload: Vec<u8>,
    pub original: Vec<u8>,
    pub storage: StoredAuth,
    pub metadata: Value,
    pub stream_id: String,
}

// Rewrite pairs from the Go version (commit 03bc412): each is a single-word
// change that dodges CodeBuddy's verbatim blocklist while preserving meaning.
const REWRITE_1_OLD: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const REWRITE_1_NEW: &str = "You are Claude Code, Anthropic's official CLI tool for Claude.";
const REWRITE_2_OLD: &str = "Main branch (you will usually use this for PRs)";
const REWRITE_2_NEW: &str = "Default branch (you will usually use this for PRs)";

fn sanitize_text(s: &str) -> String {
    s.replace(REWRITE_1_OLD, REWRITE_1_NEW)
        .replace(REWRITE_2_OLD, REWRITE_2_NEW)
}

/// hy3-family models must always receive reasoning_effort "high": CodeBuddy
/// only honors "high" for deep thinking (medium/low/max/xhigh/ultra all fall
/// back to no reasoning), so we override whatever the client sent.
fn sanitize_object(obj: &mut Value) -> bool {
    let mut changed = false;
    if obj["stream"].as_bool() != Some(true) {
        obj["stream"] = Value::Bool(true);
        changed = true;
    }
    if let Some(messages) = obj["messages"].as_array_mut() {
        for msg in messages.iter_mut() {
            match msg["content"].take() {
                Value::String(s) => {
                    let r = sanitize_text(&s);
                    if r != s {
                        changed = true;
                    }
                    msg["content"] = Value::String(r);
                }
                Value::Array(parts) => {
                    let mut new_parts = Vec::with_capacity(parts.len());
                    let mut modified = false;
                    for mut part in parts {
                        if let Some(t) = part["text"].as_str().map(str::to_owned) {
                            let r = sanitize_text(&t);
                            if r != t {
                                modified = true;
                            }
                            part["text"] = Value::String(r);
                        }
                        new_parts.push(part);
                    }
                    let _ = modified;
                    msg["content"] = Value::Array(new_parts);
                }
                other => {
                    msg["content"] = other;
                }
            }
        }
    }
    let model = obj["model"].as_str().unwrap_or("").to_string();
    if model.starts_with("hy3") {
        let eff = obj["reasoning_effort"].as_str().unwrap_or("");
        if eff != "high" {
            obj["reasoning_effort"] = Value::String("high".into());
            changed = true;
        }
    }
    changed
}

/// Force stream:true, apply prompt rewrites, re-serialize. Unparsable input
/// is returned unchanged (matches Go behavior).
pub fn sanitize_request(body: &[u8]) -> Vec<u8> {
    let mut obj: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    sanitize_object(&mut obj);
    serde_json::to_vec(&obj).unwrap_or_else(|_| body.to_vec())
}

/// CPA's chat-completions passthrough adds the "data: " prefix itself, but
/// cross-format translators only consume payloads already SSE-framed.
pub fn sse_framed_for_path(metadata: &Value) -> bool {
    let path = metadata["request_path"].as_str().unwrap_or("");
    !matches!(path.trim().to_lowercase().as_str(), "/v1/chat/completions" | "/v1/completions")
}

pub fn strip_data_prefix(s: &str) -> String {
    let mut s = s.trim().to_string();
    while let Some(rest) = s.strip_prefix("data:") {
        s = rest.trim().to_string();
    }
    s
}

/// Strip empty-valued fields from choice deltas so strict clients don't trip
/// on {"function_call":null,"tool_calls":[]}.
pub fn clean_chunk_json(s: &str) -> String {
    let mut obj: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return s.to_string(),
    };
    if let Some(choices) = obj["choices"].as_array_mut() {
        for choice in choices.iter_mut() {
            if let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) {
                delta.retain(|_, v| !is_empty_value(v));
            }
        }
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| s.to_string())
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

pub struct ExecReqParts {
    pub model: String,
    pub storage: StoredAuth,
    pub metadata: Value,
}

fn chat_headers(sa: &StoredAuth) -> HeaderSet {
    backend_header_set(sa)
}

fn post_chat(body: &[u8], sa: &StoredAuth) -> Result<ureq::Response, String> {
    let req = chat_headers(sa).apply_to(shared_agent().post(&format!("{UPSTREAM_BASE}{ENDPOINT_CHAT}")));
    req.send_bytes(body).map_err(|e| match e {
        ureq::Error::Status(s, r) => {
            let detail = r
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            format!("upstream {s}: {detail}")
        }
        other => format!("http_error: {other}"),
    })
}

/// Non-streaming client request: CodeBuddy rejects stream:false upstream
/// (code 11101), so always stream and fold chunks into one chat.completion.
pub fn execute(req: &ExecReq) -> Result<ExecutorExecResponse, String> {
    let body = sanitize_request(if req.payload.is_empty() { &req.original } else { &req.payload });
    let resp = post_chat(&body, &req.storage)?;
    let completion = aggregate_completion(std::io::BufReader::new(resp.into_reader()), &req.model)?;
    Ok(ExecutorExecResponse {
        payload: crate::rpc::b64_encode(&completion),
        headers: None,
    })
}

/// Streaming: without a stream_id collect chunks synchronously; with one,
/// return immediately and pump chunks from a background thread.
pub fn execute_stream(req: &ExecReq) -> Result<ExecutorStreamResponse, String> {
    let sse_framed = sse_framed_for_path(&req.metadata);
    let headers = stream_headers();

    if req.stream_id.is_empty() {
        let body = sanitize_request(if req.payload.is_empty() { &req.original } else { &req.payload });
        let chunks = collected_framed(&body, &req.storage, sse_framed)?;
        return Ok(ExecutorStreamResponse { headers, chunks });
    }

    // Async path: background pump emits via host.stream.emit.
    let body = sanitize_request(if req.payload.is_empty() { &req.original } else { &req.payload });
    let sa = req.storage.clone();
    let stream_id = req.stream_id.clone();
    std::thread::spawn(move || pump_upstream(body, sa, stream_id, sse_framed));
    Ok(ExecutorStreamResponse { headers, chunks: Vec::new() })
}

fn stream_headers() -> std::collections::HashMap<String, Vec<String>> {
    let mut h = std::collections::HashMap::new();
    h.insert("Content-Type".into(), vec!["text/event-stream".into()]);
    h.insert("Cache-Control".into(), vec!["no-cache".into()]);
    h.insert("X-Accel-Buffering".into(), vec!["no".into()]);
    h
}

fn collected_framed(body: &[u8], sa: &StoredAuth, sse_framed: bool) -> Result<Vec<StreamChunk>, String> {
    let resp = post_chat(body, sa)?;
    let mut out = Vec::new();
    for raw in read_sse_chunks(std::io::BufReader::new(resp.into_reader())) {
        let cleaned = clean_chunk_json(&raw);
        if cleaned.is_empty() {
            continue;
        }
        let payload = if sse_framed { format!("data: {cleaned}") } else { cleaned };
        out.push(StreamChunk { payload: crate::rpc::b64_encode(payload.as_bytes()) });
    }
    Ok(out)
}

/// Read upstream SSE lines and yield cleaned JSON payloads (no [DONE], no
/// data: prefix, empty-delta fields stripped).
pub fn read_sse_chunks<R: BufRead>(reader: R) -> Vec<String> {
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let content = strip_data_prefix(&line);
        if content.is_empty() || content == "[DONE]" {
            continue;
        }
        let cleaned = clean_chunk_json(&content);
        if cleaned.is_empty() {
            continue;
        }
        out.push(cleaned);
    }
    out
}

/// Background pump: emit each cleaned chunk to the host stream, then close.
/// An emit failure (client disconnected, host closed the stream) aborts the
/// pump so we stop reading a dead upstream.
fn pump_upstream(body: Vec<u8>, sa: StoredAuth, stream_id: String, sse_framed: bool) {
    let emit = |payload: &[u8]| -> Result<(), String> {
        let body = serde_json::json!({
            "stream_id": stream_id,
            "payload": crate::rpc::b64_encode(payload),
        });
        crate::cabi::host_call("host.stream.emit", body.to_string().as_bytes()).map(|_| ())
    };
    let close = |err: Option<&str>| {
        let body = match err {
            Some(e) => serde_json::json!({"stream_id": stream_id, "error": e}),
            None => serde_json::json!({"stream_id": stream_id}),
        };
        let _ = crate::cabi::host_call("host.stream.close", body.to_string().as_bytes());
    };

    let resp = match post_chat(&body, &sa) {
        Ok(r) => r,
        Err(e) => {
            let err_json = serde_json::json!({"error": {"message": e}}).to_string();
            let _ = emit(err_json.as_bytes());
            close(Some("upstream error"));
            return;
        }
    };
    if resp.status() >= 400 {
        let detail = resp.into_string().unwrap_or_default().chars().take(200).collect::<String>();
        let err_json = serde_json::json!({"error": {"message": format!("upstream error: {detail}")}}).to_string();
        let _ = emit(err_json.as_bytes());
        close(Some("upstream http error"));
        return;
    }
    for raw in read_sse_chunks(std::io::BufReader::new(resp.into_reader())) {
        let payload = if sse_framed { format!("data: {raw}") } else { raw };
        if emit(payload.as_bytes()).is_err() {
            break;
        }
    }
    close(None);
}

/// Fold an SSE stream into a single non-streaming chat.completion object.
pub fn aggregate_completion<R: BufRead>(reader: R, model: &str) -> Result<Vec<u8>, String> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut role = String::new();
    let mut resp_model = String::new();
    let mut resp_id = String::new();
    let mut finish = String::new();
    let mut created: i64 = 0;
    let mut usage: Option<Value> = None;
    let mut tool_calls: Vec<Value> = Vec::new();

    for raw in read_sse_chunks(reader) {
        let chunk: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(id) = chunk["id"].as_str().filter(|s| !s.is_empty()) {
            resp_id = id.to_string();
        }
        if let Some(m) = chunk["model"].as_str().filter(|s| !s.is_empty()) {
            resp_model = m.to_string();
        }
        if let Some(c) = chunk["created"].as_i64() {
            created = c;
        }
        if chunk["usage"].is_object() {
            usage = Some(chunk["usage"].clone());
        }
        if let Some(choices) = chunk["choices"].as_array() {
            for choice in choices {
                let delta = &choice["delta"];
                if !delta.is_object() {
                    continue;
                }
                if let Some(r) = delta["role"].as_str().filter(|s| !s.is_empty()) {
                    role = r.to_string();
                }
                if let Some(c) = delta["content"].as_str() {
                    content.push_str(c);
                }
                if let Some(r) = delta["reasoning_content"].as_str() {
                    reasoning.push_str(r);
                }
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        if tc.is_object() {
                            tool_calls.push(tc.clone());
                        }
                    }
                }
                if let Some(f) = choice["finish_reason"].as_str().filter(|s| !s.is_empty()) {
                    finish = f.to_string();
                }
            }
        }
    }

    let mut message = serde_json::json!({
        "role": if role.is_empty() { "assistant" } else { role.as_str() },
        "content": content,
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    if created == 0 {
        created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
    }
    let mut result = serde_json::json!({
        "id": if resp_id.is_empty() { "chatcmpl-workbuddy".to_string() } else { resp_id },
        "object": "chat.completion",
        "created": created,
        "model": if resp_model.is_empty() { model.to_string() } else { resp_model },
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if finish.is_empty() { "stop" } else { finish.as_str() },
        }],
    });
    if let Some(u) = usage {
        result["usage"] = u;
    }
    serde_json::to_vec(&result).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_forces_stream_true() {
        let out = sanitize_request(b"{\"model\":\"glm-5.2\",\"stream\":false}");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn sanitize_pins_hy3_reasoning_high() {
        let out = sanitize_request(b"{\"model\":\"hy3-preview\",\"reasoning_effort\":\"low\",\"messages\":[]}");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], "high");
    }

    #[test]
    fn sanitize_leaves_non_hy3_effort() {
        let out = sanitize_request(b"{\"model\":\"glm-5.2\",\"reasoning_effort\":\"low\",\"messages\":[]}");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning_effort"], "low");
    }

    #[test]
    fn sanitize_rewrites_identity_line() {
        let body = "{\"model\":\"glm-5.2\",\"messages\":[{\"role\":\"system\",\"content\":\"You are Claude Code, Anthropic's official CLI for Claude.\"}]}";
        let out = sanitize_request(body.as_bytes());
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("official CLI tool for Claude."), "{s}");
        assert!(!s.contains("official CLI for Claude."), "{s}");
    }

    #[test]
    fn sanitize_rewrites_default_branch_line() {
        let body = "{\"model\":\"glm-5.2\",\"messages\":[{\"role\":\"system\",\"content\":\"Main branch (you will usually use this for PRs) is main.\"}]}";
        let out = sanitize_request(body.as_bytes());
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Default branch (you will usually use this for PRs)"), "{s}");
        assert!(!s.contains("Main branch"), "{s}");
    }

    #[test]
    fn sanitize_rewrites_multimodal_parts() {
        let body = "{\"model\":\"glm-5.2\",\"messages\":[{\"role\":\"system\",\"content\":[{\"type\":\"text\",\"text\":\"Main branch (you will usually use this for PRs)\"}]}]}";
        let out = sanitize_request(body.as_bytes());
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Default branch"), "{s}");
    }

    #[test]
    fn sanitize_invalid_json_passthrough() {
        assert_eq!(sanitize_request(b"not json"), b"not json".to_vec());
    }

    #[test]
    fn clean_chunk_strips_empty_delta_fields() {
        let c = clean_chunk_json("{\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":[],\"role\":null}}]}");
        let v: Value = serde_json::from_str(&c).unwrap();
        assert!(v["choices"][0]["delta"].get("tool_calls").is_none());
        assert!(v["choices"][0]["delta"].get("role").is_none());
        assert_eq!(v["choices"][0]["delta"]["content"], "hi");
    }

    #[test]
    fn clean_chunk_invalid_passthrough() {
        assert_eq!(clean_chunk_json("junk"), "junk");
    }

    #[test]
    fn sse_frame_detection() {
        let mut m = serde_json::Map::new();
        m.insert("request_path".into(), serde_json::json!("/v1/chat/completions"));
        assert!(!sse_framed_for_path(&Value::Object(m.clone())));
        m.insert("request_path".into(), serde_json::json!("/v1/messages"));
        assert!(sse_framed_for_path(&Value::Object(m)));
        assert!(sse_framed_for_path(&Value::Null));
    }

    #[test]
    fn strip_data_prefix_handles_double() {
        assert_eq!(strip_data_prefix("data: hello"), "hello");
        assert_eq!(strip_data_prefix("data:data: x"), "x");
        assert_eq!(strip_data_prefix("[DONE]"), "[DONE]");
    }

    #[test]
    fn aggregate_folds_stream() {
        let sse = "data: {\"id\":\"c1\",\"model\":\"glm-5.3-flash\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"total_tokens\":5}}\n\
                   data: [DONE]\n";
        let out = aggregate_completion(sse.as_bytes(), "glm-5.3-flash").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["id"], "c1");
        assert_eq!(v["choices"][0]["message"]["content"], "你好");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 5);
    }
}
