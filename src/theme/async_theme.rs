use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use crate::context::{Runtime, RuntimeValue};
use crate::gitstatus::{CONFLICTED, DELETED, STAGED, Snapshot, UNSTAGED, UNTRACKED};
use crate::prompt::prompt_text;

use super::{RuntimeTheme, STYLE_RESET, SegmentId, Theme, invalid, prompt_literal, style_open};

pub struct AsyncTheme {
    git: Option<GitRenderTheme>,
    runtimes: BTreeMap<Runtime, RuntimeRenderTheme>,
}

struct GitRenderTheme {
    before: String,
    prefix: String,
    suffix: String,
    after: String,
    symbol: String,
    style: String,
    action_prefix: String,
    action_suffix: String,
    action_style: String,
    changes_prefix: String,
    symbols: [String; 9],
    styles: [String; 9],
}

struct RuntimeRenderTheme {
    before: String,
    prefix: String,
    suffix: String,
    after: String,
    symbol: String,
    version_prefix: String,
    style: String,
    environment_prefix: String,
    environment_suffix: String,
    environment_style: String,
}

pub(super) fn compile(theme: &Theme, segments: &[SegmentId]) -> io::Result<AsyncTheme> {
    let mut git = None;
    let mut runtimes = BTreeMap::new();
    for segment in segments {
        match segment {
            SegmentId::Git => git = Some(compile_git(theme)?),
            SegmentId::Runtime(runtime) => {
                let runtime_theme = theme
                    .segments
                    .runtimes
                    .get(runtime.name())
                    .ok_or_else(|| invalid(format!("missing segments.{}", runtime.name())))?;
                runtimes.insert(*runtime, compile_runtime(runtime_theme, theme)?);
            }
            SegmentId::Directory | SegmentId::Character | SegmentId::Status => {}
        }
    }
    Ok(AsyncTheme { git, runtimes })
}

impl AsyncTheme {
    pub fn decode_hex(value: &str) -> io::Result<Self> {
        if !value.len().is_multiple_of(2) || value.len() > 128 * 1024 {
            return Err(invalid("compiled async theme has an invalid length"));
        }
        let mut bytes = Vec::with_capacity(value.len() / 2);
        for pair in value.as_bytes().chunks_exact(2) {
            let high = hex_digit(pair[0])
                .ok_or_else(|| invalid("compiled async theme is not hexadecimal"))?;
            let low = hex_digit(pair[1])
                .ok_or_else(|| invalid("compiled async theme is not hexadecimal"))?;
            bytes.push((high << 4) | low);
        }

        let mut decoder = Decoder::new(&bytes);
        let git = match decoder.u8()? {
            0 => None,
            1 => Some(GitRenderTheme {
                before: decoder.text()?,
                prefix: decoder.text()?,
                suffix: decoder.text()?,
                after: decoder.text()?,
                symbol: decoder.text()?,
                style: decoder.text()?,
                action_prefix: decoder.text()?,
                action_suffix: decoder.text()?,
                action_style: decoder.text()?,
                changes_prefix: decoder.text()?,
                symbols: decoder.text_array()?,
                styles: decoder.text_array()?,
            }),
            _ => return Err(invalid("compiled async theme has an invalid Git flag")),
        };

        let runtime_count = usize::from(decoder.u8()?);
        if runtime_count > Runtime::ALL.len() {
            return Err(invalid("compiled async theme has too many runtimes"));
        }
        let mut runtimes = BTreeMap::new();
        for _ in 0..runtime_count {
            let runtime = Runtime::from_id(decoder.u8()?)
                .ok_or_else(|| invalid("compiled async theme has an unknown runtime"))?;
            let value = RuntimeRenderTheme {
                before: decoder.text()?,
                prefix: decoder.text()?,
                suffix: decoder.text()?,
                after: decoder.text()?,
                symbol: decoder.text()?,
                version_prefix: decoder.text()?,
                style: decoder.text()?,
                environment_prefix: decoder.text()?,
                environment_suffix: decoder.text()?,
                environment_style: decoder.text()?,
            };
            if runtimes.insert(runtime, value).is_some() {
                return Err(invalid("compiled async theme repeats a runtime"));
            }
        }
        if !decoder.is_empty() {
            return Err(invalid("compiled async theme has trailing data"));
        }
        Ok(Self { git, runtimes })
    }

    pub const fn git_enabled(&self) -> bool {
        self.git.is_some()
    }

