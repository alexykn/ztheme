use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub(super) version: u64,
    pub(super) theme: String,

    #[serde(default)]
    #[serde(skip_serializing_if = "CustomSegmentsConfig::is_empty")]
    pub(super) custom_segments: CustomSegmentsConfig,

    /// `[async]` prompt-rendering behavior. Currently holds only the per-group
    /// lock settings that decide whether the prompt waits for an asynchronous
    /// segment group or renders immediately and redraws when it is ready.
    #[serde(default)]
    #[serde(rename = "async")]
    #[serde(skip_serializing_if = "AsyncSection::is_default")]
    pub(super) async_section: AsyncSection,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AsyncSection {
    pub(super) lock: AsyncLock,
}

impl AsyncSection {
    pub(super) fn is_default(&self) -> bool {
        self.lock.git_segment && !self.lock.runtime_segment
    }
}

/// Per-asynchronous-group prompt lock. A locked group holds the prompt blank
/// until it completes (or the shared deadline expires); an unlocked group lets
/// the prompt render immediately and redraw when its value arrives. The Git
/// group is locked by default; the runtime group is unlocked by default,
/// because runtime version commands (such as Swift) are often the slow part
/// while `gitstatusd` is fast.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AsyncLock {
    #[serde(default = "default_true")]
    pub(crate) git_segment: bool,
    #[serde(default = "default_false")]
    pub(crate) runtime_segment: bool,
}

impl Default for AsyncLock {
    fn default() -> Self {
        Self {
            git_segment: true,
            runtime_segment: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CustomSegmentsConfig {
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) enabled: Vec<String>,
}

impl CustomSegmentsConfig {
    fn is_empty(&self) -> bool {
        self.enabled.is_empty()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Theme {
    pub(super) version: u64,
    pub(super) palette: BTreeMap<String, String>,
    pub(super) input: InputTheme,
    pub(super) layout: Layout,
    pub(super) segments: Segments,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InputTheme {
    pub(super) autosuggestion: Style,
    pub(super) completion: Style,
    pub(super) syntax: BTreeMap<String, SyntaxStyle>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum SyntaxStyle {
    Style(Style),
    Name(String),
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Layout {
    pub(super) lines: Vec<Vec<String>>,
    pub(super) right: Vec<String>,
    pub(super) separator: String,
    pub(super) blank_line_before: bool,
}

#[derive(Clone, Deserialize)]
pub(super) struct Segments {
    pub(super) directory: DirectoryTheme,
    pub(super) clock: CustomSegmentTheme,
    pub(super) git: GitTheme,
    pub(super) character: CharacterTheme,
    pub(super) status: StatusTheme,

    #[serde(default)]
    pub(super) custom: BTreeMap<String, CustomSegmentTheme>,

    #[serde(flatten)]
    pub(super) runtimes: BTreeMap<String, RuntimeTheme>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CustomSegmentTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) home_symbol: String,
    pub(super) truncation_symbol: String,
    pub(super) style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
    pub(super) width: DirectoryWidth,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryWidth {
    pub(super) percent: u16,
    pub(super) minimum: u16,
    pub(super) maximum: u16,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) symbol: String,
    pub(super) action_prefix: String,
    #[serde(default)]
    pub(super) action_suffix: String,
    pub(super) changes_prefix: String,
    pub(super) style: Style,
    pub(super) action_style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
    pub(super) symbols: GitSymbols,
    pub(super) styles: GitStyles,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitSymbols {
    pub(super) conflicted: String,
    pub(super) staged: String,
    pub(super) modified: String,
    pub(super) deleted: String,
    pub(super) untracked: String,
    pub(super) ahead: String,
    pub(super) behind: String,
    pub(super) diverged: String,
    pub(super) stash: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitStyles {
    pub(super) conflicted: Style,
    pub(super) staged: Style,
    pub(super) modified: Style,
    pub(super) deleted: Style,
    pub(super) untracked: Style,
    pub(super) ahead: Style,
    pub(super) behind: Style,
    pub(super) diverged: Style,
    pub(super) stash: Style,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuntimeTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) version_prefix: String,
    pub(super) style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
    #[serde(default)]
    pub(super) environment: EnvironmentTheme,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnvironmentTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    #[serde(default)]
    pub(super) style: Style,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CharacterTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) success: String,
    pub(super) error: String,
    pub(super) success_style: Style,
    pub(super) error_style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatusTheme {
    #[serde(default)]
    pub(super) prefix: String,
    #[serde(default)]
    pub(super) suffix: String,
    pub(super) show_success: bool,
    pub(super) success_symbol: String,
    pub(super) style: Style,
    pub(super) success_style: Style,
    #[serde(default)]
    pub(super) spacing: Spacing,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Style {
    pub(super) foreground: Option<String>,
    pub(super) background: Option<String>,
    #[serde(default)]
    pub(super) bold: bool,
    #[serde(default)]
    pub(super) underline: bool,
    #[serde(default)]
    pub(super) standout: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Spacing {
    pub(super) before: u8,
    pub(super) after: u8,
}
