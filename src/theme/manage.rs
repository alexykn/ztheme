use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use crate::runtime::Runtime;

use super::{
    BUNDLED_THEMES, CompiledTheme, DEFAULT_THEME_SELECTOR, LayoutSegment, SegmentId, Style, Theme,
    bundled_theme, config_root, configured_theme, default_config, invalid, load_config,
    resolve_color, theme_path, valid_identifier,
};

const PALETTE_ROLES: [&str; 7] = [
    "path", "git", "accent", "success", "warning", "error", "muted",
];

pub fn list() -> io::Result<String> {
    let root = config_root().ok_or_else(|| invalid("cannot determine config directory"))?;
    let directory = root.join("ztheme/themes");
    let (active, config_error) = match configured_theme() {
        Ok(selector) => (
            Some(selector.unwrap_or_else(|| DEFAULT_THEME_SELECTOR.to_owned())),
            None,
        ),
        Err(error) => (None, Some(error)),
    };
    let (mut selectors, ignored) = discover(&directory)?;
    if let Some(active) = active.as_ref()
        && bundled_theme(active).is_none()
        && !selectors.contains(active)
    {
        selectors.insert(active.clone());
    }

    let mut selectors = selectors.into_iter().collect::<Vec<_>>();
    selectors.sort_by(|left, right| {
        let left_active = active.as_ref() == Some(left);
        let right_active = active.as_ref() == Some(right);
        right_active.cmp(&left_active).then_with(|| left.cmp(right))
    });
    for theme in BUNDLED_THEMES.iter().rev() {
        selectors.insert(0, theme.selector.to_owned());
    }

    let color = use_color();
    let mut output = format!("Themes  {}\n", directory.display());
    if let Some(error) = config_error {
        writeln!(
            output,
            "Active  ! invalid config: {}\n",
            error_text(&error.to_string())
        )
        .expect("writing to a String cannot fail");
    } else {
        let active = active
            .as_deref()
            .expect("valid configuration has an active selector");
        writeln!(output, "Active  {}\n", safe_text(active))
            .expect("writing to a String cannot fail");
    }

    for selector in selectors {
        let active_theme = active.as_deref() == Some(&selector);
        match CompiledTheme::load(Some(&selector)) {
            Ok(compiled) => render_entry(&mut output, &selector, active_theme, &compiled, color)?,
            Err(error) => {
                writeln!(
                    output,
                    "! {}  invalid\n  {}\n",
                    safe_text(&selector),
                    error_text(&error.to_string())
                )
                .expect("writing to a String cannot fail");
            }
        }
    }
    for name in ignored {
        writeln!(
            output,
            "! {}  ignored: filename is not a valid theme selector\n",
            safe_text(&name)
        )
        .expect("writing to a String cannot fail");
    }
    Ok(output)
}

pub fn edit(selector: &str) -> io::Result<String> {
    if bundled_theme(selector).is_some() {
        return Err(invalid(format!(
            "bundled theme `{selector}` cannot be edited"
        )));
    }
    let path = theme_path(selector)?;
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("theme {} does not exist", path.display()),
        ));
    }

    let command = editor_command()?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .arg(&path)
        .status()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot start editor `{}`: {error}", command[0]),
            )
        })?;
    if !status.success() {
        return Err(io::Error::other(match status.code() {
            Some(code) => format!("editor exited with status {code}"),
            None => "editor terminated without an exit status".to_owned(),
        }));
    }

    CompiledTheme::load(Some(selector))?;
    Ok(format!(
        "Theme '{}' is valid.\nReload it with `ztheme theme reload` if active, or apply it with `ztheme theme apply {}`.\n",
        safe_text(selector),
        safe_text(selector)
    ))
}

pub fn apply(selector: &str) -> io::Result<PathBuf> {
    CompiledTheme::load(Some(selector))?;
    persist(selector)
}

