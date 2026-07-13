use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
}

/// Server-side UI settings a client needs to render a config-faithful
/// sidebar for this server's spaces and agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UiSettingsInfo {
    pub sidebar_width: u16,
    pub sidebar_default_width: u16,
    pub sidebar_min_width: u16,
    pub sidebar_max_width: u16,
    pub sidebar_section_split_per_mille: u16,
    pub sidebar_spaces: crate::config::SpacesSidebarConfig,
    pub sidebar_agents: crate::config::AgentsSidebarConfig,
    pub sidebar_host: crate::config::SidebarHostConfig,
}

impl Default for UiSettingsInfo {
    fn default() -> Self {
        let ui = crate::config::Config::default().ui;
        Self {
            sidebar_width: ui.sidebar_width,
            sidebar_default_width: ui.sidebar_width,
            sidebar_min_width: ui.sidebar_min_width,
            sidebar_max_width: ui.sidebar_max_width,
            sidebar_section_split_per_mille: 500,
            sidebar_spaces: ui.sidebar.spaces,
            sidebar_agents: ui.sidebar.agents,
            sidebar_host: ui.sidebar.host,
        }
    }
}

impl UiSettingsInfo {
    pub(crate) fn sidebar_section_split(&self) -> f32 {
        (self.sidebar_section_split_per_mille as f32 / 1000.0).clamp(0.1, 0.9)
    }
}