    pub fn runtimes(&self) -> Vec<Runtime> {
        self.runtimes.keys().copied().collect()
    }

    pub fn render_git(&self, snapshot: Option<&Snapshot>) -> String {
        let Some(theme) = self.git.as_ref() else {
            return String::new();
        };
        let Some(snapshot) = snapshot else {
            return String::new();
        };
        let branch = if snapshot.branch.is_empty() {
            snapshot.oid.get(..8).unwrap_or(&snapshot.oid)
        } else {
            &snapshot.branch
        };
        if branch.is_empty() {
            return String::new();
        }

        let mut fragment = format!(
            "{}{}{}{}{}{STYLE_RESET}",
            theme.before,
            theme.style,
            theme.prefix,
            theme.symbol,
            prompt_text(branch)
        );
        if !snapshot.action.is_empty() {
            write!(
                fragment,
                "{}{}{}{}{STYLE_RESET}",
                theme.action_style,
                theme.action_prefix,
                prompt_text(&snapshot.action),
                theme.action_suffix
            )
            .expect("writing to a String cannot fail");
        }

        let mut markers = String::new();
        if snapshot.ahead > 0 && snapshot.behind > 0 {
            styled_marker(
                &mut markers,
                &theme.styles[7],
                &theme.symbols[7],
                &format!("{}/{}", snapshot.ahead, snapshot.behind),
            );
        } else {
            if snapshot.ahead > 0 {
                styled_marker(
                    &mut markers,
                    &theme.styles[5],
                    &theme.symbols[5],
                    &snapshot.ahead.to_string(),
                );
            }
            if snapshot.behind > 0 {
                styled_marker(
                    &mut markers,
                    &theme.styles[6],
                    &theme.symbols[6],
                    &snapshot.behind.to_string(),
                );
            }
        }
        if snapshot.stashes > 0 {
            styled_marker(
                &mut markers,
                &theme.styles[8],
                &theme.symbols[8],
                &snapshot.stashes.to_string(),
            );
        }
        for (bit, index) in [
            (CONFLICTED, 0),
            (STAGED, 1),
            (UNSTAGED, 2),
            (DELETED, 3),
            (UNTRACKED, 4),
        ] {
            if snapshot.changes & bit != 0 {
                styled_marker(
                    &mut markers,
                    &theme.styles[index],
                    &theme.symbols[index],
                    "",
                );
            }
        }
        if !markers.is_empty() {
            fragment.push_str(&theme.changes_prefix);
            fragment.push_str(&markers);
        }
        if !theme.suffix.is_empty() {
            write!(fragment, "{}{}{STYLE_RESET}", theme.style, theme.suffix)
                .expect("writing to a String cannot fail");
        }
        fragment.push_str(&theme.after);
        fragment
    }

    pub fn render_runtime(&self, value: &RuntimeValue) -> Option<String> {
        let theme = self.runtimes.get(&value.runtime)?;
        if value.version.is_empty() {
            return Some(String::new());
        }
        let mut fragment = format!(
            "{}{}{}{}",
            theme.before, theme.style, theme.prefix, theme.symbol
        );
        if let Some(label) = value.label.as_deref() {
            write!(fragment, "{} ", prompt_text(label)).expect("writing to a String cannot fail");
        }
        write!(
            fragment,
            "{}{}{STYLE_RESET}",
            theme.version_prefix,
            prompt_text(&value.version)
        )
        .expect("writing to a String cannot fail");
        if let Some(environment) = value.environment.as_deref() {
            write!(
                fragment,
                "{}{}{}{}{STYLE_RESET}",
                theme.environment_style,
                theme.environment_prefix,
                prompt_text(environment),
                theme.environment_suffix
            )
            .expect("writing to a String cannot fail");
        }
        if !theme.suffix.is_empty() {
            write!(fragment, "{}{}{STYLE_RESET}", theme.style, theme.suffix)
                .expect("writing to a String cannot fail");
        }
        fragment.push_str(&theme.after);
        Some(fragment)
    }

