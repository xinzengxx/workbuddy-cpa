use crate::rpc::{Capabilities, Metadata, ModelInfo, Registration};

pub const PROVIDER_NAME: &str = "workbuddy";

/// Plugin (file) identity — must equal the dylib file name stem. It is
/// deliberately DIFFERENT from the provider name: the plugin-store matches
/// installed file IDs against registry entries, and the official registry's
/// "workbuddy" slot belongs to someone else's plugin
/// (zidanefaqih/codebuddy-intl-cpa) — that collision made the store show
/// their repo and offer to "update" over our build. The provider /
/// credential / model identity stays PROVIDER_NAME ("workbuddy"):
/// auth.identifier, executor.identifier, model owned_by and the existing
/// workbuddy-*.json credential files are all unaffected.
pub const PLUGIN_ID: &str = "workbuddy-cpa";

pub fn default_registration() -> Registration {
    Registration {
        schema_version: crate::SCHEMA_VERSION,
        metadata: Metadata {
            name: PLUGIN_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "xinyuan (Rust rewrite; original workbuddy plugin by Sliverkiss)".into(),
            // The host plugin-store card links exactly this URL (host reads
            // plugin.Metadata.GitHubRepository from the registration). Must
            // point at THIS repo, not the Go upstream this rewrite came from.
            repo: "https://github.com/xinzengxx/workbuddy-cpa".into(),
        },
        capabilities: Capabilities {
            model_provider: true,
            auth_provider: true,
            executor: true,
            executor_model_scope: "both",
            input_formats: vec!["chat-completions"],
            output_formats: vec!["chat-completions"],
            management_api: true,
            scheduler: true,
        },
    }
}

/// GLM-only allowlist. WorkBuddy must not advertise DeepSeek/Kimi/Hy3/MiniMax:
/// CPA routes by model id, so omitting those ids is what keeps their traffic
/// from ever selecting a WorkBuddy credential.
pub fn is_glm_model(id: &str) -> bool {
    id.trim().to_ascii_lowercase().starts_with("glm-")
}

/// The host may pass "workbuddy/glm-5.2" or a bare id. Strip a provider prefix
/// and reject anything outside the GLM family before an upstream request is made.
pub fn require_glm_model(model: &str) -> Result<String, String> {
    let bare = model.rsplit('/').next().unwrap_or(model).trim();
    if bare.is_empty() {
        return Ok(String::new());
    }
    if is_glm_model(bare) {
        Ok(bare.to_string())
    } else {
        Err(format!(
            "model_not_supported: workbuddy-cpa serves GLM models only (got {model})"
        ))
    }
}

