pub fn handle(_method: &str, _request: &[u8]) -> Result<Vec<u8>, String> {
    Ok(crate::rpc::error_envelope("unknown_method", "not implemented").into_bytes())
}
