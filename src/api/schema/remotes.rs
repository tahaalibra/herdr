use serde::{Deserialize, Serialize};

use crate::remote_registry::RemoteKeybindingsSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoteAddParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub target: String,
    #[serde(default)]
    pub keybindings: RemoteKeybindingsSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoteRemoveParams {
    pub remote_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoteRenameParams {
    pub remote_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoteSetEnabledParams {
    pub remote_id: String,
    pub enabled: bool,
}
