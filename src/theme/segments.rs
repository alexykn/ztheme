//! Custom (user-defined) synchronous segment discovery and validation.
//!
//! Custom segments are opt-in per file. `config.toml` names the enabled ids,
//! and each enabled id used by the active layout must resolve to a regular
//! file `<config-root>/ztheme/segments/<id>.zsh` whose first line is the
//! versioned identity header. Discovery and header validation happen only
//! during `ztheme init zsh` / `ztheme theme reload`; the prompt hot path
//! performs no filesystem work.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{Config, SegmentId, invalid};

const MAX_CUSTOM_ID_BYTES: usize = 64;
const CUSTOM_SEGMENT_HEADER_PREFIX: &str = "# ztheme-segment-v1: ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedCustomSegment {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}

/// A custom id is 1-64 bytes, starts with a lowercase ASCII letter, and
/// continues with lowercase letters, digits, or underscores. Reserved ids
/// (bundled segments and every supported runtime) are rejected rather than
/// shadowed. The strict grammar keeps generated function and variable names
/// safe without `eval`.
pub(super) fn valid_custom_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=MAX_CUSTOM_ID_BYTES).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && SegmentId::parse(value).is_none()
}

/// Rejects malformed, reserved, or duplicated entries in the allowlist. The
/// whole allowlist is validated so a broken configuration fails
/// deterministically at init/reload rather than depending on which ids happen
/// to be active.
pub(super) fn validate_enabled_segments(enabled: &[String]) -> io::Result<()> {
    let mut seen = HashSet::new();
    for id in enabled {
        if !valid_custom_identifier(id) {
            return Err(invalid(format!(
                "invalid custom segment id `{id}` in [custom_segments] enabled"
            )));
        }
        if !seen.insert(id) {
            return Err(invalid(format!(
                "custom segment `{id}` appears more than once in [custom_segments] enabled"
            )));
        }
    }
    Ok(())
}

/// Resolves each active custom segment id to its exact file path and verifies
/// enablement, regularity, and the identity header. Returns one entry per
/// active id in deterministic layout order. Inactive enabled segments are
/// never sourced, and the directory is never scanned.
pub(super) fn resolve_custom_segments(
    config: &Config,
    active_ids: &[String],
    config_root: &Path,
) -> io::Result<Vec<ResolvedCustomSegment>> {
    let segments_dir = config_root.join("ztheme/segments");
    let mut resolved = Vec::with_capacity(active_ids.len());
    for id in active_ids {
        if !config
            .custom_segments
            .enabled
            .iter()
            .any(|enabled| enabled == id)
        {
            return Err(invalid(format!(
                "custom segment `{id}` is not enabled; add it to [custom_segments] in {}",
                config_root.join("ztheme/config.toml").display()
            )));
        }
        let path = segments_dir.join(format!("{id}.zsh"));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(invalid(format!(
                    "custom segment file {} does not exist",
                    path.display()
                )));
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() {
            return Err(invalid(format!(
                "custom segment file {} is not a regular file",
                path.display()
            )));
        }
        let first_line = first_line(&path)?;
        let expected = format!("{CUSTOM_SEGMENT_HEADER_PREFIX}{id}");
        if first_line != expected {
            return Err(invalid(format!(
                "custom segment file {} must start with `{expected}`",
                path.display()
            )));
        }
        resolved.push(ResolvedCustomSegment {
            id: id.clone(),
            path,
        });
    }
    Ok(resolved)
}

