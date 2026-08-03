use std::ffi::{OsStr, OsString};

use tokio::process::Command;

/// The per-request prompt environment parsed from the shell's request.
///
/// The client no longer mutates its process environment; this value is the
/// single source of truth for one shell request. The wire order is mirrored in
/// `prompt::client` and `shell/ztheme.zsh`.
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
    pub(crate) pyenv_version: Option<OsString>,
    pub(crate) pyenv_dir: Option<OsString>,
    pub(crate) rustup_toolchain: Option<OsString>,
    pub(crate) rustup_home: Option<OsString>,
    pub(crate) rbenv_dir: Option<OsString>,
    pub(crate) rbenv_version: Option<OsString>,
    pub(crate) nodenv_version: Option<OsString>,
    pub(crate) nodenv_dir: Option<OsString>,
    pub(crate) plenv_dir: Option<OsString>,
    pub(crate) ruby_version: Option<OsString>,
    pub(crate) java_home: Option<OsString>,
    pub(crate) gotoolchain: Option<OsString>,
    pub(crate) dotnet_root: Option<OsString>,
    pub(crate) juliaup_channel: Option<OsString>,
    pub(crate) juliaup_depot_path: Option<OsString>,
    pub(crate) julia_project: Option<OsString>,
    pub(crate) julia_load_path: Option<OsString>,
    pub(crate) julia_depot_path: Option<OsString>,
    pub(crate) r_arch: Option<OsString>,
}

impl PromptEnvironment {
    /// Starts every runtime command from a small deterministic baseline.
    pub(crate) fn prepare_command(command: &mut Command) {
        command
            .env_clear()
            .env("LC_ALL", "C")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("DOTNET_NOLOGO", "1")
            .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1");
    }

    /// Applies the runtime portion of the request environment to a volatile
    /// command. Git routing fields stay private to the Git query path.
    ///
    /// Volatile commands are deliberately allowed to observe the shell's
    /// selection machinery. They are never put in the semantic cache.
    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        Self::prepare_command(command);
        apply(command, "PATH", self.path.as_deref());
        apply(command, "HOME", self.home.as_deref());
        apply(command, "GIT_DIR", None);
        apply(command, "GIT_WORK_TREE", None);
        apply(command, "GIT_CEILING_DIRECTORIES", None);
        apply(command, "VIRTUAL_ENV", self.virtual_env.as_deref());
        apply(command, "CONDA_PREFIX", self.conda_prefix.as_deref());
        apply(
            command,
            "CONDA_DEFAULT_ENV",
            self.conda_default_env.as_deref(),
        );
        apply(command, "PERLBREW_PERL", self.perlbrew_perl.as_deref());
        apply(command, "PLENV_VERSION", self.plenv_version.as_deref());
        apply(command, "PYENV_VERSION", self.pyenv_version.as_deref());
        apply(command, "PYENV_DIR", self.pyenv_dir.as_deref());
        apply(
            command,
            "RUSTUP_TOOLCHAIN",
            self.rustup_toolchain.as_deref(),
        );
        apply(command, "RUSTUP_HOME", self.rustup_home.as_deref());
        apply(command, "RBENV_DIR", self.rbenv_dir.as_deref());
        apply(command, "RBENV_VERSION", self.rbenv_version.as_deref());
        apply(command, "NODENV_VERSION", self.nodenv_version.as_deref());
        apply(command, "NODENV_DIR", self.nodenv_dir.as_deref());
        apply(command, "PLENV_DIR", self.plenv_dir.as_deref());
        apply(command, "RUBY_VERSION", self.ruby_version.as_deref());
        apply(command, "JAVA_HOME", self.java_home.as_deref());
        apply(command, "GOTOOLCHAIN", self.gotoolchain.as_deref());
        apply(command, "DOTNET_ROOT", self.dotnet_root.as_deref());
        apply(command, "JULIAUP_CHANNEL", self.juliaup_channel.as_deref());
        apply(
            command,
            "JULIAUP_DEPOT_PATH",
            self.juliaup_depot_path.as_deref(),
        );
        apply(command, "JULIA_PROJECT", self.julia_project.as_deref());
        apply(command, "JULIA_LOAD_PATH", self.julia_load_path.as_deref());
        apply(
            command,
            "JULIA_DEPOT_PATH",
            self.julia_depot_path.as_deref(),
        );
        apply(command, "R_ARCH", self.r_arch.as_deref());
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
            git_dir: Some(OsString::from("/repo/.git")),
            git_work_tree: Some(OsString::from("/repo")),
            git_ceilings: Some(OsString::from("/repo")),
            virtual_env: Some(OsString::from("/venv-a")),
            rustup_toolchain: Some(OsString::from("nightly")),
            juliaup_channel: Some(OsString::from("release")),
            juliaup_depot_path: Some(OsString::from("/depot-a")),
            julia_project: Some(OsString::from("@project")),
            julia_load_path: Some(OsString::from(":")),
            julia_depot_path: Some(OsString::from("/depot-b")),
            r_arch: Some(OsString::from("/x86_64")),
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
        assert_eq!(
            get("JULIAUP_CHANNEL"),
            Some(Some(OsString::from("release")))
        );
        assert_eq!(
            get("JULIAUP_DEPOT_PATH"),
            Some(Some(OsString::from("/depot-a")))
        );
        assert_eq!(get("JULIA_PROJECT"), Some(Some(OsString::from("@project"))));
        assert_eq!(get("JULIA_LOAD_PATH"), Some(Some(OsString::from(":"))));
        assert_eq!(
            get("JULIA_DEPOT_PATH"),
            Some(Some(OsString::from("/depot-b")))
        );
        assert_eq!(get("R_ARCH"), Some(Some(OsString::from("/x86_64"))));
        // Clearing the command environment means unset controls do not appear
        // as inherited values or as a synthetic environment entry.
        for name in [
            "PATH",
            "HOME",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_CEILING_DIRECTORIES",
            "CONDA_PREFIX",
            "CONDA_DEFAULT_ENV",
            "PYENV_VERSION",
            "PYENV_DIR",
            "RUSTUP_HOME",
            "RBENV_DIR",
            "NODENV_VERSION",
            "NODENV_DIR",
            "PLENV_DIR",
            "PERLBREW_PERL",
            "PLENV_VERSION",
            "RBENV_VERSION",
            "RUBY_VERSION",
            "JAVA_HOME",
            "GOTOOLCHAIN",
            "DOTNET_ROOT",
        ] {
            assert!(get(name).is_none(), "{name} should be absent");
        }
        assert_eq!(get("LC_ALL"), Some(Some(OsString::from("C"))));
        assert_eq!(get("TERM"), Some(Some(OsString::from("dumb"))));
        assert_eq!(get("NO_COLOR"), Some(Some(OsString::from("1"))));
        assert_eq!(get("DOTNET_NOLOGO"), Some(Some(OsString::from("1"))));
        assert_eq!(
            get("DOTNET_CLI_TELEMETRY_OPTOUT"),
            Some(Some(OsString::from("1")))
        );
    }
}
