use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::detect::Agent;

const MAX_SIDEBAR_ROWS: usize = 16;
const MAX_SIDEBAR_TOKENS_PER_ROW: usize = 16;

fn deserialize_sidebar_rows<'de, D, T>(deserializer: D) -> Result<Vec<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let rows = Vec::<Vec<T>>::deserialize(deserializer)?;
    validate_sidebar_rows(&rows).map_err(serde::de::Error::custom)?;
    Ok(rows)
}

fn validate_sidebar_rows<T>(rows: &[Vec<T>]) -> Result<(), String> {
    if rows.len() > MAX_SIDEBAR_ROWS {
        return Err(format!(
            "sidebar layouts may contain at most {MAX_SIDEBAR_ROWS} rows"
        ));
    }
    if rows
        .iter()
        .any(|row| row.len() > MAX_SIDEBAR_TOKENS_PER_ROW)
    {
        return Err(format!(
            "sidebar rows may contain at most {MAX_SIDEBAR_TOKENS_PER_ROW} tokens"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSidebarToken {
    StateIcon,
    StateText,
    Workspace,
    Tab,
    Pane,
    Agent,
    TerminalTitle,
    TerminalTitleStripped,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpaceSidebarToken {
    StateIcon,
    StateText,
    Workspace,
    Branch,
    GitStatus,
    Custom(String),
}

fn parse_sidebar_token<'de, D, T>(deserializer: D, builtins: &[(&str, T)]) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Clone + From<String>,
{
    let value = String::deserialize(deserializer)?;
    if let Some((_, token)) = builtins.iter().find(|(name, _)| *name == value) {
        return Ok(token.clone());
    }
    let Some(name) = value.strip_prefix('$') else {
        return Err(serde::de::Error::custom(format!(
            "unknown sidebar token `{value}`; custom tokens must start with `$`"
        )));
    };
    if name.is_empty()
        || name.len() > 32
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(serde::de::Error::custom(format!(
            "invalid custom sidebar token `{value}`"
        )));
    }
    Ok(T::from(name.to_string()))
}

impl Serialize for AgentSidebarToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::StateIcon => serializer.serialize_str("state_icon"),
            Self::StateText => serializer.serialize_str("state_text"),
            Self::Workspace => serializer.serialize_str("workspace"),
            Self::Tab => serializer.serialize_str("tab"),
            Self::Pane => serializer.serialize_str("pane"),
            Self::Agent => serializer.serialize_str("agent"),
            Self::TerminalTitle => serializer.serialize_str("terminal_title"),
            Self::TerminalTitleStripped => serializer.serialize_str("terminal_title_stripped"),
            Self::Custom(name) => serializer.serialize_str(&format!("${name}")),
        }
    }
}

impl From<String> for AgentSidebarToken {
    fn from(value: String) -> Self {
        Self::Custom(value)
    }
}

fn sidebar_token_schema() -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "description": "A builtin sidebar token name, or a `$`-prefixed custom metadata token"
    })
}

impl schemars::JsonSchema for AgentSidebarToken {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AgentSidebarToken".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        sidebar_token_schema()
    }
}

impl schemars::JsonSchema for SpaceSidebarToken {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SpaceSidebarToken".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        sidebar_token_schema()
    }
}

impl<'de> Deserialize<'de> for AgentSidebarToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        parse_sidebar_token(
            deserializer,
            &[
                ("state_icon", Self::StateIcon),
                ("state_text", Self::StateText),
                ("workspace", Self::Workspace),
                ("tab", Self::Tab),
                ("pane", Self::Pane),
                ("agent", Self::Agent),
                ("terminal_title", Self::TerminalTitle),
                ("terminal_title_stripped", Self::TerminalTitleStripped),
            ],
        )
    }
}

impl Serialize for SpaceSidebarToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::StateIcon => serializer.serialize_str("state_icon"),
            Self::StateText => serializer.serialize_str("state_text"),
            Self::Workspace => serializer.serialize_str("workspace"),
            Self::Branch => serializer.serialize_str("branch"),
            Self::GitStatus => serializer.serialize_str("git_status"),
            Self::Custom(name) => serializer.serialize_str(&format!("${name}")),
        }
    }
}