pub(crate) fn persist(selector: &str) -> io::Result<PathBuf> {
    let root = config_root().ok_or_else(|| invalid("cannot determine config directory"))?;
    let path = root.join("ztheme/config.toml");
    let parent = path
        .parent()
        .ok_or_else(|| invalid("theme config has no parent directory"))?;
    fs::create_dir_all(parent)?;

    let target = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(&path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot resolve config symlink {}: {error}", path.display()),
                )
            })?
        }
        Ok(_) => path.clone(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.clone(),
        Err(error) => return Err(error),
    };
    // Update only the theme selector so unrelated configuration such as
    // [custom_segments] is preserved across `theme apply`.
    let mut config = load_config()?.unwrap_or_else(default_config);
    selector.clone_into(&mut config.theme);
    let content = toml::to_string(&config).map_err(io::Error::other)?;
    atomic_write(&target, content.as_bytes())?;
    Ok(path)
}

fn discover(directory: &Path) -> io::Result<(BTreeSet<String>, Vec<String>)> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((BTreeSet::new(), Vec::new()));
        }
        Err(error) => return Err(error),
    };
    let mut selectors = BTreeSet::new();
    let mut ignored = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
            ignored.push(entry.file_name().to_string_lossy().into_owned());
            continue;
        };
        if bundled_theme(name).is_some() {
            continue;
        }
        if !valid_identifier(name) {
            ignored.push(entry.file_name().to_string_lossy().into_owned());
            continue;
        }
        selectors.insert(name.to_owned());
    }
    ignored.sort();
    Ok((selectors, ignored))
}

fn editor_command() -> io::Result<Vec<String>> {
    for variable in ["VISUAL", "EDITOR"] {
        let Some(value) = env::var_os(variable) else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| invalid(format!("{variable} contains non-UTF-8 data")))?;
        if value.trim().is_empty() {
            continue;
        }
        let command = shell_words::split(&value)
            .map_err(|error| invalid(format!("invalid {variable}: {error}")))?;
        if !command.is_empty() {
            return Ok(command);
        }
    }
    Err(invalid("neither VISUAL nor EDITOR is set"))
}

fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("config target has no parent directory"))?;
    let stem = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("ztheme-config");
    for attempt in 0..16 {
        let temporary = parent.join(format!(".{stem}.{}.{}.tmp", process::id(), attempt));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(content).and_then(|()| file.sync_all()) {
            drop(file);
            return cleanup_error(&temporary, error);
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, path) {
            return cleanup_error(&temporary, error);
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a temporary config file",
    ))
}

fn cleanup_error(path: &Path, original: io::Error) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Err(original),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(original),
        Err(cleanup) => Err(io::Error::new(
            original.kind(),
            format!("{original}; cannot remove {}: {cleanup}", path.display()),
        )),
    }
}

fn render_entry(
    output: &mut String,
    selector: &str,
    active: bool,
    compiled: &CompiledTheme,
    color: bool,
) -> io::Result<()> {
    let marker = if active { '●' } else { '○' };
    let qualifier = if bundled_theme(selector).is_some() {
        " - builtin"
    } else {
        ""
    };
    if selector == DEFAULT_THEME_SELECTOR {
        writeln!(
            output,
            "{marker} {} (default){qualifier}",
            safe_text(selector)
        )
        .expect("writing to a String cannot fail");
    } else {
        writeln!(output, "{marker} {}{qualifier}", safe_text(selector))
            .expect("writing to a String cannot fail");
    }
    render_palette(output, &compiled.theme, color)?;
    render_layout(output, &compiled.layout);
    render_symbols(output, &compiled.theme);
    render_example(output, compiled, color)?;
    output.push('\n');
    Ok(())
}

fn render_palette(output: &mut String, theme: &Theme, color: bool) -> io::Result<()> {
    for (index, roles) in PALETTE_ROLES.chunks(3).enumerate() {
        if index == 0 {
            output.push_str("  palette   ");
        } else {
            output.push_str("            ");
        }
        for role in roles {
            let value = theme
                .palette
                .get(*role)
                .ok_or_else(|| invalid(format!("missing palette color `{role}`")))?;
            write!(
                output,
                "{} {:<7} {}  ",
                swatch(value, color)?,
                role,
                value.to_ascii_uppercase()
            )
            .expect("writing to a String cannot fail");
        }
        output.push('\n');
    }
    Ok(())
}

