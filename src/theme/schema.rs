use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub(super) version: u64,
    pub(super) theme: String,
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
    pub(super) git: GitTheme,
    pub(super) character: CharacterTheme,
    pub(super) status: StatusTheme,
    #[serde(flatten)]
    pub(super) runtimes: BTreeMap<String, RuntimeTheme>,
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
