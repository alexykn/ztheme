mod async_theme;
mod manage;
mod zsh;

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::projects::Runtime;

pub use async_theme::AsyncTheme;
pub(crate) use manage::{apply, edit, list, persist};

const DEFAULT_THEME: &str = include_str!("../themes/default.toml");
const THEME_VERSION: u64 = 1;
const MAX_LAYOUT_LINES: usize = 8;
const MAX_LAYOUT_SEGMENTS: usize = 64;
const MAX_SEGMENT_SPACING: u8 = 16;
const STYLE_RESET: &str = "%f%k%b%u%s";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SegmentId {
    Directory,
    Git,
    Runtime(Runtime),
    Character,
    Status,
}

#[derive(Clone)]
pub struct CompiledTheme {
    theme: Theme,
    layout: ValidatedLayout,
    selector: String,
}

#[derive(Clone)]
struct ValidatedLayout {
    lines: Vec<Vec<SegmentId>>,
    right: Vec<SegmentId>,
    asynchronous: Vec<SegmentId>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u64,
    theme: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Theme {
    version: u64,
    palette: BTreeMap<String, String>,
    input: InputTheme,
    layout: Layout,
    segments: Segments,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputTheme {
    autosuggestion: Style,
    completion: Style,
    syntax: BTreeMap<String, SyntaxStyle>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum SyntaxStyle {
    Style(Style),
    Name(String),
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Layout {
    lines: Vec<Vec<String>>,
    right: Vec<String>,
    separator: String,
    blank_line_before: bool,
}

#[derive(Clone, Deserialize)]
struct Segments {
    directory: DirectoryTheme,
    git: GitTheme,
    character: CharacterTheme,
    status: StatusTheme,
    #[serde(flatten)]
    runtimes: BTreeMap<String, RuntimeTheme>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    home_symbol: String,
    truncation_symbol: String,
    style: Style,
    #[serde(default)]
    spacing: Spacing,
    width: DirectoryWidth,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryWidth {
    percent: u16,
    minimum: u16,
    maximum: u16,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    symbol: String,
    action_prefix: String,
    #[serde(default)]
    action_suffix: String,
    changes_prefix: String,
    style: Style,
    action_style: Style,
    #[serde(default)]
    spacing: Spacing,
    symbols: GitSymbols,
    styles: GitStyles,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitSymbols {
    conflicted: String,
    staged: String,
    modified: String,
    deleted: String,
    untracked: String,
    ahead: String,
    behind: String,
    diverged: String,
    stash: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitStyles {
    conflicted: Style,
    staged: Style,
    modified: Style,
    deleted: Style,
    untracked: Style,
    ahead: Style,
    behind: Style,
    diverged: Style,
    stash: Style,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    symbol: String,
    #[serde(default)]
    version_prefix: String,
    style: Style,
    #[serde(default)]
    spacing: Spacing,
    #[serde(default)]
    environment: EnvironmentTheme,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    style: Style,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharacterTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    success: String,
    error: String,
    success_style: Style,
    error_style: Style,
    #[serde(default)]
    spacing: Spacing,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusTheme {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    show_success: bool,
    success_symbol: String,
    style: Style,
    success_style: Style,
    #[serde(default)]
    spacing: Spacing,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Style {
    foreground: Option<String>,
    background: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    underline: bool,
    #[serde(default)]
    standout: bool,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Spacing {
    before: u8,
    after: u8,
}

impl SegmentId {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "directory" => Some(Self::Directory),
            "git" => Some(Self::Git),
            "character" => Some(Self::Character),
            "status" => Some(Self::Status),
            value => Runtime::from_name(value).map(Self::Runtime),
        }
    }

    pub const fn is_async(self) -> bool {
        matches!(self, Self::Git | Self::Runtime(_))
    }

    pub const fn is_right_safe(self) -> bool {
        matches!(self, Self::Directory | Self::Status)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Git => "git",
            Self::Runtime(runtime) => runtime.name(),
            Self::Character => "character",
            Self::Status => "status",
        }
    }
}

impl CompiledTheme {
    pub fn load(override_selector: Option<&str>) -> io::Result<Self> {
        let selector = match override_selector {
            Some(selector) => Some(selector.to_owned()),
            None => configured_theme()?,
        }
        .unwrap_or_else(|| "default".to_owned());
        let mut value = parse_versioned(DEFAULT_THEME, "embedded default theme")?;

        if selector != "default" {
            let path = theme_path(&selector)?;
            let source = fs::read_to_string(&path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot read theme {}: {error}", path.display()),
                )
            })?;
            let overlay = parse_versioned(&source, &format!("theme {}", path.display()))?;
            merge(&mut value, overlay);
        }

        let theme: Theme = value
            .try_into()
            .map_err(|error| invalid(format!("invalid theme: {error}")))?;
        if theme.version != THEME_VERSION {
            return Err(invalid(format!(
                "unsupported merged theme version {}",
                theme.version
            )));
        }
        let layout = validate(&theme)?;
        Ok(Self {
            theme,
            layout,
            selector,
        })
    }
}

fn configured_theme() -> io::Result<Option<String>> {
    let Some(root) = config_root() else {
        return Ok(None);
    };
    let path = root.join("ztheme/config.toml");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            ));
        }
    };
    let config: Config = toml::from_str(&source)
        .map_err(|error| invalid(format!("invalid {}: {error}", path.display())))?;
    if config.version != THEME_VERSION {
        return Err(invalid(format!(
            "unsupported config version {} in {}",
            config.version,
            path.display()
        )));
    }
    Ok(Some(config.theme))
}

fn config_root() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME") {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Some(root);
        }
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".config"))
}