fn render_layout(output: &mut String, layout: &super::ValidatedLayout) {
    for (index, line) in layout.lines.iter().enumerate() {
        if index == 0 {
            output.push_str("  layout    ");
        } else {
            output.push_str("            ");
        }
        output.push_str(&summarize_segments(line));
        if index + 1 == layout.lines.len() && !layout.right.is_empty() {
            output.push_str("    right: ");
            output.push_str(&summarize_segments(&layout.right));
        }
        output.push('\n');
    }
}

fn summarize_segments(segments: &[LayoutSegment]) -> String {
    let mut parts = Vec::new();
    let mut index = 0;
    while index < segments.len() {
        if matches!(
            segments[index],
            LayoutSegment::BuiltIn(SegmentId::Runtime(_))
        ) {
            let start = index;
            while index < segments.len()
                && matches!(
                    segments[index],
                    LayoutSegment::BuiltIn(SegmentId::Runtime(_))
                )
            {
                index += 1;
            }
            parts.push(format!("runtimes({})", index - start));
            continue;
        }
        parts.push(segments[index].name().to_owned());
        index += 1;
    }
    parts.join(" · ")
}

fn render_symbols(output: &mut String, theme: &Theme) {
    let git = &theme.segments.git;
    let symbols = &git.symbols;
    writeln!(
        output,
        "  symbols   branch {}  conflict {}  staged {}  modified {}  deleted {}",
        git.symbol, symbols.conflicted, symbols.staged, symbols.modified, symbols.deleted
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "            untracked {}  ahead {}  behind {}  diverged {}  stash {}",
        symbols.untracked, symbols.ahead, symbols.behind, symbols.diverged, symbols.stash
    )
    .expect("writing to a String cannot fail");
}

fn render_example(output: &mut String, compiled: &CompiledTheme, color: bool) -> io::Result<()> {
    let mut lines = Vec::with_capacity(compiled.layout.lines.len());
    for line in &compiled.layout.lines {
        lines.push(render_example_segments(line, &compiled.theme, color)?);
    }
    let right = render_example_segments(&compiled.layout.right, &compiled.theme, color)?;
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            output.push_str("  example   ");
        } else {
            output.push_str("            ");
        }
        output.push_str(line);
        if index + 1 == lines.len() && !right.is_empty() {
            let occupied = display_width(line) + display_width(&right);
            let padding = 72_usize.saturating_sub(occupied).max(2);
            output.push_str(&" ".repeat(padding));
            output.push_str(&right);
        }
        output.push('\n');
    }
    Ok(())
}

fn render_example_segments(
    segments: &[LayoutSegment],
    theme: &Theme,
    color: bool,
) -> io::Result<String> {
    let mut fragments = Vec::new();
    for segment in segments {
        let fragment = match segment {
            LayoutSegment::BuiltIn(SegmentId::Directory) => sample_directory(theme, color)?,
            LayoutSegment::BuiltIn(SegmentId::Git) => sample_git(theme, color)?,
            LayoutSegment::BuiltIn(SegmentId::Runtime(Runtime::Python)) => {
                sample_python(theme, color)?
            }
            LayoutSegment::BuiltIn(SegmentId::Runtime(_)) | LayoutSegment::Custom(_) => {
                String::new()
            }
            LayoutSegment::BuiltIn(SegmentId::Character) => sample_character(theme, color)?,
            LayoutSegment::BuiltIn(SegmentId::Status) => sample_status(theme, color)?,
        };
        if !fragment.is_empty() {
            fragments.push(fragment);
        }
    }
    Ok(fragments.join(&theme.layout.separator))
}

fn sample_directory(theme: &Theme, color: bool) -> io::Result<String> {
    let segment = &theme.segments.directory;
    let content = styled(
        &segment.style,
        &format!("{}~/github/ztheme{}", segment.prefix, segment.suffix),
        theme,
        color,
    )?;
    Ok(with_spacing(segment.spacing, &content))
}

fn sample_git(theme: &Theme, color: bool) -> io::Result<String> {
    let segment = &theme.segments.git;
    let mut output = styled(
        &segment.style,
        &format!("{}{}main", segment.prefix, segment.symbol),
        theme,
        color,
    )?;
    output.push_str(&segment.changes_prefix);
    output.push_str(&styled(
        &segment.styles.modified,
        &segment.symbols.modified,
        theme,
        color,
    )?);
    output.push_str(&styled(
        &segment.styles.untracked,
        &segment.symbols.untracked,
        theme,
        color,
    )?);
    output.push_str(&styled(&segment.style, &segment.suffix, theme, color)?);
    Ok(with_spacing(segment.spacing, &output))
}

