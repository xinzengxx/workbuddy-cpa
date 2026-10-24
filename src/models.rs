use crate::realm::{model_allowed, realm};
use crate::rpc::{Capabilities, Metadata, ModelInfo, Registration};

pub fn default_registration() -> Registration {
    Registration {
        schema_version: crate::SCHEMA_VERSION,
        metadata: Metadata {
            // The plugin id must equal the dylib file name stem. It is
            // deliberately DIFFERENT from the provider name: the plugin-store
            // matches installed file IDs against registry entries, and the
            // official registry's "workbuddy" slot belongs to someone else's
            // plugin (zidanfaqih/codebuddy-intl-cpa) — that collision made the
            // store show their repo and offer to "update" over our build. The
            // provider / credential / model identity stays realm().provider:
            // auth.identifier, executor.identifier, model owned_by and the
            // credential files are all unaffected.
            name: realm().plugin_id.into(),
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

/// The host may pass "workbuddy/glm-5.2" or a bare id. Strip a provider prefix
/// and reject anything outside this realm's allowlist before an upstream
/// request is made — CPA routes by model id, so a foreign id reaching us means
/// the host picked the wrong credential.
pub fn require_model(model: &str) -> Result<String, String> {
    let bare = model.rsplit('/').next().unwrap_or(model).trim();
    if bare.is_empty() {
        return Ok(String::new());
    }
    if model_allowed(bare) {
        Ok(bare.to_string())
    } else {
        let ids: Vec<&str> = realm().models.iter().map(|m| m.id).collect();
        Err(format!(
            "model_not_supported: {} serves {} only (got {model})",
            realm().provider,
            ids.join(", ")
        ))
    }
}

/// Builtin model table comes straight from the realm. Config overrides (the
/// `models:` key) may tune or add entries, never leave the allowlist.
pub fn builtin_models() -> Vec<ModelInfo> {
    realm()
        .models
        .iter()
        .map(|spec| ModelInfo {
            id: spec.id.to_string(),
            object: "model".into(),
            owned_by: realm().provider.into(),
            display_name: spec.display.to_string(),
            name: spec.id.to_string(),
            methods: vec!["chat".into()],
            context_length: spec.context,
            max_completion_tokens: spec.max_completion,
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
/// table: entries replace by id (context/name fields optional), new ids append
/// only when the realm allowlist accepts them. Foreign ids are ignored so
/// config cannot widen the allowlist. Unparsable config falls back to the base
/// table (logged by the caller).
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
        if !model_allowed(&ov.id) {
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
                owned_by: realm().provider.into(),
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
    fn registration_identity_follows_realm() {
        let reg = default_registration();
        assert_eq!(reg.metadata.repo, "https://github.com/xinzengxx/workbuddy-cpa");
        assert_eq!(reg.metadata.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(reg.metadata.name, realm().plugin_id);
        assert_ne!(reg.metadata.name, realm().provider, "plugin id must differ from provider name");
        assert!(!reg.metadata.author.is_empty());
    }

    #[test]
    fn builtin_models_follow_realm() {
        let models = builtin_models();
        assert_eq!(models.len(), realm().models.len());
        for (m, spec) in models.iter().zip(realm().models) {
            assert_eq!(m.id, spec.id);
            assert_eq!(m.context_length, spec.context);
            assert_eq!(m.max_completion_tokens, spec.max_completion);
            assert_eq!(m.owned_by, realm().provider);
        }
    }

    #[test]
    fn require_model_strips_prefix_and_rejects_foreign_family() {
        let id = realm().models[0].id;
        assert_eq!(require_model(id).unwrap(), id);
        assert_eq!(require_model(&format!("{}/{}", realm().provider, id)).unwrap(), id);
        assert_eq!(require_model("").unwrap(), "");
        let foreign = if realm().provider == "workbuddy" { "deepseek-v4.1-flash" } else { "glm-5.2" };
        assert!(require_model(foreign).is_err());
    }

    #[test]
    fn overrides_tune_own_models_and_ignore_foreign() {
        let base = builtin_models();
        let own = realm().models[0].id;
        let foreign = if realm().provider == "workbuddy" { "deepseek-v4.1-flash" } else { "glm-5.2" };
        let yaml = format!(
            "enabled: true\nmodels:\n  - id: {own}\n    context: 2000000\n  - id: {foreign}\n    name: Nope\n"
        );
        let merged = merge_model_overrides(base.clone(), yaml.as_bytes());
        assert_eq!(merged.len(), base.len(), "foreign override must be ignored");
        assert_eq!(merged[0].context_length, 2_000_000);
        assert_eq!(merged[0].owned_by, realm().provider);
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
