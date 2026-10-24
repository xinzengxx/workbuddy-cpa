use crate::executor::{execute, execute_stream, ExecReq};
use crate::models::{builtin_models, merge_model_overrides};
use crate::realm::realm;
use crate::rpc::{
    b64_decode, error_envelope, ok_envelope, AuthLoginStartResponse, AuthParseResponse,
    AuthRefreshResponse, AuthLoginPollResponse, ExecutorExecResponse, ExecutorStreamResponse,
    ModelResponse,
};
use std::sync::Mutex;

/// Live model table, replaced on plugin.register / plugin.reconfigure.
static MODELS: Mutex<Vec<crate::rpc::ModelInfo>> = Mutex::new(Vec::new());


fn current_models() -> Vec<crate::rpc::ModelInfo> {
    let guard = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_empty() {
        builtin_models()
    } else {
        guard.clone()
    }
}

fn refresh_models(config_yaml: &[u8]) {
    let merged = merge_model_overrides(builtin_models(), config_yaml);
    let mut guard = MODELS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = merged;
}

pub fn handle(method: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    let req: serde_json::Value = if request.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(request).unwrap_or_else(|_| serde_json::json!({}))
    };

    match method {
        "plugin.register" | "plugin.reconfigure" => {
            let config_yaml = b64_decode(crate::rpc::get_field(&req, "config_yaml").and_then(|v| v.as_str()).unwrap_or(""));
            refresh_models(&config_yaml);
            ok_envelope(&crate::models::default_registration()).map_err(|e| e)
        }
        "model.static" | "model.for_auth" => {
            let resp = ModelResponse { provider: realm().provider.into(), models: current_models() };
            ok_envelope(&resp).map_err(|e| e)
        }
        "auth.identifier" | "executor.identifier" => {
            ok_envelope(&serde_json::json!({"Identifier": realm().provider})).map_err(|e| e)
        }
        "auth.parse" => {
            let resp: AuthParseResponse = crate::auth::parse_auth(
            crate::rpc::get_field(&req, "raw_json").and_then(|v| v.as_str()).unwrap_or(""),
            crate::rpc::get_field(&req, "file_name").and_then(|v| v.as_str()).unwrap_or(""),
        );
            ok_envelope(&resp).map_err(|e| e)
        }
        "auth.login.start" => match crate::auth::start_login() {
            Ok(resp) => {
                let r: AuthLoginStartResponse = resp;
                ok_envelope(&r).map_err(|e| e)
            }
            Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
        },
        "auth.login.poll" => {
            let state = crate::rpc::get_field(&req, "state").and_then(|v| v.as_str()).unwrap_or("");
            match crate::auth::poll_login(state) {
                Ok(r) => {
                    let r: AuthLoginPollResponse = r;
                    ok_envelope(&r).map_err(|e| e)
                }
                Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
            }
        }
        "auth.refresh" => match crate::auth::refresh(crate::rpc::get_field(&req, "storage_json").and_then(|v| v.as_str()).unwrap_or("")) {
            Ok(r) => {
                let r: AuthRefreshResponse = r;
                ok_envelope(&r).map_err(|e| e)
            }
            Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
        },
        "executor.execute" => {
            let exec_req = decode_exec_req(&req, "")?;
            match execute(&exec_req) {
                Ok(r) => {
                    let r: ExecutorExecResponse = r;
                    ok_envelope(&r).map_err(|e| e)
                }
                Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
            }
        }
        "executor.execute_stream" => {
            let stream_id = crate::rpc::get_field(&req, "stream_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let exec_req = decode_exec_req(&req, &stream_id)?;
            match execute_stream(&exec_req) {
                Ok(r) => {
                    let r: ExecutorStreamResponse = r;
                    ok_envelope(&r).map_err(|e| e)
                }
                Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
            }
        }
        "executor.count_tokens" => {
            ok_envelope(&serde_json::json!({"Payload": crate::rpc::b64_encode(b"{\"input_tokens\":0}")}))
                .map_err(|e| e)
        }
        "scheduler.pick" => {
            let resp = crate::scheduler::pick(&req);
            ok_envelope(&resp).map_err(|e| e)
        }
        "management.register" => ok_envelope(&crate::management::registration()).map_err(|e| e),
        "management.handle" => {
            let m = crate::rpc::get_field(&req, "method").and_then(|v| v.as_str()).unwrap_or("GET");
            let p = crate::rpc::get_field(&req, "path").and_then(|v| v.as_str()).unwrap_or("");
            let body_b64 = crate::rpc::get_field(&req, "body").and_then(|v| v.as_str()).unwrap_or("");
            // The host may hand the query over as a raw string or as an object;
            // normalise both to `k=v&k2=v2` so handlers only parse one shape.
            let q = match crate::rpc::get_field(&req, "query") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Object(map)) => map
                    .iter()
                    .map(|(k, v)| {
                        let val = v
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string());
                        format!("{k}={val}")
                    })
                    .collect::<Vec<_>>()
                    .join("&"),
                _ => String::new(),
            };
            let resp: crate::rpc::MgmtResponse = crate::management::handle(m, p, &q, body_b64);
            ok_envelope(&resp).map_err(|e| e)
        }
        _ => Ok(error_envelope("unknown_method", &format!("unknown method: {method}")).into_bytes()),
    }
}