impl From<String> for SpaceSidebarToken {
    fn from(value: String) -> Self {
        Self::Custom(value)
    }
}

impl<'de> Deserialize<'de> for SpaceSidebarToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        parse_sidebar_token(
            deserializer,
            &[
                ("state_icon", Self::StateIcon),
                ("state_text", Self::StateText),
                ("workspace", Self::Workspace),
                ("branch", Self::Branch),
                ("git_status", Self::GitStatus),
            ],
        )
    }
}

type AgentSidebarRows = Vec<Vec<AgentSidebarToken>>;
type SpaceSidebarRows = Vec<Vec<SpaceSidebarToken>>;

fn deserialize_rows_by_agent<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, AgentSidebarRows>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows_by_agent = BTreeMap::<String, AgentSidebarRows>::deserialize(deserializer)?;
    for (id, rows) in &rows_by_agent {
        if crate::detect::parse_canonical_agent_label(id).is_none() {
            return Err(serde::de::Error::custom(format!(
                "unknown canonical agent id `{id}` in sidebar rows_by_agent"
            )));
        }
        validate_sidebar_rows(rows).map_err(serde::de::Error::custom)?;
    }
    Ok(rows_by_agent)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AgentsSidebarConfig {
    #[serde(deserialize_with = "deserialize_sidebar_rows")]
    pub rows: AgentSidebarRows,
    #[serde(default, deserialize_with = "deserialize_rows_by_agent")]
    pub rows_by_agent: BTreeMap<String, AgentSidebarRows>,
}

impl AgentsSidebarConfig {
    pub(crate) fn rows_for_agent(&self, agent: Option<Agent>) -> &AgentSidebarRows {
        agent
            .and_then(|agent| self.rows_by_agent.get(crate::detect::agent_label(agent)))
            .unwrap_or(&self.rows)
    }
}