fn theme_path(selector: &str) -> io::Result<PathBuf> {
    let path = Path::new(selector);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if selector.is_empty()
        || selector.len() > 128
        || !selector
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(
            "theme must be `default`, a simple name, or an absolute path",
        ));
    }
    let root = config_root().ok_or_else(|| invalid("cannot determine config directory"))?;
    Ok(root.join("ztheme/themes").join(format!("{selector}.toml")))
}

fn parse_versioned(source: &str, name: &str) -> io::Result<toml::Value> {
    let value: toml::Value =
        toml::from_str(source).map_err(|error| invalid(format!("invalid {name}: {error}")))?;
    let version = value
        .get("version")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| invalid(format!("{name} must contain integer `version = 1`")))?;
    if version != THEME_VERSION {
        return Err(invalid(format!(
            "{name} uses unsupported version {version}"
        )));
    }
    Ok(value)
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match overlay {
        toml::Value::Table(overlay_table) => {
            if let Some(base_table) = base.as_table_mut() {
                for (key, value) in overlay_table {
                    if let Some(base_value) = base_table.get_mut(&key) {
                        merge(base_value, value);
                    } else {
                        base_table.insert(key, value);
                    }
                }
                return;
            }
            *base = toml::Value::Table(overlay_table);
        }
        value => *base = value,
    }
}

