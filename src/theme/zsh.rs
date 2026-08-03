use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;

use super::{
    CharacterTheme, CompiledTheme, CustomSegmentTheme, DirectoryTheme, InputTheme, Layout,
    LayoutSegment, STYLE_RESET, SegmentId, Segments, StatusTheme, Theme, ValidatedLayout,
    async_theme, highlight_style, line_editor_style, prompt_literal, shell_quote, style_open,
};

impl CompiledTheme {
    pub fn zsh(&self) -> io::Result<String> {
        let mut output = String::with_capacity(4 * 1024);
        let theme = &self.theme;

        scalar(&mut output, "__ZTHEME_THEME_SELECTOR", &self.selector);
        scalar(
            &mut output,
            "__ZTHEME_LAYOUT_SEPARATOR",
            &prompt_literal(&theme.layout.separator),
        );
        let asynchronous = async_theme::compile(theme, &self.layout.asynchronous)?;
        scalar(
            &mut output,
            "__ZTHEME_ASYNC_THEME",
            &asynchronous.encode_hex()?,
        );
        integer(
            &mut output,
            "__ZTHEME_HAS_ASYNC",
            u64::from(!self.layout.asynchronous.is_empty()),
        );
        // Per-asynchronous-group presence flags let the shell compute how many
        // groups are actually pending (and can be locked) for this layout.
        integer(
            &mut output,
            "__ZTHEME_HAS_GIT",
            u64::from(self.layout.asynchronous.contains(&SegmentId::Git)),
        );
        integer(
            &mut output,
            "__ZTHEME_HAS_RUNTIME",
            u64::from(
                self.layout
                    .asynchronous
                    .iter()
                    .any(|segment| matches!(segment, SegmentId::Runtime(_))),
            ),
        );
        emit_input_theme(&mut output, theme, &theme.input)?;
        emit_segment_themes(&mut output, theme, &theme.segments, &self.layout)?;
        emit_sync_segments(&mut output, &self.layout);

        emit_segment_state(&mut output, &self.layout);
        emit_clear_async(&mut output, &self.layout.asynchronous);
        emit_clear_async_group(&mut output, &self.layout.asynchronous);
        emit_assign_async(&mut output, &self.layout.asynchronous);
        emit_layout_renderer(&mut output, &self.layout, &theme.layout);
        Ok(output)
    }
}

fn emit_input_theme(output: &mut String, theme: &Theme, input: &InputTheme) -> io::Result<()> {
    let completion = format!(
        "{}-- %d --{STYLE_RESET}",
        style_open(&input.completion, theme)?
    );
    writeln!(
        output,
        "zstyle ':completion:*' format {}",
        shell_quote(&completion)
    )
    .expect("writing to a String cannot fail");
    scalar(
        output,
        "ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE",
        &line_editor_style(&input.autosuggestion, theme)?,
    );
    output.push_str("typeset -gA ZSH_HIGHLIGHT_STYLES\n");
    for (name, style) in &input.syntax {
        writeln!(
            output,
            "ZSH_HIGHLIGHT_STYLES[{name}]={}",
            shell_quote(&highlight_style(style, theme)?)
        )
        .expect("writing to a String cannot fail");
    }
    Ok(())
}

/// Emits the styling shared by every synchronous segment: the per-variant
/// OPEN/CLOSE maps `_ztheme_segment_render` wraps values with, the raw
/// character/status symbols, and the directory truncation parameters.
fn emit_segment_themes(
    output: &mut String,
    theme: &Theme,
    segments: &Segments,
    layout: &ValidatedLayout,
) -> io::Result<()> {
    output.push_str("typeset -gA __ZTHEME_SEGMENT_OPEN\n");
    output.push_str("typeset -gA __ZTHEME_SEGMENT_CLOSE\n");

    emit_directory_themes(output, theme, &segments.directory)?;
    emit_clock_themes(output, theme, &segments.clock)?;
    emit_character_themes(output, theme, &segments.character)?;
    emit_status_themes(output, theme, &segments.status)?;
    emit_custom_style_entries(output, theme, segments, layout)
}

