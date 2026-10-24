//! Realm (region) constants and per-realm predicates.
//!
//! Every regional difference lives here and nowhere else. The two builds are
//! mutually exclusive (`#[cfg]`), so exactly one realm exists per compilation
//! and using a constant from the other realm is a compile error rather than a
//! production 401.

/// One entry of the builtin model table.
pub struct ModelSpec {
    pub id: &'static str,
    pub display: &'static str,
    pub context: i64,
    pub max_completion: i64,
}

/// Everything that differs between the domestic (CN) and the international
/// (Global) CodeBuddy realm. Adding a field here is the only sanctioned way to
/// introduce a regional difference.
pub struct Realm {
    /// Plugin (file) identity — must equal the dylib file name stem.
    pub plugin_id: &'static str,
    /// Provider identity: `auth.identifier`, `executor.identifier`,
    /// credential `type`, model `owned_by` and management route segment.
    pub provider: &'static str,
    /// Upstream API base, e.g. `https://copilot.tencent.com`.
    pub base: &'static str,
    /// Web origin used for `Origin`/`Referer` headers.
    pub origin: &'static str,
    /// Credential file name prefix, e.g. `workbuddy-`.
    pub credential_prefix: &'static str,
    /// Prefix owned by the OTHER realm; never claim a file starting with it.
    /// CN's prefix (`workbuddy-`) is a strict prefix of INTL's, so without this
    /// the CN plugin would happily claim every international credential.
    pub foreign_prefix: &'static str,
    /// Value written to the credential file's `type` field.
    pub credential_type: &'static str,
    /// Plugin-managed state file name (disabled list).
    pub state_file: &'static str,
    /// Prefix for synthesized non-streaming completion ids.
    pub completion_prefix: &'static str,
    /// Builtin model table.
    pub models: &'static [ModelSpec],
    /// Whether the daily check-in routes exist in this realm.
    pub checkin: bool,
    /// Whether to inject a minimal system message when the client sent none.
    pub inject_system: bool,
    /// Management panel menu label.
    pub panel_menu: &'static str,
    /// Panel document title / header brand.
    pub panel_title: &'static str,
}

impl Realm {
    /// Does this realm own the credential file with that on-disk name?
    ///
    /// The check-in module, the auth parser and the panel's account filter all
    /// ask this same question, so it lives in one place.
    pub fn owns_credential_file(&self, file_name: &str) -> bool {
        let name = file_name.trim();
        if !self.foreign_prefix.is_empty() && name.starts_with(self.foreign_prefix) {
            return false;
        }
        name.starts_with(self.credential_prefix) || name == self.legacy_file_name()
    }

    /// Legacy shared credential name (used when the account has no uid).
    pub fn legacy_file_name(&self) -> String {
        format!("{}.json", self.credential_prefix.trim_end_matches('-'))
    }
}

/// The realm this build targets.
#[cfg(not(feature = "intl"))]
pub fn realm() -> &'static Realm {
    static R: Realm = Realm {
        plugin_id: "workbuddy-cpa",
        provider: "workbuddy",
        base: "https://copilot.tencent.com",
        origin: "https://www.codebuddy.cn",
        credential_prefix: "workbuddy-",
        foreign_prefix: "workbuddy-intl",
        credential_type: "workbuddy",
        state_file: "workbuddy-state.json",
        completion_prefix: "chatcmpl-workbuddy",
        models: &[
            ModelSpec { id: "glm-5.2", display: "GLM-5.2", context: 1_000_000, max_completion: 8192 },
            ModelSpec { id: "glm-5.3-flash", display: "GLM-5.3 Flash", context: 1_000_000, max_completion: 8192 },
            ModelSpec { id: "glm-5.1", display: "GLM-5.1", context: 131_072, max_completion: 8192 },
            ModelSpec { id: "glm-5v-turbo", display: "GLM-5V Turbo", context: 131_072, max_completion: 8192 },
            ModelSpec { id: "glm-5.3", display: "GLM-5.3", context: 131_072, max_completion: 8192 },
        ],
        checkin: true,
        inject_system: false,
        panel_menu: "WorkBuddy",
        panel_title: "WorkBuddy",
    };
    &R
}

/// The realm this build targets (international / Global).
#[cfg(feature = "intl")]
pub fn realm() -> &'static Realm {
    static R: Realm = Realm {
        plugin_id: "workbuddy-intl-cpa",
        provider: "workbuddy-intl",
        base: "https://www.workbuddy.ai",
        origin: "https://www.workbuddy.ai",
        credential_prefix: "workbuddy-intl-",
        foreign_prefix: "",
        credential_type: "workbuddy-intl",
        state_file: "workbuddy-intl-state.json",
        completion_prefix: "chatcmpl-workbuddy-intl",
        models: &[
            ModelSpec {
                id: "deepseek-v4.1-flash",
                display: "DeepSeek V4.1 Flash",
                context: 131_072,
                max_completion: 8192,
            },
        ],
        checkin: false,
        inject_system: true,
        panel_menu: "WorkBuddy 国际版",
        panel_title: "WorkBuddy 国际版",
    };
    &R
}