fn validate(theme: &Theme) -> io::Result<ValidatedLayout> {
    for name in theme.segments.runtimes.keys() {
        if Runtime::from_name(name).is_none() {
            return Err(invalid(format!("unknown segment configuration `{name}`")));
        }
    }
    for runtime in Runtime::ALL {
        if !theme.segments.runtimes.contains_key(runtime.name()) {
            return Err(invalid(format!("missing segments.{}", runtime.name())));
        }
    }
    for (name, color) in &theme.palette {
        if !valid_identifier(name) {
            return Err(invalid(format!("invalid palette name `{name}`")));
        }
        if !valid_color(color) {
            return Err(invalid(format!(
                "palette `{name}` must be an RGB color such as #89b4fa"
            )));
        }
    }
    for required in ["accent", "muted", "success", "error"] {
        palette_color(theme, required)?;
    }

    validate_theme_literals(theme)?;
    validate_styles(theme)?;
    validate_spacing(theme)?;

    let width = &theme.segments.directory.width;
    if !(1..=100).contains(&width.percent) || width.minimum == 0 || width.minimum > width.maximum {
        return Err(invalid(
            "directory width requires percent 1-100 and 0 < minimum <= maximum",
        ));
    }

    if theme.layout.lines.is_empty() || theme.layout.lines.len() > MAX_LAYOUT_LINES {
        return Err(invalid("layout.lines must contain between 1 and 8 lines"));
    }

    let mut seen = HashSet::new();
    let mut lines = Vec::with_capacity(theme.layout.lines.len());
    let mut asynchronous = Vec::new();
    let mut total = 0;
    for line in &theme.layout.lines {
        if line.is_empty() {
            return Err(invalid("layout lines cannot be empty"));
        }
        let mut parsed = Vec::with_capacity(line.len());
        for name in line {
            let segment = parse_layout_segment(name, theme)?;
            if !seen.insert(segment) {
                return Err(invalid(format!("segment `{name}` appears more than once")));
            }
            if segment.is_async() {
                asynchronous.push(segment);
            }
            parsed.push(segment);
            total += 1;
        }
        lines.push(parsed);
    }

    let mut right = Vec::with_capacity(theme.layout.right.len());
    for name in &theme.layout.right {
        let segment = parse_layout_segment(name, theme)?;
        if !segment.is_right_safe() {
            return Err(invalid(format!(
                "segment `{name}` is not allowed in the right prompt"
            )));
        }
        if !seen.insert(segment) {
            return Err(invalid(format!("segment `{name}` appears more than once")));
        }
        right.push(segment);
        total += 1;
    }
    if total > MAX_LAYOUT_SEGMENTS {
        return Err(invalid("layout contains more than 64 segments"));
    }

    if let Some((line_index, item_index)) = find_segment(&lines, SegmentId::Character)
        && (line_index + 1 != lines.len() || item_index + 1 != lines[line_index].len())
    {
        return Err(invalid(
            "`character` must be the final segment of the final left line",
        ));
    }

    Ok(ValidatedLayout {
        lines,
        right,
        asynchronous,
    })
}

fn parse_layout_segment(name: &str, theme: &Theme) -> io::Result<SegmentId> {
    let segment =
        SegmentId::parse(name).ok_or_else(|| invalid(format!("unknown segment `{name}`")))?;
    if let SegmentId::Runtime(runtime) = segment
        && !theme.segments.runtimes.contains_key(runtime.name())
    {
        return Err(invalid(format!(
            "missing configuration for runtime segment `{name}`"
        )));
    }
    Ok(segment)
}

fn find_segment(lines: &[Vec<SegmentId>], needle: SegmentId) -> Option<(usize, usize)> {
    lines.iter().enumerate().find_map(|(line_index, line)| {
        line.iter()
            .position(|segment| *segment == needle)
            .map(|item_index| (line_index, item_index))
    })
}

