use std::ffi::{OsStr, OsString};

use tokio::process::Command;

/// The per-request prompt environment parsed from the shell's request.
///
/// The client never mutates its process environment; this value is the
/// single source of truth for one shell request. `REQUEST_FIELDS` owns the
/// wire order, decoding accessors, and volatile child environment policy.
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

/// One request variable: its wire identity, storage accessors, and whether
/// volatile runtime children may observe it. Git routing is request-only.
pub(crate) struct EnvironmentField {
    pub(crate) name: &'static str,
    get: fn(&PromptEnvironment) -> Option<&OsStr>,
    set: fn(&mut PromptEnvironment, Option<OsString>),
    runtime: bool,
}

impl EnvironmentField {
    pub(crate) fn set(&self, environment: &mut PromptEnvironment, value: Option<OsString>) {
        (self.set)(environment, value);
    }

    fn apply(&self, environment: &PromptEnvironment, command: &mut Command) {
        if self.runtime {
            apply(command, self.name, (self.get)(environment));
        }
    }
}

/// Version 3 wire order. Generate shell fields and decode requests from this
/// same owner; changing the order requires a request protocol version change.
pub(crate) const REQUEST_FIELDS: &[EnvironmentField] = &[
    EnvironmentField {
        name: "PATH",
        get: |environment| environment.path.as_deref(),
        set: |environment, value| environment.path = value,
        runtime: true,
    },
    EnvironmentField {
        name: "HOME",
        get: |environment| environment.home.as_deref(),
        set: |environment, value| environment.home = value,
        runtime: true,
    },
    EnvironmentField {
        name: "GIT_DIR",
        get: |environment| environment.git_dir.as_deref(),
        set: |environment, value| environment.git_dir = value,
        runtime: false,
    },
    EnvironmentField {
        name: "GIT_WORK_TREE",
        get: |environment| environment.git_work_tree.as_deref(),
        set: |environment, value| environment.git_work_tree = value,
        runtime: false,
    },
    EnvironmentField {
        name: "GIT_CEILING_DIRECTORIES",
        get: |environment| environment.git_ceilings.as_deref(),
        set: |environment, value| environment.git_ceilings = value,
        runtime: false,
    },
    EnvironmentField {
        name: "VIRTUAL_ENV",
        get: |environment| environment.virtual_env.as_deref(),
        set: |environment, value| environment.virtual_env = value,
        runtime: true,
    },
    EnvironmentField {
        name: "CONDA_PREFIX",
        get: |environment| environment.conda_prefix.as_deref(),
        set: |environment, value| environment.conda_prefix = value,
        runtime: true,
    },
    EnvironmentField {
        name: "CONDA_DEFAULT_ENV",
        get: |environment| environment.conda_default_env.as_deref(),
        set: |environment, value| environment.conda_default_env = value,
        runtime: true,
    },
    EnvironmentField {
        name: "PERLBREW_PERL",
        get: |environment| environment.perlbrew_perl.as_deref(),
        set: |environment, value| environment.perlbrew_perl = value,
        runtime: true,
    },
    EnvironmentField {
        name: "PLENV_VERSION",
        get: |environment| environment.plenv_version.as_deref(),
        set: |environment, value| environment.plenv_version = value,
        runtime: true,
    },
    EnvironmentField {
        name: "PYENV_VERSION",
        get: |environment| environment.pyenv_version.as_deref(),
        set: |environment, value| environment.pyenv_version = value,
        runtime: true,
    },
    EnvironmentField {
        name: "PYENV_DIR",
        get: |environment| environment.pyenv_dir.as_deref(),
        set: |environment, value| environment.pyenv_dir = value,
        runtime: true,
    },
    EnvironmentField {
        name: "RUSTUP_TOOLCHAIN",
        get: |environment| environment.rustup_toolchain.as_deref(),
        set: |environment, value| environment.rustup_toolchain = value,
        runtime: true,
    },
    EnvironmentField {
        name: "RUSTUP_HOME",
        get: |environment| environment.rustup_home.as_deref(),
        set: |environment, value| environment.rustup_home = value,
        runtime: true,
    },
    EnvironmentField {
        name: "RBENV_DIR",
        get: |environment| environment.rbenv_dir.as_deref(),
        set: |environment, value| environment.rbenv_dir = value,
        runtime: true,
    },
    EnvironmentField {
        name: "RBENV_VERSION",
        get: |environment| environment.rbenv_version.as_deref(),
        set: |environment, value| environment.rbenv_version = value,
        runtime: true,
    },
    EnvironmentField {
        name: "NODENV_VERSION",
        get: |environment| environment.nodenv_version.as_deref(),
        set: |environment, value| environment.nodenv_version = value,
        runtime: true,
    },
    EnvironmentField {
        name: "NODENV_DIR",
        get: |environment| environment.nodenv_dir.as_deref(),
        set: |environment, value| environment.nodenv_dir = value,
        runtime: true,
    },
    EnvironmentField {
        name: "PLENV_DIR",
        get: |environment| environment.plenv_dir.as_deref(),
        set: |environment, value| environment.plenv_dir = value,
        runtime: true,
    },
    EnvironmentField {
        name: "RUBY_VERSION",
        get: |environment| environment.ruby_version.as_deref(),
        set: |environment, value| environment.ruby_version = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JAVA_HOME",
        get: |environment| environment.java_home.as_deref(),
        set: |environment, value| environment.java_home = value,
        runtime: true,
    },
    EnvironmentField {
        name: "GOTOOLCHAIN",
        get: |environment| environment.gotoolchain.as_deref(),
        set: |environment, value| environment.gotoolchain = value,
        runtime: true,
    },
    EnvironmentField {
        name: "DOTNET_ROOT",
        get: |environment| environment.dotnet_root.as_deref(),
        set: |environment, value| environment.dotnet_root = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JULIAUP_CHANNEL",
        get: |environment| environment.juliaup_channel.as_deref(),
        set: |environment, value| environment.juliaup_channel = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JULIAUP_DEPOT_PATH",
        get: |environment| environment.juliaup_depot_path.as_deref(),
        set: |environment, value| environment.juliaup_depot_path = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JULIA_PROJECT",
        get: |environment| environment.julia_project.as_deref(),
        set: |environment, value| environment.julia_project = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JULIA_LOAD_PATH",
        get: |environment| environment.julia_load_path.as_deref(),
        set: |environment, value| environment.julia_load_path = value,
        runtime: true,
    },
    EnvironmentField {
        name: "JULIA_DEPOT_PATH",
        get: |environment| environment.julia_depot_path.as_deref(),
        set: |environment, value| environment.julia_depot_path = value,
        runtime: true,
    },
    EnvironmentField {
        name: "R_ARCH",
        get: |environment| environment.r_arch.as_deref(),
        set: |environment, value| environment.r_arch = value,
        runtime: true,
    },
];

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
        for field in REQUEST_FIELDS {
            field.apply(self, command);
        }
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