    pub(super) fn encode_hex(&self) -> io::Result<String> {
        let mut bytes = Vec::with_capacity(1_024);
        match self.git.as_ref() {
            Some(git) => {
                bytes.push(1);
                for value in [
                    &git.before,
                    &git.prefix,
                    &git.suffix,
                    &git.after,
                    &git.symbol,
                    &git.style,
                    &git.action_prefix,
                    &git.action_suffix,
                    &git.action_style,
                    &git.changes_prefix,
                ] {
                    encode_text(&mut bytes, value)?;
                }
                for value in &git.symbols {
                    encode_text(&mut bytes, value)?;
                }
                for value in &git.styles {
                    encode_text(&mut bytes, value)?;
                }
            }
            None => bytes.push(0),
        }
        bytes.push(
            u8::try_from(self.runtimes.len())
                .map_err(|_| invalid("compiled async theme has too many runtimes"))?,
        );
        for (runtime, theme) in &self.runtimes {
            bytes.push(runtime.id());
            for value in [
                &theme.before,
                &theme.prefix,
                &theme.suffix,
                &theme.after,
                &theme.symbol,
                &theme.version_prefix,
                &theme.style,
                &theme.environment_prefix,
                &theme.environment_suffix,
                &theme.environment_style,
            ] {
                encode_text(&mut bytes, value)?;
            }
        }

        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(output)
    }
}

fn compile_git(theme: &Theme) -> io::Result<GitRenderTheme> {
    let git = &theme.segments.git;
    Ok(GitRenderTheme {
        before: " ".repeat(usize::from(git.spacing.before)),
        prefix: prompt_literal(&git.prefix),
        suffix: prompt_literal(&git.suffix),
        after: " ".repeat(usize::from(git.spacing.after)),
        symbol: prompt_literal(&git.symbol),
        style: style_open(&git.style, theme)?,
        action_prefix: prompt_literal(&git.action_prefix),
        action_suffix: prompt_literal(&git.action_suffix),
        action_style: style_open(&git.action_style, theme)?,
        changes_prefix: prompt_literal(&git.changes_prefix),
        symbols: [
            &git.symbols.conflicted,
            &git.symbols.staged,
            &git.symbols.modified,
            &git.symbols.deleted,
            &git.symbols.untracked,
            &git.symbols.ahead,
            &git.symbols.behind,
            &git.symbols.diverged,
            &git.symbols.stash,
        ]
        .map(|value| prompt_literal(value)),
        styles: [
            &git.styles.conflicted,
            &git.styles.staged,
            &git.styles.modified,
            &git.styles.deleted,
            &git.styles.untracked,
            &git.styles.ahead,
            &git.styles.behind,
            &git.styles.diverged,
            &git.styles.stash,
        ]
        .map(|style| style_open(style, theme))
        .into_iter()
        .collect::<io::Result<Vec<_>>>()?
        .try_into()
        .map_err(|_| invalid("Git style table has an invalid length"))?,
    })
}

fn compile_runtime(value: &RuntimeTheme, theme: &Theme) -> io::Result<RuntimeRenderTheme> {
    Ok(RuntimeRenderTheme {
        before: " ".repeat(usize::from(value.spacing.before)),
        prefix: prompt_literal(&value.prefix),
        suffix: prompt_literal(&value.suffix),
        after: " ".repeat(usize::from(value.spacing.after)),
        symbol: prompt_literal(&value.symbol),
        version_prefix: prompt_literal(&value.version_prefix),
        style: style_open(&value.style, theme)?,
        environment_prefix: prompt_literal(&value.environment.prefix),
        environment_suffix: prompt_literal(&value.environment.suffix),
        environment_style: style_open(&value.environment.style, theme)?,
    })
}

fn styled_marker(output: &mut String, style: &str, symbol: &str, value: &str) {
    write!(output, "{style}{symbol}{value}{STYLE_RESET}").expect("writing to a String cannot fail");
}

