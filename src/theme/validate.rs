use std::collections::HashSet;
use std::io;

use crate::runtime::Runtime;

use super::{
    LayoutSegment, MAX_LAYOUT_LINES, MAX_LAYOUT_SEGMENTS, MAX_SEGMENT_SPACING, SegmentId, Spacing,
    Theme, ValidatedLayout, contains_control, git_symbol_values, highlight_style, invalid,
    palette_color, segments::valid_custom_identifier, style_open, valid_color, valid_identifier,
    valid_syntax_style_name,
};

pub(super) fn validate(theme: &Theme) -> io::Result<ValidatedLayout> {
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
    validate_custom_segments(theme)?;
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
            if !seen.insert(segment.clone()) {
                return Err(invalid(format!("segment `{name}` appears more than once")));
            }
            if let Some(async_segment) = segment.asynchronous() {
                asynchronous.push(async_segment);
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
        if !seen.insert(segment.clone()) {
            return Err(invalid(format!("segment `{name}` appears more than once")));
        }
        right.push(segment);
        total += 1;
    }
    if total > MAX_LAYOUT_SEGMENTS {
        return Err(invalid("layout contains more than 64 segments"));
    }

    if let Some((line_index, item_index)) = find_segment(&lines, LayoutSegment::is_character)
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

fn parse_layout_segment(name: &str, theme: &Theme) -> io::Result<LayoutSegment> {
    if let Some(segment) = SegmentId::parse(name) {
        if let SegmentId::Runtime(runtime) = segment
            && !theme.segments.runtimes.contains_key(runtime.name())
        {
            return Err(invalid(format!(
                "missing configuration for runtime segment `{name}`"
            )));
        }
        return Ok(LayoutSegment::BuiltIn(segment));
    }
    if valid_custom_identifier(name) {
        if !theme.segments.custom.contains_key(name) {
            return Err(invalid(format!(
                "missing configuration for custom segment `{name}`"
            )));
        }
        return Ok(LayoutSegment::Custom(name.to_owned()));
    }
    Err(invalid(format!("unknown segment `{name}`")))
}

fn validate_custom_segments(theme: &Theme) -> io::Result<()> {
    for (name, custom) in &theme.segments.custom {
        if !valid_custom_identifier(name) {
            return Err(invalid(format!("invalid custom segment id `{name}`")));
        }
        for (field, value) in [
            ("prefix", custom.prefix.as_str()),
            ("suffix", custom.suffix.as_str()),
        ] {
            if contains_control(value) {
                return Err(invalid(format!(
                    "segments.custom.{name}.{field} contains a control character"
                )));
            }
        }
        style_open(&custom.style, theme)?;
        validate_segment_spacing(name, custom.spacing)?;
    }
    Ok(())
}

fn find_segment(
    lines: &[Vec<LayoutSegment>],
    needle: impl Fn(&LayoutSegment) -> bool,
) -> Option<(usize, usize)> {
    lines.iter().enumerate().find_map(|(line_index, line)| {
        line.iter()
            .position(&needle)
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