fn validate_theme_literals(theme: &Theme) -> io::Result<()> {
    let directory = &theme.segments.directory;
    let git = &theme.segments.git;
    let character = &theme.segments.character;
    let status = &theme.segments.status;
    let mut values = vec![
        ("layout.separator", theme.layout.separator.as_str()),
        ("directory.prefix", directory.prefix.as_str()),
        ("directory.suffix", directory.suffix.as_str()),
        ("directory.home_symbol", directory.home_symbol.as_str()),
        (
            "directory.truncation_symbol",
            directory.truncation_symbol.as_str(),
        ),
        ("git.prefix", git.prefix.as_str()),
        ("git.suffix", git.suffix.as_str()),
        ("git.symbol", git.symbol.as_str()),
        ("git.action_prefix", git.action_prefix.as_str()),
        ("git.action_suffix", git.action_suffix.as_str()),
        ("git.changes_prefix", git.changes_prefix.as_str()),
        ("character.prefix", character.prefix.as_str()),
        ("character.suffix", character.suffix.as_str()),
        ("character.success", character.success.as_str()),
        ("character.error", character.error.as_str()),
        ("status.prefix", status.prefix.as_str()),
        ("status.suffix", status.suffix.as_str()),
        ("status.success_symbol", status.success_symbol.as_str()),
    ];
    for (name, value) in git_symbol_values(&git.symbols) {
        values.push((name, value));
    }
    for (name, runtime) in &theme.segments.runtimes {
        for (field, value) in [
            ("prefix", runtime.prefix.as_str()),
            ("suffix", runtime.suffix.as_str()),
            ("symbol", runtime.symbol.as_str()),
            ("version_prefix", runtime.version_prefix.as_str()),
            ("environment.prefix", runtime.environment.prefix.as_str()),
            ("environment.suffix", runtime.environment.suffix.as_str()),
        ] {
            if contains_control(value) {
                return Err(invalid(format!(
                    "segments.{name}.{field} contains a control character"
                )));
            }
        }
    }
    if let Some((name, _)) = values
        .into_iter()
        .find(|(_, value)| contains_control(value))
    {
        return Err(invalid(format!("{name} contains a control character")));
    }
    Ok(())
}

fn validate_styles(theme: &Theme) -> io::Result<()> {
    let segments = &theme.segments;
    for style in [
        &theme.input.autosuggestion,
        &theme.input.completion,
        &segments.directory.style,
        &segments.git.style,
        &segments.git.action_style,
        &segments.git.styles.conflicted,
        &segments.git.styles.staged,
        &segments.git.styles.modified,
        &segments.git.styles.deleted,
        &segments.git.styles.untracked,
        &segments.git.styles.ahead,
        &segments.git.styles.behind,
        &segments.git.styles.diverged,
        &segments.git.styles.stash,
        &segments.character.success_style,
        &segments.character.error_style,
        &segments.status.style,
        &segments.status.success_style,
    ] {
        style_open(style, theme)?;
    }
    for runtime in segments.runtimes.values() {
        style_open(&runtime.style, theme)?;
        style_open(&runtime.environment.style, theme)?;
    }
    for (name, style) in &theme.input.syntax {
        if !valid_syntax_style_name(name) {
            return Err(invalid(format!(
                "unknown zsh-syntax-highlighting style `{name}`"
            )));
        }
        highlight_style(style, theme)?;
    }
    Ok(())
}

fn validate_spacing(theme: &Theme) -> io::Result<()> {
    let segments = &theme.segments;
    for (name, spacing) in [
        ("directory", segments.directory.spacing),
        ("git", segments.git.spacing),
        ("character", segments.character.spacing),
        ("status", segments.status.spacing),
    ] {
        validate_segment_spacing(name, spacing)?;
    }
    for (name, runtime) in &segments.runtimes {
        validate_segment_spacing(name, runtime.spacing)?;
    }
    Ok(())
}

fn validate_segment_spacing(name: &str, spacing: Spacing) -> io::Result<()> {
    if spacing.before > MAX_SEGMENT_SPACING || spacing.after > MAX_SEGMENT_SPACING {
        return Err(invalid(format!(
            "segments.{name}.spacing values cannot exceed {MAX_SEGMENT_SPACING}"
        )));
    }
    Ok(())
}

