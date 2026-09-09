use crate::rpc::StoredAuth;
use crate::upstream::{backend_header_set, post_envelope, shared_agent, ENDPOINT_BILLING_USER_RESOURCE};

/// Query CodeBuddy billing for one credential. Returns the credits summary in
/// the exact JSON shape the v2 panel consumes (total_remain/total_used/
/// total_size/pack_count/packages/fetched_at, plus "error" on failure).
pub fn fetch_credits(sa: &StoredAuth) -> serde_json::Value {
    let fetched_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let mut out = serde_json::json!({
        "total_remain": 0,
        "total_used": 0,
        "total_size": 0,
        "pack_count": 0,
        "packages": [],
        "fetched_at": fetched_at,
    });
    let data = match post_envelope(
        shared_agent(),
        ENDPOINT_BILLING_USER_RESOURCE,
        backend_header_set(sa),
        "{}",
    ) {
        Ok(d) => d,
        Err(e) => {
            out["error"] = serde_json::json!(e.to_string());
            return out;
        }
    };
    let accounts = &data["Response"]["Data"]["Accounts"];
    let empty = Vec::new();
    let list = accounts.as_array().unwrap_or(&empty);
    let mut packages = Vec::with_capacity(list.len());
    let mut total_remain: i64 = 0;
    let mut total_used: i64 = 0;
    let mut total_size: i64 = 0;
    for acc in list {
        let remain = acc["CapacityRemain"].as_i64().unwrap_or(0);
        let used = acc["CapacityUsed"].as_i64().unwrap_or(0);
        let size = acc["CapacitySize"].as_i64().unwrap_or(0);
        total_remain += remain;
        total_used += used;
        total_size += size;
        packages.push(serde_json::json!({
            "name": acc["PackageName"].as_str().unwrap_or(""),
            "remain": remain,
            "used": used,
            "size": size,
            "cycle_start": acc["CycleStartTime"].as_str().unwrap_or(""),
            "cycle_end": acc["CycleEndTime"].as_str().unwrap_or(""),
        }));
    }
    out["packages"] = serde_json::json!(packages);
    out["pack_count"] = serde_json::json!(packages.len());
    out["total_remain"] = serde_json::json!(total_remain);
    out["total_used"] = serde_json::json!(total_used);
    out["total_size"] = serde_json::json!(total_size);
    out
}
