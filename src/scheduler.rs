use rand::Rng;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const PROVIDER: &str = "workbuddy";
const CACHE_TTL_SECS: u64 = 60;
/// Pseudo-quota for candidates whose quota is not yet known: they win the
/// first pick so a real refresh gets triggered, then settle into weights.
const UNKNOWN_QUOTA: i64 = i64::MAX / 4;
const WEIGHT_CAP: i64 = 1_000_000;

#[derive(Clone)]
struct CacheEntry {
    total_remain: i64,
    #[allow(dead_code)]
    total_size: i64,
    refreshed_at: u64,
}

static QUOTA_CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);

/// auth ID -> host auth_index (the key host.auth.get expects and the panel's
/// refresh_quota_sync writes). Built from host.auth.list, refreshed at most
/// once per 30s and only on lookup misses.
static INDEX_MAP: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static INDEX_MAP_AT: Mutex<u64> = Mutex::new(0);

fn refresh_index_map() {
    {
        let mut at = INDEX_MAP_AT.lock().unwrap_or_else(|e| e.into_inner());
        if now_unix().saturating_sub(*at) < 30 {
            return;
        }
        *at = now_unix();
    }
    let files = match crate::management::list_auth_files() {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(list) = files.as_array() else { return };
    let mut map = HashMap::new();
    for f in list {
        let Some(id) = f.get("id").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else { continue };
        let Some(index) = f.get("auth_index").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) else { continue };
        map.insert(id.trim().to_string(), index.trim().to_string());
    }
    INDEX_MAP.lock().unwrap_or_else(|e| e.into_inner()).replace(map);
}

/// Resolve a candidate auth ID to the host's auth_index. Unresolvable IDs
/// (host unavailable, unknown account) yield None and callers degrade
/// gracefully — pick keeps them as UNKNOWN_QUOTA, note_usage no-ops.
fn index_for(id: &str) -> Option<String> {
    {
        let guard = INDEX_MAP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = guard.as_ref() {
            if let Some(v) = m.get(id) {
                return Some(v.clone());
            }
        }
    }
    refresh_index_map();
    INDEX_MAP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(id))
        .cloned()
}

