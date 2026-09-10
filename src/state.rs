use std::path::PathBuf;
use std::sync::Mutex;

/// Path of the plugin-managed state file. Env override for tests.
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("WORKBUDDY_STATE_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cli-proxy-api").join("workbuddy-state.json")
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn load_disabled() -> Vec<String> {
    let path = state_path();
    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v["disabled"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

pub fn set_disabled(auth_index: &str, disabled: bool) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<String> = load_disabled().into_iter().filter(|x| x != auth_index).collect();
    if disabled {
        list.push(auth_index.to_string());
    }
    let body = serde_json::json!({ "disabled": list });
    let tmp = state_path().with_extension("json.tmp");
    std::fs::write(&tmp, body.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, state_path()).map_err(|e| e.to_string())
}