fn sample_python(theme: &Theme, color: bool) -> io::Result<String> {
    let segment = theme
        .segments
        .runtimes
        .get("python")
        .ok_or_else(|| invalid("missing segments.python"))?;
    let mut output = styled(
        &segment.style,
        &format!(
            "{}{}{}3.12.13",
            segment.prefix, segment.symbol, segment.version_prefix
        ),
        theme,
        color,
    )?;
    output.push_str(&styled(
        &segment.environment.style,
        &format!(
            "{}venv{}",
            segment.environment.prefix, segment.environment.suffix
        ),
        theme,
        color,
    )?);
    output.push_str(&styled(&segment.style, &segment.suffix, theme, color)?);
    Ok(with_spacing(segment.spacing, &output))
}

fn sample_character(theme: &Theme, color: bool) -> io::Result<String> {
    let segment = &theme.segments.character;
    let content = styled(
        &segment.error_style,
        &format!("{}{}{}", segment.prefix, segment.error, segment.suffix),
        theme,
        color,
    )?;
    Ok(with_spacing(segment.spacing, &content))
}

fn sample_status(theme: &Theme, color: bool) -> io::Result<String> {
    let segment = &theme.segments.status;
    let content = styled(
        &segment.style,
        &format!("{}1{}", segment.prefix, segment.suffix),
        theme,
        color,
    )?;
    Ok(with_spacing(segment.spacing, &content))
}

fn with_spacing(spacing: super::Spacing, value: &str) -> String {
    format!(
        "{}{value}{}",
        " ".repeat(usize::from(spacing.before)),
        " ".repeat(usize::from(spacing.after))
    )
}

fn styled(style: &Style, value: &str, theme: &Theme, color: bool) -> io::Result<String> {
    if !color || value.is_empty() {
        return Ok(value.to_owned());
    }
    let mut codes = Vec::new();
    if let Some(foreground) = style.foreground.as_deref() {
        let (red, green, blue) = rgb(resolve_color(foreground, theme)?)?;
        codes.push(format!("38;2;{red};{green};{blue}"));
    }
    if let Some(background) = style.background.as_deref() {
        let (red, green, blue) = rgb(resolve_color(background, theme)?)?;
        codes.push(format!("48;2;{red};{green};{blue}"));
    }
    if style.bold {
        codes.push("1".to_owned());
    }
    if style.underline {
        codes.push("4".to_owned());
    }
    if style.standout {
        codes.push("7".to_owned());
    }
    if codes.is_empty() {
        return Ok(value.to_owned());
    }
    Ok(format!("\u{1b}[{}m{value}\u{1b}[0m", codes.join(";")))
}

fn swatch(value: &str, color: bool) -> io::Result<String> {
    if !color {
        return Ok("■".to_owned());
    }
    let (red, green, blue) = rgb(value)?;
    Ok(format!("\u{1b}[38;2;{red};{green};{blue}m■\u{1b}[0m"))
}

fn rgb(value: &str) -> io::Result<(u8, u8, u8)> {
    if value.len() != 7 || !value.starts_with('#') {
        return Err(invalid(format!("invalid RGB color `{value}`")));
    }
    let red = u8::from_str_radix(&value[1..3], 16).map_err(io::Error::other)?;
    let green = u8::from_str_radix(&value[3..5], 16).map_err(io::Error::other)?;
    let blue = u8::from_str_radix(&value[5..7], 16).map_err(io::Error::other)?;
    Ok((red, green, blue))
}

fn use_color() -> bool {
    io::stdout().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && env::var_os("TERM").is_none_or(|term| term != "dumb")
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn error_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn display_width(value: &str) -> usize {
    let mut escape = false;
    let mut width = 0;
    for character in value.chars() {
        if escape {
            if character == 'm' {
                escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            escape = true;
        } else {
            width += 1;
        }
    }
    width
}