impl Default for AgentsSidebarConfig {
    fn default() -> Self {
        Self {
            rows: vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Workspace,
                    AgentSidebarToken::Tab,
                ],
                vec![AgentSidebarToken::Agent],
            ],
            rows_by_agent: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SpacesSidebarConfig {
    #[serde(deserialize_with = "deserialize_sidebar_rows")]
    pub rows: SpaceSidebarRows,
}

impl Default for SpacesSidebarConfig {
    fn default() -> Self {
        Self {
            rows: vec![
                vec![SpaceSidebarToken::StateIcon, SpaceSidebarToken::Workspace],
                vec![SpaceSidebarToken::Branch, SpaceSidebarToken::GitStatus],
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub agents: AgentsSidebarConfig,
    pub spaces: SpacesSidebarConfig,
    pub host: SidebarHostConfig,
}

/// Host-banner sidebar configuration. Styles the per-host banner row that
/// sits above each remote host's spaces in a multi-server sidebar. There is
/// no off switch — the banner is always drawn for remote hosts; only its
/// presentation is configured here.
///
/// Deserialization is hand-written via [`RawSidebarHostConfig`] so unknown
/// TOML enum values fall back to the documented defaults instead of failing
/// the parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SidebarHostConfig {
    pub gradient: HostBannerGradient,
    pub animation: HostBannerAnimation,
    pub speed: HostBannerSpeed,
    pub glyph: HostBannerGlyph,
    pub show_count: bool,
}

impl Default for SidebarHostConfig {
    fn default() -> Self {
        Self {
            gradient: HostBannerGradient::Rainbow,
            animation: HostBannerAnimation::Animated,
            speed: HostBannerSpeed::Calm,
            glyph: HostBannerGlyph::Left,
            show_count: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostBannerGradient {
    Rainbow,
    Accent,
    Cool,
    Warm,
    Muted,
}

impl HostBannerGradient {
    pub fn next(self) -> Self {
        match self {
            Self::Rainbow => Self::Accent,
            Self::Accent => Self::Cool,
            Self::Cool => Self::Warm,
            Self::Warm => Self::Muted,
            Self::Muted => Self::Rainbow,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rainbow => "rainbow",
            Self::Accent => "accent",
            Self::Cool => "cool",
            Self::Warm => "warm",
            Self::Muted => "muted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostBannerAnimation {
    Animated,
    Static,
}

impl HostBannerAnimation {
    pub fn next(self) -> Self {
        match self {
            Self::Animated => Self::Static,
            Self::Static => Self::Animated,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Animated => "animated",
            Self::Static => "static",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostBannerSpeed {
    Calm,
    Normal,
    Lively,
}

impl HostBannerSpeed {
    pub fn next(self) -> Self {
        match self {
            Self::Calm => Self::Normal,
            Self::Normal => Self::Lively,
            Self::Lively => Self::Calm,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Normal => "normal",
            Self::Lively => "lively",
        }
    }

    /// Per-tick phase drift used by the lolcat gradient animation. `Calm < Normal < Lively`.
    pub fn drift(self) -> f32 {
        match self {
            Self::Calm => 0.04,
            Self::Normal => 0.09,
            Self::Lively => 0.16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostBannerGlyph {
    Left,
    None,
}

impl HostBannerGlyph {
    pub fn next(self) -> Self {
        match self {
            Self::Left => Self::None,
            Self::None => Self::Left,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::None => "none",
        }
    }
}

/// Raw intermediate for [`SidebarHostConfig`] deserialization. Every enum field is parsed
/// through a `parse_host_*` helper whose final arm yields the default, so unknown /
/// missing values degrade to defaults instead of rejecting the config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawSidebarHostConfig {
    gradient: Option<String>,
    animation: Option<String>,
    speed: Option<String>,
    glyph: Option<String>,
    show_count: Option<bool>,
}

impl<'de> Deserialize<'de> for SidebarHostConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawSidebarHostConfig::deserialize(deserializer)?;
        Ok(SidebarHostConfig {
            gradient: parse_host_gradient(raw.gradient.as_deref()),
            animation: parse_host_animation(raw.animation.as_deref()),
            speed: parse_host_speed(raw.speed.as_deref()),
            glyph: parse_host_glyph(raw.glyph.as_deref()),
            show_count: raw.show_count.unwrap_or(false),
        })
    }
}

fn parse_host_gradient(value: Option<&str>) -> HostBannerGradient {
    match value {
        Some("accent") => HostBannerGradient::Accent,
        Some("cool") => HostBannerGradient::Cool,
        Some("warm") => HostBannerGradient::Warm,
        Some("muted") => HostBannerGradient::Muted,
        _ => HostBannerGradient::Rainbow,
    }
}

fn parse_host_animation(value: Option<&str>) -> HostBannerAnimation {
    match value {
        Some("static") => HostBannerAnimation::Static,
        _ => HostBannerAnimation::Animated,
    }
}

fn parse_host_speed(value: Option<&str>) -> HostBannerSpeed {
    match value {
        Some("normal") => HostBannerSpeed::Normal,
        Some("lively") => HostBannerSpeed::Lively,
        _ => HostBannerSpeed::Calm,
    }
}

fn parse_host_glyph(value: Option<&str>) -> HostBannerGlyph {
    match value {
        Some("none") => HostBannerGlyph::None,
        _ => HostBannerGlyph::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_compact_agent_and_existing_space_layouts() {
        let config = SidebarConfig::default();
        assert_eq!(
            config.agents.rows,
            vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Workspace,
                    AgentSidebarToken::Tab,
                ],
                vec![AgentSidebarToken::Agent],
            ]
        );
        assert!(config.agents.rows_by_agent.is_empty());
        assert_eq!(
            config.spaces.rows,
            vec![
                vec![SpaceSidebarToken::StateIcon, SpaceSidebarToken::Workspace],
                vec![SpaceSidebarToken::Branch, SpaceSidebarToken::GitStatus],
            ]
        );
    }

    #[test]
    fn parses_builtin_and_arbitrary_custom_tokens() {
        let config: crate::config::Config = toml::from_str(
            r#"
[ui.sidebar.agents]
rows = [["state_icon", "workspace"], ["state_text", "agent", "$summary"], ["terminal_title", "terminal_title_stripped", "$terminal_title"]]

[ui.sidebar.agents.rows_by_agent]
claude = [["terminal_title_stripped"], ["agent", "$model"]]

[ui.sidebar.spaces]
rows = [["workspace"], ["$jj_status"]]
"#,
        )
        .expect("sidebar token config");

        assert_eq!(
            config.ui.sidebar.agents.rows[1],
            vec![
                AgentSidebarToken::StateText,
                AgentSidebarToken::Agent,
                AgentSidebarToken::Custom("summary".into()),
            ]
        );
        assert_eq!(
            config.ui.sidebar.agents.rows[2],
            vec![
                AgentSidebarToken::TerminalTitle,
                AgentSidebarToken::TerminalTitleStripped,
                AgentSidebarToken::Custom("terminal_title".into()),
            ]
        );
        assert_eq!(
            config.ui.sidebar.agents.rows_by_agent["claude"],
            vec![
                vec![AgentSidebarToken::TerminalTitleStripped],
                vec![
                    AgentSidebarToken::Agent,
                    AgentSidebarToken::Custom("model".into()),
                ],
            ]
        );
        assert_eq!(
            config.ui.sidebar.spaces.rows[1],
            vec![SpaceSidebarToken::Custom("jj_status".into())]
        );
    }

    #[test]
    fn rejects_unknown_bare_and_malformed_custom_tokens() {
        for token in ["summary", "$", "$bad.name"] {
            let input = format!("[ui.sidebar.agents]\\nrows = [[\"{token}\"]]\\n");
            assert!(toml::from_str::<crate::config::Config>(&input).is_err());
        }
    }

    #[test]
    fn rejects_oversized_sidebar_layouts() {
        let too_many_rows = std::iter::repeat_n("[\"agent\"]", MAX_SIDEBAR_ROWS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let input = format!("[ui.sidebar.agents]\nrows = [{too_many_rows}]\n");
        assert!(toml::from_str::<crate::config::Config>(&input).is_err());

        let too_many_tokens = std::iter::repeat_n("\"workspace\"", MAX_SIDEBAR_TOKENS_PER_ROW + 1)
            .collect::<Vec<_>>()
            .join(",");
        let input = format!("[ui.sidebar.spaces]\nrows = [[{too_many_tokens}]]\n");
        assert!(toml::from_str::<crate::config::Config>(&input).is_err());

        let input = format!("[ui.sidebar.agents.rows_by_agent]\nclaude = [{too_many_rows}]\n");
        assert!(toml::from_str::<crate::config::Config>(&input).is_err());
    }

    #[test]
    fn accepts_every_canonical_agent_override_key() {
        let agents = [
            Agent::Pi,
            Agent::Claude,
            Agent::Codex,
            Agent::Gemini,
            Agent::Cursor,
            Agent::Devin,
            Agent::Antigravity,
            Agent::Cline,
            Agent::Omp,
            Agent::Mastracode,
            Agent::OpenCode,
            Agent::GithubCopilot,
            Agent::Kimi,
            Agent::Kiro,
            Agent::Droid,
            Agent::Amp,
            Agent::Grok,
            Agent::Hermes,
            Agent::Kilo,
            Agent::Qodercli,
            Agent::Maki,
        ];
        let entries = agents
            .iter()
            .map(|agent| format!("{} = [[\"agent\"]]", crate::detect::agent_label(*agent)))
            .collect::<Vec<_>>()
            .join("\n");
        let input = format!("[ui.sidebar.agents.rows_by_agent]\n{entries}\n");
        let config: crate::config::Config = toml::from_str(&input).expect("canonical keys");

        assert_eq!(config.ui.sidebar.agents.rows_by_agent.len(), agents.len());
    }

    #[test]
    fn rejects_alias_case_whitespace_and_unknown_override_keys() {
        for key in ["claude-code", "Claude", "' claude '", "unknown"] {
            let input = format!("[ui.sidebar.agents.rows_by_agent]\n{key} = [[\"agent\"]]\n");
            assert!(
                toml::from_str::<crate::config::Config>(&input).is_err(),
                "accepted key {key:?}"
            );
        }
    }

    #[test]
    fn sidebar_host_config_default() {
        let host = SidebarHostConfig::default();
        assert_eq!(host.gradient, HostBannerGradient::Rainbow);
        assert_eq!(host.animation, HostBannerAnimation::Animated);
        assert_eq!(host.speed, HostBannerSpeed::Calm);
        assert_eq!(host.glyph, HostBannerGlyph::Left);
        assert!(!host.show_count);
        // A config with no `[ui.sidebar.host]` table yields the default.
        let config = crate::config::Config::default();
        assert_eq!(config.ui.sidebar.host, SidebarHostConfig::default());
    }

    #[test]
    fn sidebar_host_partial_toml_falls_back() {
        let toml = r#"
[ui.sidebar.host]
glyph = "none"
"#;
        let config: crate::config::Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.sidebar.host.glyph, HostBannerGlyph::None);
        // The other four keep their defaults.
        assert_eq!(config.ui.sidebar.host.gradient, HostBannerGradient::Rainbow);
        assert_eq!(
            config.ui.sidebar.host.animation,
            HostBannerAnimation::Animated
        );
        assert_eq!(config.ui.sidebar.host.speed, HostBannerSpeed::Calm);
        assert!(!config.ui.sidebar.host.show_count);
    }

    #[test]
    fn sidebar_host_unknown_enum_values_fall_back_to_defaults() {
        let toml = r#"
[ui.sidebar.host]
gradient = "sparkly"
animation = "off"
speed = "warp"
glyph = "right"
"#;
        let config: crate::config::Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ui.sidebar.host, SidebarHostConfig::default());
    }

    #[test]
    fn host_banner_enum_cycles_round_trip_through_toml_names() {
        // `next()` walks every variant exactly once before wrapping, and each
        // `as_str()` name deserializes back to the same variant, so the settings
        // UI cycle stays in lockstep with the config parser.
        fn assert_cycle<T: Copy + PartialEq + std::fmt::Debug>(
            start: T,
            next: impl Fn(T) -> T,
            as_str: impl Fn(T) -> &'static str,
            parse: impl Fn(&str) -> T,
            expected_len: usize,
        ) {
            let mut seen = Vec::new();
            let mut current = start;
            loop {
                assert_eq!(parse(as_str(current)), current);
                seen.push(current);
                current = next(current);
                if current == start {
                    break;
                }
            }
            assert_eq!(seen.len(), expected_len);
        }

        assert_cycle(
            HostBannerGradient::Rainbow,
            HostBannerGradient::next,
            HostBannerGradient::as_str,
            |name| parse_host_gradient(Some(name)),
            5,
        );
        assert_cycle(
            HostBannerAnimation::Animated,
            HostBannerAnimation::next,
            HostBannerAnimation::as_str,
            |name| parse_host_animation(Some(name)),
            2,
        );
        assert_cycle(
            HostBannerSpeed::Calm,
            HostBannerSpeed::next,
            HostBannerSpeed::as_str,
            |name| parse_host_speed(Some(name)),
            3,
        );
        assert_cycle(
            HostBannerGlyph::Left,
            HostBannerGlyph::next,
            HostBannerGlyph::as_str,
            |name| parse_host_glyph(Some(name)),
            2,
        );

        // Animation speed drift is strictly increasing with liveliness.
        assert!(HostBannerSpeed::Calm.drift() < HostBannerSpeed::Normal.drift());
        assert!(HostBannerSpeed::Normal.drift() < HostBannerSpeed::Lively.drift());
    }
}
