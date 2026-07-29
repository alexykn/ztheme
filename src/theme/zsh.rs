use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;

use super::{
    CompiledTheme, DirectoryTheme, InputTheme, Layout, STYLE_RESET, SegmentId, Segments, Theme,
    ValidatedLayout, async_theme, highlight_style, line_editor_style, prompt_literal, shell_quote,
    style_open,
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
        emit_immediate_theme(&mut output, theme, &theme.segments)?;

        emit_segment_state(&mut output, &self.layout);
        emit_clear_async(&mut output, &self.layout.asynchronous);
        emit_assign_async(&mut output, &self.layout.asynchronous);
        emit_layout_renderer(&mut output, &self.layout, &theme.layout);
        Ok(output)
    }
}

fn emit_immediate_theme(output: &mut String, theme: &Theme, segments: &Segments) -> io::Result<()> {
    emit_input_theme(output, theme, &theme.input)?;
    emit_directory_theme(output, theme, &segments.directory)?;

    let character = &segments.character;
    let character_before = " ".repeat(usize::from(character.spacing.before));
    let character_after = " ".repeat(usize::from(character.spacing.after));
    scalar(
        output,
        "__ZTHEME_CHARACTER_SUCCESS",
        &format!(
            "{character_before}{}{}{}{}{STYLE_RESET}{character_after}",
            style_open(&character.success_style, theme)?,
            prompt_literal(&character.prefix),
            prompt_literal(&character.success),
            prompt_literal(&character.suffix)
        ),
    );
    scalar(
        output,
        "__ZTHEME_CHARACTER_ERROR",
        &format!(
            "{character_before}{}{}{}{}{STYLE_RESET}{character_after}",
            style_open(&character.error_style, theme)?,
            prompt_literal(&character.prefix),
            prompt_literal(&character.error),
            prompt_literal(&character.suffix)
        ),
    );

    let status = &segments.status;
    let status_before = " ".repeat(usize::from(status.spacing.before));
    let status_after = " ".repeat(usize::from(status.spacing.after));
    integer(
        output,
        "__ZTHEME_STATUS_SHOW_SUCCESS",
        u64::from(status.show_success),
    );
    scalar(
        output,
        "__ZTHEME_STATUS_SUCCESS",
        &format!(
            "{status_before}{}{}{}{}{STYLE_RESET}{status_after}",
            style_open(&status.success_style, theme)?,
            prompt_literal(&status.prefix),
            prompt_literal(&status.success_symbol),
            prompt_literal(&status.suffix)
        ),
    );
    scalar(
        output,
        "__ZTHEME_STATUS_OPEN",
        &format!(
            "{status_before}{}{}",
            style_open(&status.style, theme)?,
            prompt_literal(&status.prefix)
        ),
    );
    scalar(
        output,
        "__ZTHEME_STATUS_CLOSE",
        &format!(
            "{}{STYLE_RESET}{status_after}",
            prompt_literal(&status.suffix)
        ),
    );
    Ok(())
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

fn emit_directory_theme(
    output: &mut String,
    theme: &Theme,
    directory: &DirectoryTheme,
) -> io::Result<()> {
    let before = " ".repeat(usize::from(directory.spacing.before));
    let after = " ".repeat(usize::from(directory.spacing.after));
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
    scalar(
        output,
        "__ZTHEME_DIRECTORY_OPEN",
        &format!(
            "{before}{}{}",
            style_open(&directory.style, theme)?,
            prompt_literal(&directory.prefix)
        ),
    );
    scalar(
        output,
        "__ZTHEME_DIRECTORY_CLOSE",
        &format!("{}{STYLE_RESET}{after}", prompt_literal(&directory.suffix)),
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
    Ok(())
}

fn emit_segment_state(output: &mut String, layout: &ValidatedLayout) {
    let mut seen = HashSet::new();
    for segment in layout
        .lines
        .iter()
        .flatten()
        .chain(layout.right.iter())
        .copied()
    {
        if seen.insert(segment) {
            writeln!(output, "typeset -g {}=''", segment_variable(segment))
                .expect("writing to a String cannot fail");
        }
    }
}

fn emit_clear_async(output: &mut String, segments: &[SegmentId]) {
    output.push_str("_ztheme_clear_async_segments() {\n    emulate -L zsh\n");
    for segment in segments {
        writeln!(output, "    {}=''", segment_variable(*segment))
            .expect("writing to a String cannot fail");
    }
    output.push_str("}\n");
}

fn emit_assign_async(output: &mut String, segments: &[SegmentId]) {
    output.push_str("_ztheme_assign_async_segment() {\n    emulate -L zsh\n    case \"$1\" in\n");
    for segment in segments {
        let variable = segment_variable(*segment);
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
        "_ztheme_render_layout() {\n    emulate -L zsh\n    local prompt='' right='' line='' separator=''\n",
    );
    if source.blank_line_before {
        output.push_str("    prompt=$'\\n'\n");
    }
    for (index, line) in layout.lines.iter().enumerate() {
        output.push_str("    line=''\n    separator=''\n");
        for segment in line {
            emit_layout_segment(output, *segment, "line");
        }
        output.push_str("    prompt+=\"$line\"\n");
        if index + 1 != layout.lines.len() {
            output.push_str("    prompt+=$'\\n'\n");
        }
    }

    output.push_str("    line=''\n    separator=''\n");
    for segment in &layout.right {
        emit_layout_segment(output, *segment, "line");
    }
    output.push_str(
        "    right=\"$line\"\n    ZTHEME_PROMPT=\"$prompt\"\n    ZTHEME_RPROMPT=\"$right\"\n}\n",
    );
}

fn emit_layout_segment(output: &mut String, segment: SegmentId, target: &str) {
    let variable = segment_variable(segment);
    writeln!(
        output,
        "    if [[ -n \"${{{variable}}}\" ]]; then\n        {target}+=\"${{separator}}${{{variable}}}\"\n        separator=\"$__ZTHEME_LAYOUT_SEPARATOR\"\n    fi"
    )
    .expect("writing to a String cannot fail");
}

fn segment_variable(segment: SegmentId) -> String {
    format!("ZTHEME_SEGMENT_{}", segment.name().to_ascii_uppercase())
}

fn scalar(output: &mut String, name: &str, value: &str) {
    writeln!(output, "typeset -g {name}={}", shell_quote(value))
        .expect("writing to a String cannot fail");
}

fn integer(output: &mut String, name: &str, value: u64) {
    writeln!(output, "typeset -gi {name}={value}").expect("writing to a String cannot fail");
}
