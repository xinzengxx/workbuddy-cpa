use crate::rpc::{Capabilities, Metadata, ModelInfo, Registration};

pub const PROVIDER_NAME: &str = "workbuddy";

pub fn default_registration() -> Registration {
    Registration {
        schema_version: crate::SCHEMA_VERSION,
        metadata: Metadata {
            name: PROVIDER_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "xinyuan (Rust rewrite; original workbuddy plugin by Sliverkiss)".into(),
            repo: "https://github.com/lovingfish/workbuddy-cliproxy".into(),
        },
        capabilities: Capabilities {
            model_provider: true,
            auth_provider: true,
            executor: true,
            executor_model_scope: "both",
            input_formats: vec!["chat-completions"],
            output_formats: vec!["chat-completions"],
            management_api: true,
        },
    }
}

/// Builtin model table, identical to the Go plugin's snapshot plus
/// glm-5.3-flash. Config overrides (models: key in the plugin's config_yaml)
/// are merged on top at register/reconfigure time.
pub fn builtin_models() -> Vec<ModelInfo> {
    let specs: &[(&str, &str, i64)] = &[
        ("glm-5.2", "GLM-5.2", 1_000_000),
        ("glm-5.3-flash", "GLM-5.3 Flash", 1_000_000),
        ("glm-5.1", "GLM-5.1", 131_072),
        ("glm-5v-turbo", "GLM-5V Turbo", 131_072),
        ("kimi-k2.7", "Kimi K2.7", 262_144),
        ("minimax-m3-pay", "MiniMax M3", 204_800),
        ("hy3", "Hy3", 262_144),
        ("hy3-preview", "Hy3 Preview", 262_144),
        ("hy3-preview-agent", "Hy3 Preview Agent", 262_144),
        ("deepseek-v4-pro", "DeepSeek V4 Pro", 1_000_000),
        ("deepseek-v4-flash", "DeepSeek V4 Flash", 1_000_000),
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
/// table: entries replace by id (context/name fields optional), new ids append.
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
    fn merge_override_replaces_context_and_appends_new() {
        let base = builtin_models();
        let yaml = b"enabled: true\npriority: 100\nmodels:\n  - id: glm-5.3-flash\n    context: 2000000\n  - id: kimi-k3\n    name: Kimi K3\n";
        let merged = merge_model_overrides(base, yaml);
        let f = merged.iter().find(|m| m.id == "glm-5.3-flash").unwrap();
        assert_eq!(f.context_length, 2_000_000);
        let k3 = merged.iter().find(|m| m.id == "kimi-k3").unwrap();
        assert_eq!(k3.display_name, "Kimi K3");
        assert_eq!(merged.len(), 12);
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
