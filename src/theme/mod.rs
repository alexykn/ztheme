mod async_theme;
mod manage;
mod schema;
mod validate;
mod zsh;

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::runtime::Runtime;

pub use async_theme::AsyncTheme;
pub(crate) use manage::{apply, edit, list, persist};
use schema::{
    Config, DirectoryTheme, GitSymbols, InputTheme, Layout, RuntimeTheme, Segments, Spacing, Style,
    SyntaxStyle, Theme,
};
use validate::validate;

const DEFAULT_THEME_SELECTOR: &str = "catppuccin-mocha";
const CATPPUCCIN_MOCHA_THEME: &str = include_str!("../../themes/catppuccin-mocha.toml");
const VESPER_THEME: &str = include_str!("../../themes/vesper.toml");
const BUNDLED_THEMES: &[BundledTheme] = &[
    BundledTheme {
        selector: DEFAULT_THEME_SELECTOR,
        overlay: None,
    },
    BundledTheme {
        selector: "vesper",
        overlay: Some(VESPER_THEME),
    },
];
const THEME_VERSION: u64 = 1;
const MAX_LAYOUT_LINES: usize = 8;
const MAX_LAYOUT_SEGMENTS: usize = 64;
const MAX_SEGMENT_SPACING: u8 = 16;
const STYLE_RESET: &str = "%f%k%b%u%s";

struct BundledTheme {
    selector: &'static str,
    overlay: Option<&'static str>,
}

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
        .unwrap_or_else(|| DEFAULT_THEME_SELECTOR.to_owned());
        let mut value = parse_versioned(CATPPUCCIN_MOCHA_THEME, "bundled Catppuccin Mocha theme")?;

        if let Some(bundled) = bundled_theme(&selector) {
            if let Some(source) = bundled.overlay {
                let overlay =
                    parse_versioned(source, &format!("bundled {} theme", bundled.selector))?;
                merge(&mut value, overlay);
            }
        } else {
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

fn bundled_theme(selector: &str) -> Option<&'static BundledTheme> {
    BUNDLED_THEMES
        .iter()
        .find(|theme| theme.selector == selector)
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
            "theme must be a bundled theme, a simple name, or an absolute path",
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
    use super::{
        CATPPUCCIN_MOCHA_THEME, CompiledTheme, DEFAULT_THEME_SELECTOR, Theme, merge,
        parse_versioned, validate,
    };

    fn merged_theme(overlay: &str) -> std::io::Result<Theme> {
        let mut value = parse_versioned(CATPPUCCIN_MOCHA_THEME, "Catppuccin Mocha")?;
        merge(&mut value, parse_versioned(overlay, "overlay")?);
        value
            .try_into()
            .map_err(|error| super::invalid(format!("invalid theme: {error}")))
    }

    fn validate_overlay(overlay: &str) -> std::io::Result<()> {
        validate(&merged_theme(overlay)?).map(|_| ())
    }

    #[test]
    fn bundled_catppuccin_mocha_theme_compiles_to_zsh() {
        let theme = merged_theme("version = 1").unwrap();
        let layout = validate(&theme).unwrap();
        let compiled = CompiledTheme {
            theme,
            layout,
            selector: DEFAULT_THEME_SELECTOR.to_owned(),
        };
        let zsh = compiled.zsh().unwrap();

        assert!(zsh.contains("__ZTHEME_THEME_SELECTOR='catppuccin-mocha'"));
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
