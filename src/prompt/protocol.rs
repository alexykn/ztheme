use std::io::{self, Write};

// ---------------------------------------------------------------------------
// Request protocol
//
// The NUL-delimited request the shell writes on every async prompt: magic,
// version, generation, cwd, then the environment fields in `REQUEST_FIELDS`
// order. `read_request` parses exactly this layout and `init_zsh` writes it
// into the generated shell integration from the same definition, so the two
// sides cannot drift apart.
// ---------------------------------------------------------------------------

pub(crate) const REQUEST_MAGIC: &[u8] = b"ZTREQ";
pub(crate) const REQUEST_VERSION: &str = "3";

/// Ordered request environment fields, in wire order. A field added here
/// reaches the daemon parser (via the pinned field count in the `client.rs`
/// tests) and the generated shell integration (via `init_zsh`) together.
pub(crate) const REQUEST_FIELDS: &[&str] = &[
    "PATH",
    "HOME",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CEILING_DIRECTORIES",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "PERLBREW_PERL",
    "PLENV_VERSION",
    "PYENV_VERSION",
    "PYENV_DIR",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "RBENV_DIR",
    "RBENV_VERSION",
    "NODENV_VERSION",
    "NODENV_DIR",
    "PLENV_DIR",
    "RUBY_VERSION",
    "JAVA_HOME",
    "GOTOOLCHAIN",
    "DOTNET_ROOT",
    "JULIAUP_CHANNEL",
    "JULIAUP_DEPOT_PATH",
    "JULIA_PROJECT",
    "JULIA_LOAD_PATH",
    "JULIA_DEPOT_PATH",
    "R_ARCH",
];

/// Request fields the shell's prompt-refresh change key omits. The shell
/// tracks PATH itself (appended raw at the end of the key), and HOME and
/// `GIT_CEILING_DIRECTORIES` never change the rendered prompt.
pub(crate) const CONTEXT_EXCLUDED: &[&str] = &["PATH", "HOME", "GIT_CEILING_DIRECTORIES"];

/// One `request_line+=` line per request field, in wire order. Splice target:
/// `@ZTHEME_REQUEST_FIELDS@` in `shell/ztheme.zsh`.
pub(crate) fn request_field_lines() -> String {
    REQUEST_FIELDS
        .iter()
        .map(|field| format!("    request_line+=\"${{{field}:-}}\"$'\\0'"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `context_key+=` line per change-detection field (`REQUEST_FIELDS`
/// minus `CONTEXT_EXCLUDED`), in request order. Splice target:
/// `@ZTHEME_CONTEXT_FIELDS@` in `shell/ztheme.zsh`.
pub(crate) fn context_field_lines() -> String {
    REQUEST_FIELDS
        .iter()
        .copied()
        .filter(|field| !CONTEXT_EXCLUDED.contains(field))
        .map(|field| format!("    context_key+=\"|${{{field}:-}}\""))
        .collect::<Vec<_>>()
        .join("\n")
}

const MAGIC: &str = "ZTHEME1";

pub fn prompt_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '\t' | '\r' | '\n' => escaped.push(' '),
            character if character.is_control() => escaped.push('?'),
            character => escaped.push(character),
        }
    }

    escaped
}

pub fn write_segment(
    output: &mut impl Write,
    generation: u64,
    segment: &str,
    fragment: &str,
) -> io::Result<()> {
    write_fields(output, generation, &["segment", segment, fragment])
}

pub fn write_error(
    output: &mut impl Write,
    generation: u64,
    segment: &str,
    message: &str,
) -> io::Result<()> {
    let message = prompt_text(message);
    write_fields(output, generation, &["error", segment, &message])
}

pub fn write_done(output: &mut impl Write, generation: u64) -> io::Result<()> {
    write_fields(output, generation, &["done"])
}

/// Records that one asynchronous group (git or runtime) has finished producing
/// its segment records. The shell uses these to release the rendering barrier
/// for locked groups, so the prompt can appear as soon as every *locked* group
/// is complete instead of waiting for the final `done`.
pub fn write_complete(output: &mut impl Write, generation: u64, group: &str) -> io::Result<()> {
    write_fields(output, generation, &["complete", group])
}

fn write_fields(output: &mut impl Write, generation: u64, fields: &[&str]) -> io::Result<()> {
    write!(output, "{MAGIC}\t{generation}")?;
    for field in fields {
        write!(output, "\t{field}")?;
    }
    writeln!(output)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::{prompt_text, write_complete, write_done, write_error, write_segment};

    #[test]
    fn prompt_text_escapes_prompt_sequences_and_controls() {
        assert_eq!(prompt_text("100%\tready\n\u{1b}"), "100%% ready ?");
        assert_eq!(prompt_text("Grüße 🚀"), "Grüße 🚀");
    }

    #[test]
    fn records_have_exact_tab_delimited_framing() {
        let mut output = Vec::new();
        write_segment(&mut output, 7, "git", " main").unwrap();
        write_complete(&mut output, 7, "git").unwrap();
        write_error(&mut output, 7, "runtime", "bad\tvalue\nnext").unwrap();
        write_complete(&mut output, 7, "runtime").unwrap();
        write_done(&mut output, 7).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "ZTHEME1\t7\tsegment\tgit\t main\n\
             ZTHEME1\t7\tcomplete\tgit\n\
             ZTHEME1\t7\terror\truntime\tbad value next\n\
             ZTHEME1\t7\tcomplete\truntime\n\
             ZTHEME1\t7\tdone\n"
        );
    }
}
