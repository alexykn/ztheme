use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::timeout;

pub(crate) mod detect;

use crate::cache::CacheKey;

const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);
const OUTPUT_LIMIT: u64 = 4_097;
const MAX_FIELD_BYTES: usize = 4_096;

macro_rules! define_runtimes {
    ($($variant:ident = $id:literal => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(u8)]
        pub(crate) enum Runtime {
            $($variant = $id),+
        }

        impl Runtime {
            pub(crate) const ALL: [Self; [$(stringify!($variant)),+].len()] = [
                $(Self::$variant),+
            ];

            pub(crate) const fn id(self) -> u8 {
                self as u8
            }

            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }

            pub(crate) const fn from_id(value: u8) -> Option<Self> {
                match value {
                    $($id => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub(crate) fn from_name(value: &str) -> Option<Self> {
                match value {
                    $($name => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

define_runtimes! {
    Python = 0 => "python",
    Perl = 1 => "perl",
    Java = 2 => "java",
    Kotlin = 3 => "kotlin",
    Scala = 4 => "scala",
    Rust = 5 => "rust",
    Go = 6 => "go",
    Bun = 7 => "bun",
    Deno = 8 => "deno",
    Node = 9 => "node",
    Ruby = 10 => "ruby",
    Php = 11 => "php",
    Dotnet = 12 => "dotnet",
    C = 13 => "c",
    Cpp = 14 => "cpp",
    Swift = 15 => "swift",
    Lua = 16 => "lua",
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeValue {
    pub(crate) runtime: Runtime,
    pub(crate) version: String,
    pub(crate) label: Option<String>,
    pub(crate) environment: Option<String>,
}

pub(crate) async fn snapshot(project: detect::Project, active: Vec<Runtime>) -> Vec<RuntimeValue> {
    let mut tasks = JoinSet::new();

    for runtime in active {
        let cwd = project.cwd.clone();
        tasks.spawn(async move {
            let spec = spec(runtime);
            let output = capture(&cwd, &spec).await?;
            let version = (spec.parse_version)(&output)?;
            Some(RuntimeValue {
                runtime,
                version,
                label: spec.label.map(str::to_owned),
                environment: spec.environment.and_then(|environment| environment()),
            })
        });
    }

    let mut values = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(Some(value)) = result {
            values.push(value);
        }
    }
    values.sort_unstable_by_key(|value| value.runtime);
    values
}

pub(crate) fn cache_key(project: &detect::Project, active: &[Runtime]) -> CacheKey {
    let mut hash = project.hash.clone();
    hash.add_u64(b"runtime-snapshot-version", 1);

    for runtime in active {
        hash.add_u64(b"runtime-command", u64::from(runtime.id()));
        let spec = spec(*runtime);
        hash.add_os(b"runtime-program", &spec.program);
        hash.add_u64(
            b"runtime-argument-count",
            u64::try_from(spec.arguments.len()).unwrap_or(u64::MAX),
        );
        for argument in spec.arguments {
            hash.add_bytes(b"runtime-argument", argument.as_bytes());
        }
        hash.add_u64(b"runtime-merge-stderr", u64::from(spec.merge_stderr));
        let executable = resolve_program(&spec.program);
        if let Some(executable) = executable.as_deref() {
            hash.add_path(b"resolved-executable", executable);
            let link_metadata = executable.symlink_metadata().ok();
            hash.add_metadata(b"executable-link-metadata", link_metadata.as_ref());

            if let Ok(canonical) = executable.canonicalize() {
                hash.add_path(b"canonical-executable", &canonical);
                let metadata = canonical.metadata().ok();
                hash.add_metadata(b"executable-metadata", metadata.as_ref());
            } else {
                hash.add_bytes(b"canonical-executable", b"unavailable");
            }
        } else {
            hash.add_bytes(b"resolved-executable", b"missing");
        }
    }

    CacheKey::from_value(hash.finish())
}

pub(crate) fn encode(values: &[RuntimeValue]) -> io::Result<Vec<u8>> {
    let count = u8::try_from(values.len()).map_err(|_| invalid_data("too many runtime values"))?;
    let mut output = Vec::with_capacity(128);
    output.push(count);
    for value in values {
        output.push(value.runtime.id());
        write_text(&mut output, &value.version)?;
        write_optional_text(&mut output, value.label.as_deref())?;
        write_optional_text(&mut output, value.environment.as_deref())?;
    }
    Ok(output)
}

pub(crate) fn decode(bytes: &[u8]) -> io::Result<Vec<RuntimeValue>> {
    let mut decoder = Decoder::new(bytes);
    let count = usize::from(decoder.u8()?);
    if count > Runtime::ALL.len() {
        return Err(invalid_data("runtime cache contains too many values"));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let runtime = Runtime::from_id(decoder.u8()?)
            .ok_or_else(|| invalid_data("runtime cache contains an unknown runtime"))?;
        values.push(RuntimeValue {
            runtime,
            version: decoder.text()?,
            label: decoder.optional_text()?,
            environment: decoder.optional_text()?,
        });
    }
    if !decoder.is_empty() {
        return Err(invalid_data("runtime cache contains trailing data"));
    }
    values.sort_unstable_by_key(|value| value.runtime);
    if values
        .windows(2)
        .any(|pair| pair[0].runtime == pair[1].runtime)
    {
        return Err(invalid_data("runtime cache contains duplicate runtimes"));
    }
    Ok(values)
}

struct RuntimeSpec {
    program: OsString,
    arguments: &'static [&'static str],
    merge_stderr: bool,
    label: Option<&'static str>,
    parse_version: fn(&str) -> Option<String>,
    environment: Option<fn() -> Option<String>>,
}

fn spec(runtime: Runtime) -> RuntimeSpec {
    match runtime {
        Runtime::Python => RuntimeSpec {
            program: python_program(),
            arguments: &["--version"],
            merge_stderr: true,
            label: None,
            parse_version: parse_python,
            environment: Some(python_environment),
        },
        Runtime::Perl => RuntimeSpec {
            program: OsString::from("perl"),
            arguments: &["-e", "printf \"%vd\\n\", $^V"],
            merge_stderr: false,
            label: None,
            parse_version: parse_line,
            environment: Some(perl_environment),
        },
        Runtime::Java => static_spec("java", &["-version"], true, parse_java),
        Runtime::Kotlin => static_spec("kotlinc", &["-version"], true, parse_numeric),
        Runtime::Scala => static_spec("scala", &["-version"], true, parse_numeric),
        Runtime::Rust => RuntimeSpec {
            environment: Some(rust_environment),
            ..static_spec("rustc", &["--version"], false, parse_second_word)
        },
        Runtime::Go => static_spec("go", &["version"], false, parse_go),
        Runtime::Bun => static_spec("bun", &["--version"], false, parse_line),
        Runtime::Deno => static_spec("deno", &["--version"], false, parse_second_word),
        Runtime::Node => static_spec("node", &["--version"], false, parse_node),
        Runtime::Ruby => RuntimeSpec {
            environment: Some(ruby_environment),
            ..static_spec("ruby", &["--version"], false, parse_second_word)
        },
        Runtime::Php => static_spec("php", &["--version"], false, parse_second_word),
        Runtime::Dotnet => static_spec("dotnet", &["--version"], false, parse_line),
        Runtime::C => compiler_spec(false),
        Runtime::Cpp => compiler_spec(true),
        Runtime::Swift => static_spec("swift", &["--version"], false, parse_numeric),
        Runtime::Lua => static_spec("lua", &["-v"], true, parse_second_word),
    }
}

fn static_spec(
    program: &'static str,
    arguments: &'static [&'static str],
    merge_stderr: bool,
    parse_version: fn(&str) -> Option<String>,
) -> RuntimeSpec {
    RuntimeSpec {
        program: OsString::from(program),
        arguments,
        merge_stderr,
        label: None,
        parse_version,
        environment: None,
    }
}

fn compiler_spec(cpp: bool) -> RuntimeSpec {
    let path = env::var_os("PATH").unwrap_or_default();
    let clang = if cpp { "clang++" } else { "clang" };
    let gcc = if cpp { "g++" } else { "gcc" };

    if executable_on_path(clang, &path) {
        return RuntimeSpec {
            label: Some("clang"),
            ..static_spec(clang, &["--version"], false, parse_numeric)
        };
    }

    RuntimeSpec {
        label: Some("gcc"),
        ..static_spec(
            gcc,
            &["-dumpfullversion", "-dumpversion"],
            false,
            parse_numeric,
        )
    }
}

fn python_program() -> OsString {
    for variable in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        let Some(prefix) = env::var_os(variable) else {
            continue;
        };
        let candidate = std::path::PathBuf::from(prefix).join("bin/python");
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }

    let path = env::var_os("PATH").unwrap_or_default();
    if executable_on_path("python", &path) {
        OsString::from("python")
    } else {
        OsString::from("python3")
    }
}

fn executable_on_path(name: &str, path: &OsString) -> bool {
    env::split_paths(path).any(|directory| directory.join(name).is_file())
}

fn resolve_program(program: &OsString) -> Option<PathBuf> {
    let program = Path::new(program);
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }

    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

async fn capture(cwd: &Path, spec: &RuntimeSpec) -> Option<String> {
    let mut command = Command::new(&spec.program);
    command
        .args(spec.arguments)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if spec.merge_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);

    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take();
    let merge_stderr = spec.merge_stderr;

    let collected = timeout(COMMAND_TIMEOUT, async move {
        let stdout_task = async {
            let mut bytes = Vec::new();
            stdout
                .take(OUTPUT_LIMIT)
                .read_to_end(&mut bytes)
                .await
                .ok()?;
            Some(bytes)
        };
        let stderr_task = async {
            let mut bytes = Vec::new();
            if let Some(ref mut stderr) = stderr {
                stderr
                    .take(OUTPUT_LIMIT)
                    .read_to_end(&mut bytes)
                    .await
                    .ok()?;
            }
            Some(bytes)
        };
        let (stdout, stderr, status) = tokio::join!(stdout_task, stderr_task, child.wait());
        status.ok()?.success().then_some((stdout?, stderr?))
    })
    .await
    .ok()??;

    let mut bytes = collected.0;
    if merge_stderr {
        bytes.extend_from_slice(&collected.1);
    }
    if u64::try_from(bytes.len()).ok()? >= OUTPUT_LIMIT {
        return None;
    }

    String::from_utf8(bytes).ok()
}

fn first_line(output: &str) -> Option<&str> {
    output
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
}

fn parse_python(output: &str) -> Option<String> {
    first_line(output)?
        .strip_prefix("Python ")
        .map(str::to_owned)
}

fn parse_line(output: &str) -> Option<String> {
    let value = first_line(output)?;
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_java(output: &str) -> Option<String> {
    first_line(output)?.split('"').nth(1).map(str::to_owned)
}

fn parse_numeric(output: &str) -> Option<String> {
    numeric_word(first_line(output)?)
}

fn parse_second_word(output: &str) -> Option<String> {
    first_line(output)?
        .split_whitespace()
        .nth(1)
        .map(str::to_owned)
}

fn parse_go(output: &str) -> Option<String> {
    first_line(output)?
        .split_whitespace()
        .nth(2)?
        .strip_prefix("go")
        .map(str::to_owned)
}

fn parse_node(output: &str) -> Option<String> {
    let line = first_line(output)?;
    let value = line.strip_prefix('v').unwrap_or(line);
    (!value.is_empty()).then(|| value.to_owned())
}

fn numeric_word(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '.' && character != '-'
        });
        let starts_numeric = word.as_bytes().first().is_some_and(u8::is_ascii_digit);
        (starts_numeric && word.contains('.')).then(|| word.to_owned())
    })
}

fn python_environment() -> Option<String> {
    env::var_os("VIRTUAL_ENV")
        .and_then(|path| PathBuf::from(path).file_name().map(OsString::from))
        .or_else(|| env::var_os("CONDA_DEFAULT_ENV"))
        .or_else(|| {
            env::var_os("CONDA_PREFIX")
                .and_then(|path| PathBuf::from(path).file_name().map(OsString::from))
        })
        .and_then(|value| value.into_string().ok())
}

fn perl_environment() -> Option<String> {
    env::var_os("PERLBREW_PERL")
        .or_else(|| env::var_os("PLENV_VERSION"))
        .and_then(|value| value.into_string().ok())
}

fn rust_environment() -> Option<String> {
    env::var_os("RUSTUP_TOOLCHAIN").and_then(|value| value.into_string().ok())
}

fn ruby_environment() -> Option<String> {
    env::var_os("RBENV_VERSION")
        .or_else(|| env::var_os("RUBY_VERSION"))
        .and_then(|value| value.into_string().ok())
}

fn write_text(output: &mut Vec<u8>, value: &str) -> io::Result<()> {
    if value.len() > MAX_FIELD_BYTES {
        return Err(invalid_data("runtime cache field exceeds size limit"));
    }
    let length =
        u16::try_from(value.len()).map_err(|_| invalid_data("runtime cache field is too large"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_optional_text(output: &mut Vec<u8>, value: Option<&str>) -> io::Result<()> {
    if let Some(value) = value {
        output.push(1);
        return write_text(output, value);
    }
    output.push(0);
    Ok(())
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
            .ok_or_else(|| invalid_data("runtime cache length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid_data("runtime cache is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> io::Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| invalid_data("runtime cache is truncated"))
    }

    fn u16(&mut self) -> io::Result<u16> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| invalid_data("runtime cache is truncated"))?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn text(&mut self) -> io::Result<String> {
        let length = usize::from(self.u16()?);
        if length > MAX_FIELD_BYTES {
            return Err(invalid_data("runtime cache field exceeds size limit"));
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| invalid_data("runtime cache field is not UTF-8"))
    }

    fn optional_text(&mut self) -> io::Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.text().map(Some),
            _ => Err(invalid_data("runtime cache optional field is invalid")),
        }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Runtime, RuntimeValue, decode, encode, spec};

    fn parse_version(runtime: Runtime, output: &str) -> Option<String> {
        (spec(runtime).parse_version)(output)
    }

    #[test]
    fn runtime_identities_are_unique_and_round_trip() {
        let ids = Runtime::ALL
            .into_iter()
            .map(Runtime::id)
            .collect::<HashSet<_>>();
        let names = Runtime::ALL
            .into_iter()
            .map(Runtime::name)
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), Runtime::ALL.len());
        assert_eq!(names.len(), Runtime::ALL.len());
        for runtime in Runtime::ALL {
            assert_eq!(Runtime::from_id(runtime.id()), Some(runtime));
            assert_eq!(Runtime::from_name(runtime.name()), Some(runtime));
        }
    }

    #[test]
    fn parses_supported_runtime_versions() {
        let cases = [
            (Runtime::Python, "Python 3.14.6\n", "3.14.6"),
            (Runtime::Perl, "5.40.2\n", "5.40.2"),
            (
                Runtime::Java,
                "openjdk version \"25.0.1\" 2025-10-21\n",
                "25.0.1",
            ),
            (
                Runtime::Kotlin,
                "info: kotlinc-jvm 2.2.20 (JRE 25)\n",
                "2.2.20",
            ),
            (Runtime::Scala, "Scala code runner version 3.7.3\n", "3.7.3"),
            (
                Runtime::Rust,
                "rustc 1.97.1 (8bab26f4f 2026-07-14)\n",
                "1.97.1",
            ),
            (Runtime::Go, "go version go1.25.1 darwin/arm64\n", "1.25.1"),
            (Runtime::Bun, "1.2.20\n", "1.2.20"),
            (Runtime::Deno, "deno 2.4.5\nv8 13.7\n", "2.4.5"),
            (Runtime::Node, "v24.5.0\n", "24.5.0"),
            (Runtime::Ruby, "ruby 3.4.5 (2025-07-16 revision)\n", "3.4.5"),
            (Runtime::Php, "PHP 8.4.11 (cli)\n", "8.4.11"),
            (Runtime::Dotnet, "9.0.304\n", "9.0.304"),
            (Runtime::C, "Apple clang version 17.0.0\n", "17.0.0"),
            (Runtime::Cpp, "g++ (GCC) 15.2.1 20250730\n", "15.2.1"),
            (
                Runtime::Swift,
                "Apple Swift version 6.2 (swiftlang)\n",
                "6.2",
            ),
            (Runtime::Lua, "Lua 5.4.8  Copyright (C)\n", "5.4.8"),
        ];

        for (runtime, output, expected) in cases {
            assert_eq!(
                parse_version(runtime, output).as_deref(),
                Some(expected),
                "{}",
                runtime.name()
            );
        }
    }

    #[test]
    fn rejects_unparseable_runtime_versions() {
        assert_eq!(parse_version(Runtime::Python, "python unknown"), None);
        assert_eq!(parse_version(Runtime::Go, "go version unknown"), None);
        assert_eq!(
            parse_version(Runtime::Java, "openjdk version unknown"),
            None
        );
        assert_eq!(parse_version(Runtime::Rust, "\n\n"), None);
    }

    #[test]
    fn runtime_cache_round_trips_and_sorts_values() {
        let values = [
            RuntimeValue {
                runtime: Runtime::Rust,
                version: "1.97.1".to_owned(),
                label: None,
                environment: Some("nightly".to_owned()),
            },
            RuntimeValue {
                runtime: Runtime::Python,
                version: "3.14.6".to_owned(),
                label: Some("cpython".to_owned()),
                environment: Some(".venv".to_owned()),
            },
        ];

        let decoded = decode(&encode(&values).unwrap()).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].runtime, Runtime::Python);
        assert_eq!(decoded[0].version, "3.14.6");
        assert_eq!(decoded[0].label.as_deref(), Some("cpython"));
        assert_eq!(decoded[0].environment.as_deref(), Some(".venv"));
        assert_eq!(decoded[1].runtime, Runtime::Rust);
        assert_eq!(decoded[1].environment.as_deref(), Some("nightly"));
    }

    #[test]
    fn runtime_cache_rejects_malformed_records() {
        let duplicate = [
            2,
            Runtime::Rust.id(),
            0,
            1,
            b'1',
            0,
            0,
            Runtime::Rust.id(),
            0,
            1,
            b'2',
            0,
            0,
        ];
        assert!(decode(&duplicate).is_err());

        assert!(decode(&[1, u8::MAX]).is_err());
        assert!(decode(&[1, Runtime::Rust.id(), 0, 0, 2]).is_err());

        let valid = encode(&[RuntimeValue {
            runtime: Runtime::Node,
            version: "24.5.0".to_owned(),
            label: None,
            environment: None,
        }])
        .unwrap();
        for length in 0..valid.len() {
            assert!(
                decode(&valid[..length]).is_err(),
                "accepted length {length}"
            );
        }
        let mut trailing = valid;
        trailing.push(0);
        assert!(decode(&trailing).is_err());
    }
}