fn encode_text(output: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let length =
        u16::try_from(value.len()).map_err(|_| invalid("compiled theme field is too large"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| invalid("compiled async theme length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("compiled async theme is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> io::Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| invalid("compiled async theme is truncated"))
    }

    fn u16(&mut self) -> io::Result<u16> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| invalid("compiled async theme is truncated"))?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn text(&mut self) -> io::Result<String> {
        let length = usize::from(self.u16()?);
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| invalid("compiled async theme contains invalid UTF-8"))
    }

    fn text_array<const N: usize>(&mut self) -> io::Result<[String; N]> {
        let mut values = Vec::with_capacity(N);
        for _ in 0..N {
            values.push(self.text()?);
        }
        values
            .try_into()
            .map_err(|_| invalid("compiled async theme table has an invalid length"))
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{AsyncTheme, compile};
    use crate::context::{Runtime, RuntimeValue};
    use crate::gitstatus::{CONFLICTED, DELETED, STAGED, Snapshot, UNSTAGED, UNTRACKED};
    use crate::theme::{CATPPUCCIN_MOCHA_THEME, SegmentId, Theme, parse_versioned, validate};

    fn theme() -> Theme {
        parse_versioned(CATPPUCCIN_MOCHA_THEME, "Catppuccin Mocha")
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn compiled() -> AsyncTheme {
        let theme = theme();
        validate(&theme).unwrap();
        compile(
            &theme,
            &[SegmentId::Git, SegmentId::Runtime(Runtime::Python)],
        )
        .unwrap()
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            worktree: PathBuf::from("/repo"),
            oid: "0123456789abcdef".to_owned(),
            branch: "main".to_owned(),
            action: String::new(),
            ahead: 0,
            behind: 0,
            stashes: 0,
            changes: 0,
        }
    }

    #[test]
    fn encoded_theme_preserves_rendering_behavior() {
        let original = compiled();
        let decoded = AsyncTheme::decode_hex(&original.encode_hex().unwrap()).unwrap();
        let mut git = snapshot();
        git.ahead = 2;
        git.changes = STAGED | UNTRACKED;
        let runtime = RuntimeValue {
            runtime: Runtime::Python,
            version: "3.14.6".to_owned(),
            label: Some("cpython".to_owned()),
            environment: Some(".venv".to_owned()),
        };

        assert_eq!(
            decoded.render_git(Some(&git)),
            original.render_git(Some(&git))
        );
        assert_eq!(
            decoded.render_runtime(&runtime),
            original.render_runtime(&runtime)
        );
        assert_eq!(decoded.runtimes(), [Runtime::Python]);
    }

    #[test]
    fn git_rendering_handles_detached_heads_and_marker_order() {
        let theme = compiled();
        let mut git = snapshot();
        git.branch.clear();
        git.action = "rebase%onto\nmain".to_owned();
        git.ahead = 2;
        git.behind = 1;
        git.stashes = 3;
        git.changes = CONFLICTED | STAGED | UNSTAGED | DELETED | UNTRACKED;

        let rendered = theme.render_git(Some(&git));
        assert!(rendered.contains("01234567"));
        assert!(rendered.contains("rebase%%onto main"));
        assert!(rendered.contains("⇕2/1"));
        assert!(rendered.contains("$3"));

        let markers = ["=", "+", "!", "✘", "?"];
        let mut position = 0;
        for marker in markers {
            let next = rendered[position..].find(marker).unwrap() + position;
            assert!(next >= position);
            position = next + marker.len();
        }
    }

    #[test]
    fn empty_git_and_runtime_values_do_not_render_content() {
        let theme = compiled();
        assert_eq!(theme.render_git(None), "");

        let mut git = snapshot();
        git.branch.clear();
        git.oid.clear();
        assert_eq!(theme.render_git(Some(&git)), "");

        let runtime = RuntimeValue {
            runtime: Runtime::Python,
            version: String::new(),
            label: None,
            environment: None,
        };
        assert_eq!(theme.render_runtime(&runtime).as_deref(), Some(""));
    }

    #[test]
    fn runtime_rendering_sanitizes_dynamic_values() {
        let rendered = compiled()
            .render_runtime(&RuntimeValue {
                runtime: Runtime::Python,
                version: "3.14%dev".to_owned(),
                label: Some("cpython\nfast".to_owned()),
                environment: Some("venv\tone".to_owned()),
            })
            .unwrap();

        assert!(rendered.contains("cpython fast"));
        assert!(rendered.contains("3.14%%dev"));
        assert!(rendered.contains("venv one"));
    }

    #[test]
    fn malformed_encoded_themes_are_rejected() {
        assert!(AsyncTheme::decode_hex("0").is_err());
        assert!(AsyncTheme::decode_hex("zz").is_err());
        assert!(AsyncTheme::decode_hex("02").is_err());

        let encoded = compiled().encode_hex().unwrap();
        for length in (0..encoded.len()).step_by(2) {
            assert!(
                AsyncTheme::decode_hex(&encoded[..length]).is_err(),
                "accepted byte length {}",
                length / 2
            );
        }
        let mut trailing = encoded;
        trailing.push_str("00");
        assert!(AsyncTheme::decode_hex(&trailing).is_err());
    }
}