fn first_line(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..end])
        .trim_end_matches('\r')
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::{resolve_custom_segments, valid_custom_identifier, validate_enabled_segments};
    use crate::theme::tests_support::write_theme_files;
    use crate::theme::{Config, CustomSegmentsConfig};

    fn config(enabled: &[&str]) -> Config {
        Config {
            version: 1,
            theme: "test".to_owned(),
            custom_segments: CustomSegmentsConfig {
                enabled: enabled.iter().map(|value| (*value).to_owned()).collect(),
            },
        }
    }

    fn segments_dir(root: &std::path::Path) -> std::path::PathBuf {
        root.join("ztheme/segments")
    }

    #[test]
    fn identifier_grammar_accepts_and_rejects() {
        for valid in ["clock", "cpu_load", "a", "z9_0"] {
            assert!(valid_custom_identifier(valid), "rejected `{valid}`");
        }
        for invalid in [
            "",
            "Clock",
            "0clock",
            "clock-time",
            "clock.time",
            "clock time",
            "C",
            "a".repeat(65).as_str(),
            "git",
            "character",
            "status",
            "directory",
            "python",
            "rust",
            "node",
        ] {
            assert!(!valid_custom_identifier(invalid), "accepted `{invalid}`");
        }
    }

    #[test]
    fn allowlist_rejects_duplicates_and_invalid_ids() {
        assert!(validate_enabled_segments(&[]).is_ok());
        assert!(validate_enabled_segments(&["clock".to_owned()]).is_ok());
        assert!(validate_enabled_segments(&["clock".to_owned(), "cpu".to_owned()]).is_ok());

        assert!(
            validate_enabled_segments(&["clock".to_owned(), "clock".to_owned()]).is_err(),
            "accepted a duplicate"
        );
        assert!(
            validate_enabled_segments(&["clock-time".to_owned()]).is_err(),
            "accepted an invalid id"
        );
        assert!(
            validate_enabled_segments(&["git".to_owned()]).is_err(),
            "accepted a reserved id"
        );
        assert!(
            validate_enabled_segments(&["python".to_owned()]).is_err(),
            "accepted a runtime id"
        );
    }

    #[test]
    fn enabled_active_segment_resolves_with_matching_file_and_header() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("clock.zsh"),
                "# ztheme-segment-v1: clock\nztheme_segment_clock() { :; }\n",
            )
            .unwrap();
        });
        let resolved =
            resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "clock");
        assert_eq!(resolved[0].path, segments_dir(&root).join("clock.zsh"));
    }

    #[test]
    fn unlisted_random_files_are_ignored() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(segments_dir(home).join("random.zsh"), "print random\n").unwrap();
        });
        // A stray file is never scanned; resolving an unrelated id fails only
        // on the missing active file, and an empty active set resolves to
        // nothing.
        assert!(
            resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).is_err()
        );
        let resolved = resolve_custom_segments(&config(&[]), &[], &root).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn active_but_disabled_segment_fails() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("clock.zsh"),
                "# ztheme-segment-v1: clock\n",
            )
            .unwrap();
        });
        let error =
            resolve_custom_segments(&config(&[]), &["clock".to_owned()], &root).unwrap_err();
        assert!(error.to_string().contains("not enabled"));
    }

    #[test]
    fn missing_active_file_fails() {
        let (root, _guard) = write_theme_files(|_| {});
        let error =
            resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn headerless_file_fails() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("clock.zsh"),
                "ztheme_segment_clock() { :; }\n",
            )
            .unwrap();
        });
        assert!(
            resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).is_err()
        );
    }

    #[test]
    fn mismatched_or_unsupported_header_fails() {
        for header in [
            "# ztheme-segment-v1: watch\n",
            "# ztheme-segment-v2: clock\n",
            "# ztheme-segment-v1:cloc\n",
            " # ztheme-segment-v1: clock\n",
        ] {
            let (root, _guard) = write_theme_files(|home| {
                std::fs::write(segments_dir(home).join("clock.zsh"), header).unwrap();
            });
            let error = resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root)
                .unwrap_err();
            assert!(
                error.to_string().contains("must start with"),
                "accepted header {header:?}: {error}"
            );
        }
    }

    #[test]
    fn symlink_and_non_regular_files_fail() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("target.zsh"),
                "# ztheme-segment-v1: clock\n",
            )
            .unwrap();
            std::os::unix::fs::symlink(
                segments_dir(home).join("target.zsh"),
                segments_dir(home).join("clock.zsh"),
            )
            .unwrap();
            std::fs::create_dir_all(segments_dir(home).join("cpu.zsh")).unwrap();
        });
        assert!(
            resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).is_err(),
            "accepted a symlink"
        );
        assert!(
            resolve_custom_segments(&config(&["cpu"]), &["cpu".to_owned()], &root).is_err(),
            "accepted a directory"
        );
    }

    #[test]
    fn enabled_but_inactive_segment_is_not_sourced() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("clock.zsh"),
                "# ztheme-segment-v1: clock\n",
            )
            .unwrap();
            std::fs::write(
                segments_dir(home).join("cpu.zsh"),
                "# ztheme-segment-v1: cpu\n",
            )
            .unwrap();
        });
        let resolved =
            resolve_custom_segments(&config(&["clock", "cpu"]), &["clock".to_owned()], &root)
                .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "clock");
    }

    #[test]
    fn crlf_line_endings_are_tolerated() {
        let (root, _guard) = write_theme_files(|home| {
            std::fs::write(
                segments_dir(home).join("clock.zsh"),
                "# ztheme-segment-v1: clock\r\nztheme_segment_clock() { :; }\r\n",
            )
            .unwrap();
        });
        assert!(resolve_custom_segments(&config(&["clock"]), &["clock".to_owned()], &root).is_ok());
    }
}