/// Builtin model table is deliberately GLM-only. Config overrides (models: key
/// in the plugin's config_yaml) may add or tune GLM entries, never other families.
pub fn builtin_models() -> Vec<ModelInfo> {
    let specs: &[(&str, &str, i64)] = &[
        ("glm-5.2", "GLM-5.2", 1_000_000),
        ("glm-5.3-flash", "GLM-5.3 Flash", 1_000_000),
        ("glm-5.1", "GLM-5.1", 131_072),
        ("glm-5v-turbo", "GLM-5V Turbo", 131_072),
        ("glm-5.3", "GLM-5.3", 131_072),
    ];
    specs
        .iter()
        .map(|(id, name, ctx)| ModelInfo {
            id: id.to_string(),
            object: "model".into(),
            owned_by: PROVIDER_NAME.into(),
            display_name: name.to_string(),
            name: id.to_string(),
            methods: vec!["chat".into()],
            context_length: *ctx,
            max_completion_tokens: 8192,
            user_defined: true,
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct ModelOverride {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context: Option<i64>,
}

/// Merge `models:` overrides from the plugin's config_yaml onto the builtin
/// table: entries replace by id (context/name fields optional), new GLM ids
/// append. Non-GLM entries are ignored so config cannot widen the allowlist.
/// Unparsable config falls back to the base table (logged by the caller).
pub fn merge_model_overrides(base: Vec<ModelInfo>, config_yaml: &[u8]) -> Vec<ModelInfo> {
    let parsed: serde_yaml::Value = match serde_yaml::from_slice(config_yaml) {
        Ok(v) => v,
        Err(_) => return base,
    };
    let items = match parsed.get("models").and_then(|m| m.as_sequence()) {
        Some(seq) => seq.clone(),
        None => return base,
    };
    let overrides: Vec<ModelOverride> = items
        .iter()
        .filter_map(|item| serde_yaml::from_value(item.clone()).ok())
        .collect();
    if overrides.is_empty() {
        return base;
    }
    let mut out = base;
    for ov in overrides {
        if !is_glm_model(&ov.id) {
            continue;
        }
        if let Some(existing) = out.iter_mut().find(|m| m.id == ov.id) {
            if let Some(ctx) = ov.context {
                existing.context_length = ctx;
            }
            if let Some(name) = ov.name {
                existing.display_name = name;
            }
        } else {
            out.push(ModelInfo {
                id: ov.id.clone(),
                object: "model".into(),
                owned_by: PROVIDER_NAME.into(),
                display_name: ov.name.unwrap_or_else(|| ov.id.clone()),
                name: ov.id,
                methods: vec!["chat".into()],
                context_length: ov.context.unwrap_or(131_072),
                max_completion_tokens: 8192,
                user_defined: true,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_repo_points_at_this_repo() {
        // The plugin-store card links whatever GitHubRepository the
        // registration metadata carries. The value used to be inherited from
        // the Go upstream and pointed at someone else's repo — pin ours.
        let reg = default_registration();
        assert_eq!(reg.metadata.repo, "https://github.com/xinzengxx/workbuddy-cpa");
        assert_eq!(reg.metadata.version, env!("CARGO_PKG_VERSION"));
        // Plugin id must differ from the registry-occupied "workbuddy" slot.
        assert_eq!(reg.metadata.name, PLUGIN_ID);
        assert_ne!(reg.metadata.name, PROVIDER_NAME);
        assert!(!reg.metadata.author.is_empty());
    }

    #[test]
    fn builtin_models_are_glm_only() {
        let models = builtin_models();
        assert_eq!(models.len(), 5);
        assert!(models.iter().all(|m| is_glm_model(&m.id)));
        assert!(models.iter().all(|m| m.owned_by == PROVIDER_NAME));
    }

    #[test]
    fn require_glm_model_strips_prefix_and_rejects_other_families() {
        assert_eq!(require_glm_model("glm-5.2").unwrap(), "glm-5.2");
        assert_eq!(require_glm_model("workbuddy/glm-5.3").unwrap(), "glm-5.3");
        assert_eq!(require_glm_model("").unwrap(), "");
        assert!(require_glm_model("deepseek-v4-pro").is_err());
        assert!(require_glm_model("kimi-k3").is_err());
    }

    #[test]
    fn merge_override_replaces_context_and_appends_new_glm() {
        let base = builtin_models();
        let yaml = b"enabled: true\npriority: 100\nmodels:\n  - id: glm-5.3-flash\n    context: 2000000\n  - id: glm-5.4\n    name: GLM 5.4\n  - id: kimi-k3\n    name: Kimi K3\n";
        let merged = merge_model_overrides(base, yaml);
        let f = merged.iter().find(|m| m.id == "glm-5.3-flash").unwrap();
        assert_eq!(f.context_length, 2_000_000);
        let new = merged.iter().find(|m| m.id == "glm-5.4").unwrap();
        assert_eq!(new.display_name, "GLM 5.4");
        assert!(!merged.iter().any(|m| m.id == "kimi-k3"), "non-GLM override must be ignored");
        assert_eq!(merged.len(), 6);
    }

    #[test]
    fn bad_yaml_falls_back() {
        let base = builtin_models();
        let merged = merge_model_overrides(base.clone(), b"\t-bad:[");
        assert_eq!(merged.len(), base.len());
    }

    #[test]
    fn no_models_key_falls_back() {
        let base = builtin_models();
        let merged = merge_model_overrides(base.clone(), b"enabled: true\n");
        assert_eq!(merged.len(), base.len());
    }
}
