use std::collections::BTreeMap;

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct OldStoredTarget {
    pub(crate) dir: Option<String>,
    pub(crate) script: Option<String>,
    pub(crate) steps: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
}

/// T3.6 compatibility fixture: the pre-protocol request parser used
/// `flatten` and silently accepted unknown fields.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct OldRegisterRequest {
    pub(crate) name: String,
    #[serde(flatten)]
    pub(crate) target: OldStoredTarget,
}