fn decode_exec_req(req: &serde_json::Value, stream_id: &str) -> Result<ExecReq, String> {
    let model = crate::rpc::get_field(&req, "model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let auth_id = crate::rpc::get_field(&req, "auth_id")
        .or_else(|| crate::rpc::get_field(&req, "AuthID"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let payload = b64_decode(crate::rpc::get_field(&req, "payload").and_then(|v| v.as_str()).unwrap_or(""));
    let original = b64_decode(crate::rpc::get_field(&req, "original_request").and_then(|v| v.as_str()).unwrap_or(""));
    let storage = b64_decode(crate::rpc::get_field(&req, "storage_json").and_then(|v| v.as_str()).unwrap_or(""));
    let storage: crate::rpc::StoredAuth = serde_json::from_slice(&storage)
        .map_err(|e| format!("storage_parse_error: {e}"))?;
    Ok(ExecReq {
        model,
        payload,
        original,
        storage,
        // The host sends the map as "Metadata" (PascalCase wire format); a
        // direct req["metadata"] index is case-sensitive and always missed,
        // leaving metadata null (which broke SSE framing detection).
        metadata: crate::rpc::get_field(req, "metadata").cloned().unwrap_or(serde_json::Value::Null),
        stream_id: stream_id.to_string(),
        auth_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::realm;

    #[test]
    fn model_static_and_identifiers_follow_realm() {
        let raw = handle("model.static", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["result"]["Provider"], realm().provider);
        assert!(v["result"]["Models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["OwnedBy"] == realm().provider));
        let raw = handle("auth.identifier", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["result"]["Identifier"], realm().provider);
    }

    #[test]
    fn unknown_method_returns_error_envelope_rc0() {
        let raw = handle("nope.nope", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "unknown_method");
        assert!(v["error"]["message"].as_str().unwrap().contains("nope.nope"));
    }

    #[test]
    fn model_static_lists_builtin() {
        let raw = handle("model.static", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["result"]["Provider"], realm().provider);
        let models = v["result"]["Models"].as_array().unwrap();
        assert_eq!(models.len(), realm().models.len());
        assert_eq!(models[0]["ID"], realm().models[0].id);
        assert!(models.iter().all(|m| m["OwnedBy"] == realm().provider));
    }

    #[test]
    fn reconfigure_hot_reloads_models() {
        // Tune the first realm model's context: the allowlist is per-realm, so
        // adding a brand-new id only works on CN and would fail on Global.
        let own = realm().models[0].id;
        let mut cfg = format!("models:\n  - id: {own}\n    context: 424242\n").into_bytes();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&cfg);
        let body = serde_json::json!({"config_yaml": b64}).to_string();
        let raw = handle("plugin.reconfigure", body.as_bytes()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["result"]["capabilities"]["executor"], true);
        let raw = handle("model.static", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let tuned = v["result"]["Models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["ID"] == realm().models[0].id)
            .expect("realm model must stay advertised");
        assert_eq!(tuned["ContextLength"], 424242);
        // restore builtin table for other tests
        cfg.clear();
        let _ = cfg;
        let raw = handle("plugin.reconfigure", serde_json::json!({"config_yaml": ""}).to_string().as_bytes()).unwrap();
        assert!(!raw.is_empty());
    }
}
