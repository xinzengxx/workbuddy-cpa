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
    total_size: i64,
    refreshed_at: u64,
}

static QUOTA_CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);

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
        let remain = match cache_get(&c.id) {
            Some(entry) => entry.total_remain,
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
/// drift between real billing refreshes. Cache misses are no-ops.
pub fn note_usage(auth_index: &str, total_tokens: i64) {
    if total_tokens <= 0 {
        return;
    }
    with_cache(|m| {
        if let Some(e) = m.get_mut(auth_index) {
            e.total_remain -= total_tokens;
        }
    });
}

/// Refresh quota for one account in the background (never blocks pick).
pub fn refresh_quota_async(auth_index: &str) {
    let index = auth_index.to_string();
    std::thread::spawn(move || {
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
        let credits = crate::billing::fetch_credits(&sa);
        let entry = CacheEntry {
            total_remain: credits["total_remain"].as_i64().unwrap_or(0),
            total_size: credits["total_size"].as_i64().unwrap_or(0),
            refreshed_at: now_unix(),
        };
        with_cache(|m| {
            m.insert(index.clone(), entry);
        });
    });
}

/// Pre-populate the cache synchronously (used by the panel refresh path).
pub fn refresh_quota_sync(auth_index: &str, credits: &Value) {
    let entry = CacheEntry {
        total_remain: credits["total_remain"].as_i64().unwrap_or(0),
        total_size: credits["total_size"].as_i64().unwrap_or(0),
        refreshed_at: now_unix(),
    };
    with_cache(|m| {
        m.insert(auth_index.to_string(), entry);
    });
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

    #[test]
    fn disabled_candidate_skipped() {
        seed("a1", 500);
        seed("a2", 500);
        let cands = vec![cand("a1", "workbuddy", "active"), cand("a2", "workbuddy", "active")];
        let st = state_with(vec!["a1".into()]);
        for _ in 0..50 {
            let picked = candidates_filter_and_pick(&cands, &st).unwrap();
            assert_ne!(picked, "a1");
        }
    }

    #[test]
    fn exhausted_candidate_skipped() {
        seed("rich", 900);
        seed("empty", 0);
        let cands = vec![cand("rich", "workbuddy", "active"), cand("empty", "workbuddy", "active")];
        let st = state_with(vec![]);
        for _ in 0..30 {
            assert_eq!(candidates_filter_and_pick(&cands, &st).unwrap(), "rich");
        }
    }

    #[test]
    fn weighted_distribution_skews_to_rich() {
        seed("rich", 900);
        seed("poor", 100);
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
        seed("only", 0);
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
    fn note_usage_decrements() {
        seed("acct", 500);
        note_usage("acct", 120);
        with_cache(|m| assert_eq!(m.get("acct").unwrap().total_remain, 380));
        note_usage("acct", 0); // no-op
        with_cache(|m| assert_eq!(m.get("acct").unwrap().total_remain, 380));
    }

    #[test]
    fn unknown_candidate_has_pseudo_quota() {
        // No cache entry for "fresh": beats a small known quota.
        seed("small", 10);
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
