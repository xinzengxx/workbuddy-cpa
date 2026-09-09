pub fn error_envelope(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}