/// Emits the directory scalars (home symbol, truncation symbol, and the
/// width parameters) plus its default style entry.
fn emit_directory_themes(
    output: &mut String,
    theme: &Theme,
    directory: &DirectoryTheme,
) -> io::Result<()> {
    scalar(
        output,
        "__ZTHEME_DIRECTORY_HOME",
        &prompt_literal(&directory.home_symbol),
    );
    scalar(
        output,
        "__ZTHEME_DIRECTORY_TRUNCATION",
        &prompt_literal(&directory.truncation_symbol),
    );
    integer(
        output,
        "__ZTHEME_DIRECTORY_PERCENT",
        u64::from(directory.width.percent),
    );
    integer(
        output,
        "__ZTHEME_DIRECTORY_MINIMUM",
        u64::from(directory.width.minimum),
    );
    integer(
        output,
        "__ZTHEME_DIRECTORY_MAXIMUM",
        u64::from(directory.width.maximum),
    );
    emit_style_entry(
        output,
        theme,
        &directory.style,
        &directory.prefix,
        &directory.suffix,
        directory.spacing,
        "directory:default",
    )
}

/// Emits the clock style entry; clock shares `CustomSegmentTheme` with
/// custom segments, so this is a single default entry.
fn emit_clock_themes(
    output: &mut String,
    theme: &Theme,
    clock: &CustomSegmentTheme,
) -> io::Result<()> {
    emit_style_entry(
        output,
        theme,
        &clock.style,
        &clock.prefix,
        &clock.suffix,
        clock.spacing,
        "clock:default",
    )
}

/// Emits the character success/error symbols plus both variant style
/// entries.
fn emit_character_themes(
    output: &mut String,
    theme: &Theme,
    character: &CharacterTheme,
) -> io::Result<()> {
    scalar(
        output,
        "__ZTHEME_CHARACTER_SUCCESS_SYMBOL",
        &prompt_literal(&character.success),
    );
    scalar(
        output,
        "__ZTHEME_CHARACTER_ERROR_SYMBOL",
        &prompt_literal(&character.error),
    );
    emit_style_entry(
        output,
        theme,
        &character.success_style,
        &character.prefix,
        &character.suffix,
        character.spacing,
        "character:success",
    )?;
    emit_style_entry(
        output,
        theme,
        &character.error_style,
        &character.prefix,
        &character.suffix,
        character.spacing,
        "character:error",
    )
}

/// Emits the status flag, success symbol, and both variant style entries.
fn emit_status_themes(output: &mut String, theme: &Theme, status: &StatusTheme) -> io::Result<()> {
    integer(
        output,
        "__ZTHEME_STATUS_SHOW_SUCCESS",
        u64::from(status.show_success),
    );
    scalar(
        output,
        "__ZTHEME_STATUS_SUCCESS_SYMBOL",
        &prompt_literal(&status.success_symbol),
    );
    emit_style_entry(
        output,
        theme,
        &status.success_style,
        &status.prefix,
        &status.suffix,
        status.spacing,
        "status:success",
    )?;
    emit_style_entry(
        output,
        theme,
        &status.style,
        &status.prefix,
        &status.suffix,
        status.spacing,
        "status:error",
    )
}

fn emit_custom_style_entries(
    output: &mut String,
    theme: &Theme,
    segments: &Segments,
    layout: &ValidatedLayout,
) -> io::Result<()> {
    for segment in layout.lines.iter().flatten().chain(layout.right.iter()) {
        let LayoutSegment::Custom(id) = segment else {
            continue;
        };
        let Some(custom) = segments.custom.get(id) else {
            continue;
        };
        emit_style_entry(
            output,
            theme,
            &custom.style,
            &custom.prefix,
            &custom.suffix,
            custom.spacing,
            &format!("{id}:default"),
        )?;
    }
    Ok(())
}

fn emit_style_entry(
    output: &mut String,
    theme: &Theme,
    style: &super::Style,
    prefix: &str,
    suffix: &str,
    spacing: super::Spacing,
    key: &str,
) -> io::Result<()> {
    let before = " ".repeat(usize::from(spacing.before));
    let after = " ".repeat(usize::from(spacing.after));
    let open = format!(
        "{before}{}{}",
        style_open(style, theme)?,
        prompt_literal(prefix)
    );
    let close = format!("{}{STYLE_RESET}{after}", prompt_literal(suffix));
    writeln!(
        output,
        "__ZTHEME_SEGMENT_OPEN[{key}]={}",
        shell_quote(&open)
    )
    .expect("writing to a String cannot fail");
    writeln!(
        output,
        "__ZTHEME_SEGMENT_CLOSE[{key}]={}",
        shell_quote(&close)
    )
    .expect("writing to a String cannot fail");
    Ok(())
}

/// Emits the ordered list of synchronous (shell-provided) segment ids present
/// in the active layout. The ids are validated identifiers, safe to emit
/// unquoted.
fn emit_sync_segments(output: &mut String, layout: &ValidatedLayout) {
    output.push_str("typeset -ga __ZTHEME_SYNC_SEGMENTS=(");
    for id in layout.sync_ids() {
        output.push_str(id);
        output.push(' ');
    }
    output.push_str(")\n");
}

