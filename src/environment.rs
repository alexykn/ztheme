use std::ffi::{OsStr, OsString};

use tokio::process::Command;

use crate::utils::HashBuilder;

/// The per-request prompt environment parsed from the shell's request.
///
/// The client no longer mutates its process environment; this value is the
/// single source of truth and is threaded explicitly through Git query
/// construction, runtime detection, cache fingerprints, and runtime child
/// commands.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptEnvironment {
    pub(crate) path: Option<OsString>,
    pub(crate) home: Option<OsString>,
    pub(crate) git_dir: Option<OsString>,
    pub(crate) git_work_tree: Option<OsString>,
    pub(crate) git_ceilings: Option<OsString>,
    pub(crate) virtual_env: Option<OsString>,
    pub(crate) conda_prefix: Option<OsString>,
    pub(crate) conda_default_env: Option<OsString>,
    pub(crate) perlbrew_perl: Option<OsString>,
    pub(crate) plenv_version: Option<OsString>,
    pub(crate) rustup_toolchain: Option<OsString>,
    pub(crate) rbenv_version: Option<OsString>,
    pub(crate) ruby_version: Option<OsString>,
}

impl PromptEnvironment {
    /// Hashes the runtime-relevant values into the project fingerprint so the
    /// cache key always matches the environment the runtime command runs
    /// under.
    pub(crate) fn add_runtime_fingerprint(&self, hash: &mut HashBuilder) {
        for (name, value) in [
            ("PATH", self.path.as_deref()),
            ("VIRTUAL_ENV", self.virtual_env.as_deref()),
            ("CONDA_PREFIX", self.conda_prefix.as_deref()),
            ("CONDA_DEFAULT_ENV", self.conda_default_env.as_deref()),
            ("PERLBREW_PERL", self.perlbrew_perl.as_deref()),
            ("PLENV_VERSION", self.plenv_version.as_deref()),
            ("RUSTUP_TOOLCHAIN", self.rustup_toolchain.as_deref()),
            ("RBENV_VERSION", self.rbenv_version.as_deref()),
            ("RUBY_VERSION", self.ruby_version.as_deref()),
        ] {
            hash.add_bytes(b"environment-name", name.as_bytes());
            hash.add_optional_os(b"environment-value", value);
        }
    }

    /// Applies the request values to a child command: set variables that are
    /// present, explicitly remove variables that are unset, so the child can
    /// never observe a stale inherited value. The child keeps the client's
    /// stable startup environment for everything else.
    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        apply(command, "PATH", self.path.as_deref());
        apply(command, "HOME", self.home.as_deref());
        apply(command, "GIT_DIR", self.git_dir.as_deref());
        apply(command, "GIT_WORK_TREE", self.git_work_tree.as_deref());
        apply(
            command,
            "GIT_CEILING_DIRECTORIES",
            self.git_ceilings.as_deref(),
        );
        apply(command, "VIRTUAL_ENV", self.virtual_env.as_deref());
        apply(command, "CONDA_PREFIX", self.conda_prefix.as_deref());
        apply(
            command,
            "CONDA_DEFAULT_ENV",
            self.conda_default_env.as_deref(),
        );
        apply(command, "PERLBREW_PERL", self.perlbrew_perl.as_deref());
        apply(command, "PLENV_VERSION", self.plenv_version.as_deref());
        apply(
            command,
            "RUSTUP_TOOLCHAIN",
            self.rustup_toolchain.as_deref(),
        );
        apply(command, "RBENV_VERSION", self.rbenv_version.as_deref());
        apply(command, "RUBY_VERSION", self.ruby_version.as_deref());
    }
}

fn apply(command: &mut Command, name: &str, value: Option<&OsStr>) {
    match value {
        Some(value) => {
            command.env(name, value);
        }
        None => {
            command.env_remove(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::PromptEnvironment;

    #[test]
    fn apply_to_command_sets_and_removes_all_controls() {
        let environment = PromptEnvironment {
            virtual_env: Some(OsString::from("/venv-a")),
            rustup_toolchain: Some(OsString::from("nightly")),
            ..PromptEnvironment::default()
        };

        let mut command = tokio::process::Command::new("true");
        environment.apply_to_command(&mut command);
        let envs: Vec<(OsString, Option<OsString>)> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsString::from)))
            .collect();

        let get = |name: &str| {
            envs.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get("VIRTUAL_ENV"), Some(Some(OsString::from("/venv-a"))));
        assert_eq!(
            get("RUSTUP_TOOLCHAIN"),
            Some(Some(OsString::from("nightly")))
        );
        // Explicit removals for every unset prompt-controlled variable.
        assert_eq!(get("PATH"), Some(None));
        assert_eq!(get("HOME"), Some(None));
        assert_eq!(get("GIT_DIR"), Some(None));
        assert_eq!(get("GIT_WORK_TREE"), Some(None));
        assert_eq!(get("GIT_CEILING_DIRECTORIES"), Some(None));
        assert_eq!(get("CONDA_PREFIX"), Some(None));
        assert_eq!(get("CONDA_DEFAULT_ENV"), Some(None));
        assert_eq!(get("PERLBREW_PERL"), Some(None));
        assert_eq!(get("PLENV_VERSION"), Some(None));
        assert_eq!(get("RBENV_VERSION"), Some(None));
        assert_eq!(get("RUBY_VERSION"), Some(None));
    }
}