fn with_cache<F, R>(f: F) -> R
where
    F: FnOnce(&mut HashMap<String, CacheEntry>) -> R,
{
    let mut guard = QUOTA_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct Cand {
    pub id: String,
    pub provider: String,
    pub status: String,
}

/// One scheduling decision for the host's scheduler.pick RPC.
/// Candidates arrive as a JSON object with PascalCase keys (the host marshals
/// pluginapi.SchedulerPickRequest directly, no rpc wrapper).
pub fn pick(req: &Value) -> Value {
    let candidates = crate::rpc::get_field(req, "candidates")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let cands: Vec<Cand> = candidates
        .iter()
        .map(|c| Cand {
            id: crate::rpc::get_field(c, "id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            provider: crate::rpc::get_field(c, "provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: crate::rpc::get_field(c, "status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .filter(|c| !c.id.is_empty())
        .collect();

    let disabled = crate::state::load_disabled();
    let state_view = StateView { disabled };

    match candidates_filter_and_pick(&cands, &state_view) {
        Some(id) => serde_json::json!({"AuthID": id, "DelegateBuiltin": "", "Handled": true}),
        None => serde_json::json!({"AuthID": "", "DelegateBuiltin": "", "Handled": false}),
    }
}

pub struct StateView {
    pub disabled: Vec<String>,
}

/// Weighted-random selection over candidates by remaining quota.
/// Unknown-quota candidates win the first pick (pseudo-quota) so a real
/// billing refresh is triggered; exhausted/disabled candidates are skipped.
/// Returns None when no candidate is eligible: the caller must answer
/// Handled:false so the host's built-in selector takes over.
pub fn candidates_filter_and_pick(cands: &[Cand], state: &StateView) -> Option<String> {
    let mut weighted: Vec<(String, i64)> = Vec::new();
    for c in cands {
        if c.provider != PROVIDER {
            continue;
        }
        if !c.status.is_empty() && c.status != "active" {
            continue;
        }
        if state.disabled.iter().any(|d| d == &c.id) {
            continue;
        }
        // Quota cache is keyed by the host auth_index (same key the panel
        // writes); the candidate arrives with its auth ID, so resolve first.
        let remain = match index_for(&c.id) {
            Some(index) => match cache_get(&index) {
                Some(entry) => entry.total_remain,
                None => {
                    refresh_quota_async(&c.id);
                    UNKNOWN_QUOTA
                }
            },
            None => {
                refresh_quota_async(&c.id);
                UNKNOWN_QUOTA
            }
        };
        if remain <= 0 {
            continue;
        }
        weighted.push((c.id.clone(), remain.min(WEIGHT_CAP)));
    }
    if weighted.is_empty() {
        return None;
    }
    let total: i64 = weighted.iter().map(|(_, w)| *w).sum();
    let mut pick = rand::thread_rng().gen_range(0..total);
    for (id, w) in &weighted {
        if pick < *w {
            return Some(id.clone());
        }
        pick -= *w;
    }
    weighted.last().map(|(id, _)| id.clone())
}

fn cache_get(auth_index: &str) -> Option<CacheEntry> {
    let fresh = with_cache(|m| {
        m.get(auth_index)
            .map(|e| now_unix() - e.refreshed_at < CACHE_TTL_SECS)
            .unwrap_or(false)
    });
    if fresh {
        with_cache(|m| m.get(auth_index).cloned())
    } else {
        None
    }
}

/// Optimistic in-memory decrement after a successful request, so weights
/// drift between real billing refreshes. `auth_id` is the candidate auth ID;
/// it is resolved to the host auth_index first. Cache misses and
/// unresolvable IDs are no-ops.
pub fn note_usage(auth_id: &str, total_tokens: i64) {
    if total_tokens <= 0 {
        return;
    }
    let Some(index) = index_for(auth_id) else { return };
    with_cache(|m| {
        if let Some(e) = m.get_mut(&index) {
            e.total_remain -= total_tokens;
        }
    });
}

/// Insert one billing snapshot into the cache. Failed fetches (error shape)
/// are dropped entirely: caching their zero would make the scheduler treat a
/// healthy account as exhausted for a whole cache TTL.
fn store_quota(index: &str, credits: &Value) {
    if credits.get("error").is_some() {
        return;
    }
    let entry = CacheEntry {
        total_remain: credits["total_remain"].as_i64().unwrap_or(0),
        total_size: credits["total_size"].as_i64().unwrap_or(0),
        refreshed_at: now_unix(),
    };
    with_cache(|m| {
        m.insert(index.to_string(), entry);
    });
}

/// Refresh quota for one account in the background (never blocks pick).
/// `auth_id` is the candidate auth ID; it is resolved to the host auth_index
/// so host.auth.get finds the credential.
pub fn refresh_quota_async(auth_id: &str) {
    let id = auth_id.to_string();
    std::thread::spawn(move || {
        let Some(index) = index_for(&id) else { return };
        // Read the credential through the host and fetch billing data.
        let raw = match crate::cabi::host_call(
            "host.auth.get",
            serde_json::json!({"auth_index": index}).to_string().as_bytes(),
        ) {
            Ok(r) => r,
            Err(_) => return,
        };
        let result = match crate::rpc::parse_envelope(&raw) {
            Ok(v) => v,
            Err(_) => return,
        };
        let json_value = match result
            .get("JSON")
            .or_else(|| result.get("json"))
            .cloned()
        {
            Some(v) => v,
            None => return,
        };
        let sa: crate::rpc::StoredAuth = match serde_json::from_value(json_value) {
            Ok(v) => v,
            Err(_) => return,
        };
        store_quota(&index, &crate::billing::fetch_credits(&sa));
    });
}

/// Pre-populate the cache synchronously (used by the panel refresh path).
pub fn refresh_quota_sync(auth_index: &str, credits: &Value) {
    store_quota(auth_index, credits);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, provider: &str, status: &str) -> Cand {
        Cand { id: id.into(), provider: provider.into(), status: status.into() }
    }

    fn state_with(disabled: Vec<String>) -> StateView {
        StateView { disabled }
    }

    fn seed(id: &str, remain: i64) {
        with_cache(|m| {
            m.insert(id.into(), CacheEntry { total_remain: remain, total_size: 1000, refreshed_at: now_unix() });
        });
    }

    fn map_id(id: &str, index: &str) {
        INDEX_MAP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(id.into(), index.into());
    }

    #[test]
    fn disabled_candidate_skipped() {
        map_id("a1", "idx-a1");
        map_id("a2", "idx-a2");
        seed("idx-a1", 500);
        seed("idx-a2", 500);
        let cands = vec![cand("a1", "workbuddy", "active"), cand("a2", "workbuddy", "active")];
        let st = state_with(vec!["a1".into()]);
        for _ in 0..50 {
            let picked = candidates_filter_and_pick(&cands, &st).unwrap();
            assert_ne!(picked, "a1");
        }
    }

    #[test]
    fn exhausted_candidate_skipped() {
        map_id("rich", "idx-rich");
        map_id("empty", "idx-empty");
        seed("idx-rich", 900);
        seed("idx-empty", 0);
        let cands = vec![cand("rich", "workbuddy", "active"), cand("empty", "workbuddy", "active")];
        let st = state_with(vec![]);
        for _ in 0..30 {
            assert_eq!(candidates_filter_and_pick(&cands, &st).unwrap(), "rich");
        }
    }

    #[test]
    fn weighted_distribution_skews_to_rich() {
        map_id("rich", "idx-rich");
        map_id("poor", "idx-poor");
        seed("idx-rich", 900);
        seed("idx-poor", 100);
        let cands = vec![cand("rich", "workbuddy", "active"), cand("poor", "workbuddy", "active")];
        let st = state_with(vec![]);
        let mut rich = 0;
        for _ in 0..200 {
            if candidates_filter_and_pick(&cands, &st).unwrap() == "rich" {
                rich += 1;
            }
        }
        assert!(rich >= 120, "rich should win ~180/200, got {rich}");
    }

    #[test]
    fn all_exhausted_returns_none() {
        map_id("only", "idx-only");
        seed("idx-only", 0);
        let cands = vec![cand("only", "workbuddy", "active")];
        assert!(candidates_filter_and_pick(&cands, &state_with(vec![])).is_none());
    }

    #[test]
    fn non_workbuddy_skipped() {
        seed("codex1", 999);
        let cands = vec![cand("codex1", "codex", "active")];
        assert!(candidates_filter_and_pick(&cands, &state_with(vec![])).is_none());
    }

    #[test]
    fn note_usage_resolves_index() {
        map_id("acct", "idx-acct");
        seed("idx-acct", 500);
        note_usage("acct", 120);
        with_cache(|m| assert_eq!(m.get("idx-acct").unwrap().total_remain, 380));
        note_usage("acct", 0); // no-op
        with_cache(|m| assert_eq!(m.get("idx-acct").unwrap().total_remain, 380));
        note_usage("unmapped", 50); // unresolvable id: silent no-op, no panic
    }

    #[test]
    fn unmapped_id_still_pickable_as_unknown() {
        // No index mapping (host.auth.list unavailable in tests): the candidate
        // must stay pickable via the UNKNOWN_QUOTA path, never dropped.
        let cands = vec![cand("nomap", "workbuddy", "active")];
        let st = state_with(vec![]);
        for _ in 0..10 {
            assert_eq!(candidates_filter_and_pick(&cands, &st).unwrap(), "nomap");
        }
    }

    #[test]
    fn billing_error_not_cached() {
        let credits = serde_json::json!({"total_remain": 0, "total_used": 0, "error": "upstream 403"});
        store_quota("idx-err", &credits);
        assert!(cache_get("idx-err").is_none(), "failed billing must not be cached as 0");
        store_quota("idx-ok", &serde_json::json!({"total_remain": 42, "total_size": 100}));
        assert_eq!(cache_get("idx-ok").unwrap().total_remain, 42);
    }

    #[test]
    fn unknown_candidate_has_pseudo_quota() {
        // No cache entry for "fresh": beats a small known quota.
        map_id("small", "idx-small");
        seed("idx-small", 10);
        let cands = vec![cand("small", "workbuddy", "active"), cand("fresh", "workbuddy", "active")];
        let st = state_with(vec![]);
        let mut fresh = 0;
        for _ in 0..20 {
            if candidates_filter_and_pick(&cands, &st).unwrap() == "fresh" {
                fresh += 1;
            }
        }
        assert!(fresh >= 18, "unknown should dominate first picks, got {fresh}/20");
    }

    #[test]
    fn pick_shape_over_json() {
        let body = serde_json::json!({
            "Provider": "workbuddy",
            "Candidates": [
                {"ID": "a1", "Provider": "workbuddy", "Status": "active"},
                {"ID": "a2", "Provider": "codex", "Status": "active"}
            ]
        });
        let resp = pick(&body);
        assert_eq!(resp["Handled"], true);
        assert_eq!(resp["AuthID"], "a1");
    }

    #[test]
    fn pick_no_candidates_delegates() {
        let resp = pick(&serde_json::json!({"Candidates": []}));
        assert_eq!(resp["Handled"], false);
    }
}
