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

    /// `[async]` prompt-rendering behavior: how long the prompt waits for
    /// each asynchronous segment group before drawing (see `AsyncSection`).
    #[serde(default)]
    #[serde(rename = "async")]
    #[serde(skip_serializing_if = "AsyncSection::is_default")]
    pub(super) async_section: AsyncSection,

    /// `[git]` gitstatusd tuning. Validated here at init and reload so a typo
    /// is reported, and preserved by `theme apply`; the daemon reads it.
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::gitstatus::Options::is_default")]
    pub(super) git: crate::gitstatus::Options,
}

/// The `[async]` table exactly as written. Every key is optional so that
/// `theme apply` writes back only what the user set; `modes` resolves the
/// defaults and the legacy `[async.lock]` booleans.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AsyncSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) git: Option<AsyncMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runtime: Option<AsyncMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) budget_ms: Option<u64>,
    /// Pre-mode lock booleans: `true` maps to `locked`, `false` to `async`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) lock: Option<AsyncLock>,
}

impl AsyncSection {
    pub(super) fn is_default(&self) -> bool {
        self.git.is_none()
            && self.runtime.is_none()
            && self.budget_ms.is_none()
            && self.lock.is_none()
    }

    /// Resolves the effective per-group modes, rejecting a group configured
    /// both through its mode key and through the legacy lock boolean.
    pub(super) fn modes(&self) -> Result<AsyncModes, String> {
        let lock = self.lock.clone().unwrap_or_default();
        let budget_ms = self.budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
        if budget_ms > MAX_BUDGET_MS {
            return Err(format!(
                "async.budget_ms must be at most {MAX_BUDGET_MS}, got {budget_ms}"
            ));
        }
        Ok(AsyncModes {
            git: group_mode("git", self.git, lock.git_segment, "git_segment")?,
            runtime: group_mode(
                "runtime",
                self.runtime,
                lock.runtime_segment,
                "runtime_segment",
            )?,
            budget_ms,
        })
    }
}

fn group_mode(
    key: &str,
    mode: Option<AsyncMode>,
    locked: Option<bool>,
    lock_key: &str,
) -> Result<AsyncMode, String> {
    match (mode, locked) {
        (Some(_), Some(_)) => Err(format!(
            "async.{key} and async.lock.{lock_key} both configure the {key} group; keep only async.{key}"
        )),
        (Some(mode), None) => Ok(mode),
        (None, Some(true)) => Ok(AsyncMode::Locked),
        (None, Some(false)) => Ok(AsyncMode::Async),
        (None, None) => Ok(AsyncMode::Auto),
    }
}

/// How long the shell waits for an asynchronous group before drawing:
/// `locked` until the group completes (bounded by the request deadline),
/// `auto` up to `budget_ms`, `async` not at all. Whatever arrives later is
/// drawn by a redraw; until then the last known value stays visible.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AsyncMode {
    Locked,
    Async,
    Auto,
}

impl AsyncMode {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Async => "async",
            Self::Auto => "auto",
        }
    }
}

/// The resolved `[async]` behavior baked into the shell integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AsyncModes {
    pub(crate) git: AsyncMode,
    pub(crate) runtime: AsyncMode,
    pub(crate) budget_ms: u64,
}

impl Default for AsyncModes {
    fn default() -> Self {
        AsyncSection::default()
            .modes()
            .expect("the default async section is valid")
    }
}

/// A warm Git query in a small repository and a cache-hit runtime both answer
/// in well under 10 ms; 100 ms covers them with margin under load while
/// staying below the delay a user notices before the prompt.
const DEFAULT_BUDGET_MS: u64 = 100;
/// The shell stops waiting at the client's 550 ms `done` anyway; a larger
/// budget would only lengthen the wait on a client that stopped answering.
const MAX_BUDGET_MS: u64 = 1000;

/// Legacy `[async.lock]` table, kept for existing configurations.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct AsyncLock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) git_segment: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) runtime_segment: Option<bool>,
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