fn emit_segment_state(output: &mut String, layout: &ValidatedLayout) {
    let mut seen = HashSet::new();
    for segment in layout.lines.iter().flatten().chain(layout.right.iter()) {
        if seen.insert(segment.name()) {
            writeln!(output, "typeset -g {}=''", segment_variable(segment))
                .expect("writing to a String cannot fail");
        }
    }
}

fn emit_clear_async(output: &mut String, segments: &[SegmentId]) {
    output.push_str("_ztheme_clear_async_segments() {\n    emulate -L zsh\n");
    for segment in segments {
        writeln!(
            output,
            "    {}=''",
            segment_variable(&LayoutSegment::BuiltIn(*segment))
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("}\n");
}

/// Emits a group-scoped async clear dispatcher. The shell's error records carry
/// a group name (`git` or `runtime`); clearing only that group keeps a
/// successful unlocked group from being erased when another group reports an
/// error.
fn emit_clear_async_group(output: &mut String, segments: &[SegmentId]) {
    output.push_str("_ztheme_clear_async_group() {\n    emulate -L zsh\n    case \"$1\" in\n");
    if segments.contains(&SegmentId::Git) {
        writeln!(
            output,
            "        git)\n            {}=''\n            ;;\n",
            segment_variable(&LayoutSegment::BuiltIn(SegmentId::Git))
        )
        .expect("writing to a String cannot fail");
    }
    let runtimes = segments
        .iter()
        .filter_map(|segment| match segment {
            SegmentId::Runtime(runtime) => Some(*runtime),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !runtimes.is_empty() {
        output.push_str("        runtime)\n");
        for runtime in runtimes {
            writeln!(
                output,
                "            {}=''",
                segment_variable(&LayoutSegment::BuiltIn(SegmentId::Runtime(runtime)))
            )
            .expect("writing to a String cannot fail");
        }
        output.push_str("            ;;\n");
    }
    output.push_str("        *) return 1 ;;\n    esac\n}\n");
}

fn emit_assign_async(output: &mut String, segments: &[SegmentId]) {
    output.push_str("_ztheme_assign_async_segment() {\n    emulate -L zsh\n    case \"$1\" in\n");
    for segment in segments {
        let variable = segment_variable(&LayoutSegment::BuiltIn(*segment));
        writeln!(
            output,
            "        {})\n            [[ \"${{{variable}}}\" == \"$2\" ]] && return 1\n            {variable}=\"$2\"\n            ;;\n",
            segment.name()
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("        *) return 2 ;;\n    esac\n}\n");
}

fn emit_layout_renderer(output: &mut String, layout: &ValidatedLayout, source: &Layout) {
    output.push_str(
        "_ztheme_render_layout(){\n    emulate -L zsh\n    (( ! ZTHEME_LOCKED_PENDING )) || return\n    local prompt='' right='' line='' separator=''\n",
    );
    if source.blank_line_before {
        output.push_str("    prompt=$'\\n'\n");
    }
    for (index, line) in layout.lines.iter().enumerate() {
        output.push_str("    line=''\n    separator=''\n");
        for segment in line {
            emit_layout_segment(output, segment, "line");
        }
        output.push_str("    prompt+=\"$line\"\n");
        if index + 1 != layout.lines.len() {
            output.push_str("    prompt+=$'\\n'\n");
        }
    }

    output.push_str("    line=''\n    separator=''\n");
    for segment in &layout.right {
        emit_layout_segment(output, segment, "line");
    }
    output.push_str(
        "    right=\"$line\"\n    ZTHEME_PROMPT=\"$prompt\"\n    ZTHEME_RPROMPT=\"$right\"\n}\n",
    );
}

fn emit_layout_segment(output: &mut String, segment: &LayoutSegment, target: &str) {
    let variable = segment_variable(segment);
    writeln!(
        output,
        "    if [[ -n \"${{{variable}}}\" ]]; then\n        {target}+=\"${{separator}}${{{variable}}}\"\n        separator=\"$__ZTHEME_LAYOUT_SEPARATOR\"\n    fi"
    )
    .expect("writing to a String cannot fail");
}

fn segment_variable(segment: &LayoutSegment) -> String {
    format!("ZTHEME_SEGMENT_{}", segment.name().to_ascii_uppercase())
}

fn scalar(output: &mut String, name: &str, value: &str) {
    writeln!(output, "typeset -g {name}={}", shell_quote(value))
        .expect("writing to a String cannot fail");
}

fn integer(output: &mut String, name: &str, value: u64) {
    writeln!(output, "typeset -gi {name}={value}").expect("writing to a String cannot fail");
}
