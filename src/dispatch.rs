use crate::executor::{execute, execute_stream, ExecReq};
use crate::models::{builtin_models, merge_model_overrides};
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
            let config_yaml = b64_decode(req["config_yaml"].as_str().unwrap_or(""));
            refresh_models(&config_yaml);
            ok_envelope(&crate::models::default_registration()).map_err(|e| e)
        }
        "model.static" | "model.for_auth" => {
            let resp = ModelResponse { provider: "workbuddy".into(), models: current_models() };
            ok_envelope(&resp).map_err(|e| e)
        }
        "auth.identifier" | "executor.identifier" => {
            ok_envelope(&serde_json::json!({"Identifier": "workbuddy"})).map_err(|e| e)
        }
        "auth.parse" => {
            let resp: AuthParseResponse = crate::auth::parse_auth(req["raw_json"].as_str().unwrap_or(""));
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
            let state = req["state"].as_str().unwrap_or("");
            match crate::auth::poll_login(state) {
                Ok(r) => {
                    let r: AuthLoginPollResponse = r;
                    ok_envelope(&r).map_err(|e| e)
                }
                Err(e) => Ok(error_envelope("plugin_error", &e).into_bytes()),
            }
        }
        "auth.refresh" => match crate::auth::refresh(req["storage_json"].as_str().unwrap_or("")) {
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
            let stream_id = req["stream_id"].as_str().unwrap_or("").to_string();
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
        "management.register" => ok_envelope(&crate::management::registration()).map_err(|e| e),
        "management.handle" => {
            let m = req["method"].as_str().unwrap_or("GET");
            let p = req["path"].as_str().unwrap_or("");
            let q = req["query"].as_str().unwrap_or("");
            let body_b64 = req["body"].as_str().unwrap_or("");
            // query may arrive as an object rather than string
            let q = if q.is_empty() {
                match req["query"].as_object() {
                    Some(map) => &serde_json::to_string(map).unwrap_or_default(),
                    None => "",
                }
            } else {
                q
            };
            let resp: crate::rpc::MgmtResponse = crate::management::handle(m, p, q, body_b64);
            ok_envelope(&resp).map_err(|e| e)
        }
        _ => Ok(error_envelope("unknown_method", &format!("unknown method: {method}")).into_bytes()),
    }
}

fn decode_exec_req(req: &serde_json::Value, stream_id: &str) -> Result<ExecReq, String> {
    let model = req["model"].as_str().unwrap_or("").to_string();
    let payload = b64_decode(req["payload"].as_str().unwrap_or(""));
    let original = b64_decode(req["original_request"].as_str().unwrap_or(""));
    let storage = b64_decode(req["storage_json"].as_str().unwrap_or(""));
    let storage: crate::rpc::StoredAuth = serde_json::from_slice(&storage)
        .map_err(|e| format!("storage_parse_error: {e}"))?;
    Ok(ExecReq {
        model,
        payload,
        original,
        storage,
        metadata: req["metadata"].clone(),
        stream_id: stream_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(v["result"]["Provider"], "workbuddy");
        let models = v["result"]["Models"].as_array().unwrap();
        assert!(models.len() >= 11);
        assert!(models.iter().any(|m| m["ID"] == "glm-5.3-flash"));
        assert!(models.iter().all(|m| m["OwnedBy"] == "workbuddy"));
    }

    #[test]
    fn reconfigure_hot_reloads_models() {
        let mut cfg = "models:\n  - id: test-model-x\n    name: Test X\n".as_bytes().to_vec();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&cfg);
        let body = serde_json::json!({"config_yaml": b64}).to_string();
        let raw = handle("plugin.reconfigure", body.as_bytes()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["result"]["capabilities"]["executor"], true);
        let raw = handle("model.static", b"{}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(v["result"]["Models"].as_array().unwrap().iter().any(|m| m["ID"] == "test-model-x"));
        // restore builtin table for other tests
        cfg.clear();
        let _ = cfg;
        let raw = handle("plugin.reconfigure", serde_json::json!({"config_yaml": ""}).to_string().as_bytes()).unwrap();
        assert!(!raw.is_empty());
    }
}