/// Is this model id servable by the current realm?
pub fn model_allowed(id: &str) -> bool {
    let id = id.trim();
    #[cfg(not(feature = "intl"))]
    {
        id.to_ascii_lowercase().starts_with("glm-")
    }
    #[cfg(feature = "intl")]
    {
        id.eq_ignore_ascii_case("deepseek-v4.1-flash")
    }
}

/// `workbuddy-abcdef.json` -> `abcdef`; anything else keeps its stem.
pub fn display_name_from_file(file_name: &str) -> String {
    let stem = file_name.strip_suffix(".json").unwrap_or(file_name);
    stem.strip_prefix(realm().credential_prefix).unwrap_or(stem).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_build() {
        let r = realm();
        #[cfg(not(feature = "intl"))]
        {
            assert_eq!(r.plugin_id, "workbuddy-cpa");
            assert_eq!(r.provider, "workbuddy");
            assert_eq!(r.base, "https://copilot.tencent.com");
            assert_eq!(r.origin, "https://www.codebuddy.cn");
            assert_eq!(r.credential_type, "workbuddy");
            assert_eq!(r.state_file, "workbuddy-state.json");
            assert_eq!(r.completion_prefix, "chatcmpl-workbuddy");
            assert!(r.checkin);
            assert!(!r.inject_system);
            assert_eq!(r.models.len(), 5);
        }
        #[cfg(feature = "intl")]
        {
            assert_eq!(r.plugin_id, "workbuddy-intl-cpa");
            assert_eq!(r.provider, "workbuddy-intl");
            assert_eq!(r.base, "https://www.workbuddy.ai");
            assert_eq!(r.origin, "https://www.workbuddy.ai");
            assert_eq!(r.credential_type, "workbuddy-intl");
            assert_eq!(r.state_file, "workbuddy-intl-state.json");
            assert_eq!(r.completion_prefix, "chatcmpl-workbuddy-intl");
            assert!(!r.checkin);
            assert!(r.inject_system);
            assert_eq!(r.models.len(), 1);
            assert_eq!(r.models[0].id, "deepseek-v4.1-flash");
        }
    }

    #[test]
    fn model_allowlist_is_realm_specific() {
        #[cfg(not(feature = "intl"))]
        {
            assert!(model_allowed("glm-5.3-flash"));
            assert!(!model_allowed("deepseek-v4.1-flash"));
        }
        #[cfg(feature = "intl")]
        {
            assert!(model_allowed("deepseek-v4.1-flash"));
            assert!(!model_allowed("glm-5.3-flash"));
            assert!(!model_allowed("deepseek-v4.1-flash-intl"));
        }
    }

    #[test]
    fn credential_ownership_never_overlaps() {
        let r = realm();
        assert!(r.owns_credential_file(&format!("{}abcdef.json", r.credential_prefix)));
        assert!(r.owns_credential_file(&r.legacy_file_name()));
        let foreign = if r.provider == "workbuddy" {
            "workbuddy-intl-abcdef.json"
        } else {
            "workbuddy-abcdef.json"
        };
        assert!(!r.owns_credential_file(foreign));
        assert!(!r.owns_credential_file("qoderwork.json"));
    }

    #[test]
    fn cn_does_not_claim_the_intl_legacy_file() {
        // `workbuddy-intl.json` 不带尾随短横，只靠 "workbuddy-" 前缀会被 CN 吞掉，
        // 所以 foreign_prefix 必须写成 "workbuddy-intl"（无尾横）。
        let r = realm();
        if r.provider == "workbuddy" {
            assert!(!r.owns_credential_file("workbuddy-intl.json"));
            assert!(!r.owns_credential_file("workbuddy-intl-abc.json"));
            assert!(r.owns_credential_file("workbuddy.json"));
        } else {
            assert!(r.owns_credential_file("workbuddy-intl.json"));
            assert!(!r.owns_credential_file("workbuddy.json"));
        }
    }

    #[test]
    fn legacy_and_display_names_use_own_prefix() {
        let r = realm();
        assert_eq!(
            r.legacy_file_name(),
            format!("{}.json", r.credential_prefix.trim_end_matches('-'))
        );
        assert_eq!(
            display_name_from_file(&format!("{}abcdef.json", r.credential_prefix)),
            "abcdef"
        );
        assert_eq!(display_name_from_file("plain.json"), "plain");
    }
}