fn git_symbol_values(symbols: &GitSymbols) -> [(&'static str, &str); 9] {
    [
        ("git.symbols.conflicted", &symbols.conflicted),
        ("git.symbols.staged", &symbols.staged),
        ("git.symbols.modified", &symbols.modified),
        ("git.symbols.deleted", &symbols.deleted),
        ("git.symbols.untracked", &symbols.untracked),
        ("git.symbols.ahead", &symbols.ahead),
        ("git.symbols.behind", &symbols.behind),
        ("git.symbols.diverged", &symbols.diverged),
        ("git.symbols.stash", &symbols.stash),
    ]
}

fn style_open(style: &Style, theme: &Theme) -> io::Result<String> {
    let mut output = String::new();
    if let Some(foreground) = style.foreground.as_deref() {
        write!(output, "%F{{{}}}", resolve_color(foreground, theme)?)
            .expect("writing to a String cannot fail");
    }
    if let Some(background) = style.background.as_deref() {
        write!(output, "%K{{{}}}", resolve_color(background, theme)?)
            .expect("writing to a String cannot fail");
    }
    if style.bold {
        output.push_str("%B");
    }
    if style.underline {
        output.push_str("%U");
    }
    if style.standout {
        output.push_str("%S");
    }
    Ok(output)
}

fn highlight_style(style: &SyntaxStyle, theme: &Theme) -> io::Result<String> {
    match style {
        SyntaxStyle::Name(name) if name == "none" => Ok(name.clone()),
        SyntaxStyle::Name(name) => Err(invalid(format!(
            "syntax style name must be `none`, not `{name}`"
        ))),
        SyntaxStyle::Style(style) => line_editor_style(style, theme),
    }
}

fn line_editor_style(style: &Style, theme: &Theme) -> io::Result<String> {
    let mut output = Vec::with_capacity(5);
    if let Some(foreground) = style.foreground.as_deref() {
        output.push(format!("fg={}", resolve_color(foreground, theme)?));
    }
    if let Some(background) = style.background.as_deref() {
        output.push(format!("bg={}", resolve_color(background, theme)?));
    }
    if style.bold {
        output.push("bold".to_owned());
    }
    if style.underline {
        output.push("underline".to_owned());
    }
    if style.standout {
        output.push("standout".to_owned());
    }
    if output.is_empty() {
        return Ok("none".to_owned());
    }
    Ok(output.join(","))
}

fn valid_syntax_style_name(name: &str) -> bool {
    const FIXED: &[&str] = &[
        "alias",
        "arg0",
        "arithmetic-expansion",
        "autodirectory",
        "back-dollar-quoted-argument",
        "back-double-quoted-argument",
        "back-quoted-argument",
        "back-quoted-argument-delimiter",
        "back-quoted-argument-unclosed",
        "bracket-error",
        "builtin",
        "command",
        "command-substitution",
        "command-substitution-delimiter",
        "command-substitution-delimiter-quoted",
        "command-substitution-delimiter-unquoted",
        "command-substitution-quoted",
        "command-substitution-unquoted",
        "commandseparator",
        "comment",
        "cursor",
        "cursor-matchingbracket",
        "default",
        "dollar-double-quoted-argument",
        "dollar-quoted-argument",
        "dollar-quoted-argument-unclosed",
        "double-hyphen-option",
        "double-quoted-argument",
        "double-quoted-argument-unclosed",
        "function",
        "global-alias",
        "globbing",
        "hashed-command",
        "history-expansion",
        "line",
        "named-fd",
        "numeric-fd",
        "path",
        "path_pathseparator",
        "path_prefix",
        "path_prefix_pathseparator",
        "precommand",
        "process-substitution",
        "process-substitution-delimiter",
        "rc-quote",
        "redirection",
        "reserved-word",
        "root",
        "single-hyphen-option",
        "single-quoted-argument",
        "single-quoted-argument-unclosed",
        "suffix-alias",
        "unknown-token",
    ];

    FIXED.binary_search(&name).is_ok()
        || name.strip_prefix("arg0_").is_some_and(valid_identifier)
        || name.strip_prefix("bracket-level-").is_some_and(|level| {
            !level.is_empty() && level.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn resolve_color<'a>(value: &'a str, theme: &'a Theme) -> io::Result<&'a str> {
    if valid_color(value) {
        return Ok(value);
    }
    palette_color(theme, value)
}

fn palette_color<'a>(theme: &'a Theme, name: &str) -> io::Result<&'a str> {
    theme
        .palette
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| invalid(format!("unknown palette color `{name}`")))
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn contains_control(value: &str) -> bool {
    value.chars().any(char::is_control)
}

fn prompt_literal(value: &str) -> String {
    value.replace('%', "%%")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::{CompiledTheme, DEFAULT_THEME, Theme, merge, parse_versioned, validate};

    fn merged_theme(overlay: &str) -> std::io::Result<Theme> {
        let mut value = parse_versioned(DEFAULT_THEME, "default")?;
        merge(&mut value, parse_versioned(overlay, "overlay")?);
        value
            .try_into()
            .map_err(|error| super::invalid(format!("invalid theme: {error}")))
    }

    fn validate_overlay(overlay: &str) -> std::io::Result<()> {
        validate(&merged_theme(overlay)?).map(|_| ())
    }

    #[test]
    fn embedded_default_theme_compiles_to_zsh() {
        let theme = merged_theme("version = 1").unwrap();
        let layout = validate(&theme).unwrap();
        let compiled = CompiledTheme {
            theme,
            layout,
            selector: "default".to_owned(),
        };
        let zsh = compiled.zsh().unwrap();

        assert!(zsh.contains("__ZTHEME_THEME_SELECTOR='default'"));
        assert!(zsh.contains("_ztheme_render_layout()"));
        assert!(zsh.contains("ZSH_HIGHLIGHT_STYLES[command]"));
        assert!(!zsh.contains("@ZTHEME_"));
    }

    #[test]
    fn overlays_replace_only_explicit_values() {
        let theme = merged_theme(
            r##"
version = 1

[palette]
path = "#112233"

[layout]
lines = [["directory"], ["character"]]
"##,
        )
        .unwrap();

        assert_eq!(theme.palette["path"], "#112233");
        assert_eq!(theme.palette["accent"], "#f9e2af");
        assert_eq!(theme.layout.lines.len(), 2);
        assert_eq!(theme.layout.lines[0], ["directory"]);
        assert_eq!(theme.segments.git.symbol, " ");
        validate(&theme).unwrap();
    }

    #[test]
    fn invalid_theme_contracts_are_rejected() {
        let cases = [
            ("unknown field", "version = 1\nsurprise = true"),
            (
                "duplicate segment",
                "version = 1\n[layout]\nlines = [[\"directory\", \"directory\"]]",
            ),
            (
                "async right segment",
                "version = 1\n[layout]\nright = [\"git\"]",
            ),
            (
                "misplaced character",
                "version = 1\n[layout]\nlines = [[\"character\", \"directory\"]]",
            ),
            (
                "invalid palette color",
                "version = 1\n[palette]\naccent = \"orange\"",
            ),
            (
                "control character",
                "version = 1\n[segments.git]\nsymbol = \"bad\\n\"",
            ),
            (
                "invalid width",
                "version = 1\n[segments.directory.width]\npercent = 0",
            ),
            (
                "invalid spacing",
                "version = 1\n[segments.character.spacing]\nafter = 17",
            ),
            (
                "unknown syntax style",
                "version = 1\n[input.syntax]\nnot-a-real-style = \"none\"",
            ),
        ];

        for (name, overlay) in cases {
            assert!(
                validate_overlay(overlay).is_err(),
                "accepted invalid case: {name}"
            );
        }
    }

    #[test]
    fn unsupported_or_missing_versions_are_rejected_before_merge() {
        assert!(parse_versioned("theme = \"x\"", "missing").is_err());
        assert!(parse_versioned("version = 2", "future").is_err());
        assert!(parse_versioned("version = \"1\"", "wrong type").is_err());
    }
}
