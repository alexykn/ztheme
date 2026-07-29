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

use crate::cache::CacheKey;
use crate::projects::{Project, Runtime};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);
const OUTPUT_LIMIT: u64 = 4_097;
const MAX_FIELD_BYTES: usize = 4_096;

#[derive(Clone, Debug)]
pub struct RuntimeValue {
    pub runtime: Runtime,
    pub version: String,
    pub label: Option<String>,
    pub environment: Option<String>,
}

pub async fn snapshot(project: Project, active: Vec<Runtime>) -> Vec<RuntimeValue> {
    let mut tasks = JoinSet::new();

    for runtime in active {
        let cwd = project.cwd.clone();
        tasks.spawn(async move {
            let spec = command(runtime);
            let output = capture(&cwd, &spec).await?;
            let version = parse_version(runtime, &output)?;
            Some(RuntimeValue {
                runtime,
                version,
                label: spec.compiler.map(str::to_owned),
                environment: environment_name(runtime),
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

pub fn cache_key(project: &Project, active: &[Runtime]) -> CacheKey {
    let mut fingerprint = project.fingerprint.clone();
    fingerprint.add_u64(b"runtime-snapshot-version", 1);

    for runtime in active {
        fingerprint.add_u64(b"runtime-command", u64::from(runtime.id()));
        let spec = command(*runtime);
        fingerprint.add_os(b"runtime-program", &spec.program);
        let executable = resolve_program(&spec.program);
        if let Some(executable) = executable.as_deref() {
            fingerprint.add_path(b"resolved-executable", executable);
            let link_metadata = executable.symlink_metadata().ok();
            fingerprint.add_metadata(b"executable-link-metadata", link_metadata.as_ref());

            if let Ok(canonical) = executable.canonicalize() {
                fingerprint.add_path(b"canonical-executable", &canonical);
                let metadata = canonical.metadata().ok();
                fingerprint.add_metadata(b"executable-metadata", metadata.as_ref());
            } else {
                fingerprint.add_bytes(b"canonical-executable", b"unavailable");
            }
        } else {
            fingerprint.add_bytes(b"resolved-executable", b"missing");
        }
    }

    fingerprint.finish()
}

pub fn encode(values: &[RuntimeValue]) -> io::Result<Vec<u8>> {
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

pub fn decode(bytes: &[u8]) -> io::Result<Vec<RuntimeValue>> {
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

struct RuntimeCommand {
    program: OsString,
    arguments: &'static [&'static str],
    merge_stderr: bool,
    compiler: Option<&'static str>,
}

fn command(runtime: Runtime) -> RuntimeCommand {
    let (program, arguments, merge_stderr, compiler) = match runtime {
        Runtime::Python => (python_program(), &["--version"][..], true, None),
        Runtime::Perl => (
            OsString::from("perl"),
            &["-e", "printf \"%vd\\n\", $^V"][..],
            false,
            None,
        ),
        Runtime::Java => (OsString::from("java"), &["-version"][..], true, None),
        Runtime::Kotlin => (OsString::from("kotlinc"), &["-version"][..], true, None),
        Runtime::Scala => (OsString::from("scala"), &["-version"][..], true, None),
        Runtime::Rust => (OsString::from("rustc"), &["--version"][..], false, None),
        Runtime::Go => (OsString::from("go"), &["version"][..], false, None),
        Runtime::Bun => (OsString::from("bun"), &["--version"][..], false, None),
        Runtime::Deno => (OsString::from("deno"), &["--version"][..], false, None),
        Runtime::Node => (OsString::from("node"), &["--version"][..], false, None),
        Runtime::Ruby => (OsString::from("ruby"), &["--version"][..], false, None),
        Runtime::Php => (OsString::from("php"), &["--version"][..], false, None),
        Runtime::Dotnet => (OsString::from("dotnet"), &["--version"][..], false, None),
        Runtime::C => compiler_command(false),
        Runtime::Cpp => compiler_command(true),
        Runtime::Swift => (OsString::from("swift"), &["--version"][..], false, None),
        Runtime::Lua => (OsString::from("lua"), &["-v"][..], true, None),
    };

    RuntimeCommand {
        program,
        arguments,
        merge_stderr,
        compiler,
    }
}

fn compiler_command(
    cpp: bool,
) -> (
    OsString,
    &'static [&'static str],
    bool,
    Option<&'static str>,
) {
    let path = env::var_os("PATH").unwrap_or_default();
    let clang = if cpp { "clang++" } else { "clang" };
    let gcc = if cpp { "g++" } else { "gcc" };

    if executable_on_path(clang, &path) {
        return (OsString::from(clang), &["--version"], false, Some("clang"));
    }

    (
        OsString::from(gcc),
        &["-dumpfullversion", "-dumpversion"],
        false,
        Some("gcc"),
    )
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

async fn capture(cwd: &Path, spec: &RuntimeCommand) -> Option<String> {
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

fn parse_version(runtime: Runtime, output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?.trim();
    let version = match runtime {
        Runtime::Python => line.strip_prefix("Python ")?.to_owned(),
        Runtime::Perl | Runtime::Bun | Runtime::Dotnet => line.to_owned(),
        Runtime::Java => line.split('"').nth(1)?.to_owned(),
        Runtime::Kotlin | Runtime::Scala | Runtime::C | Runtime::Cpp | Runtime::Swift => {
            numeric_word(line)?
        }
        Runtime::Rust | Runtime::Ruby | Runtime::Php | Runtime::Lua => {
            line.split_whitespace().nth(1)?.to_owned()
        }
        Runtime::Go => line
            .split_whitespace()
            .nth(2)?
            .strip_prefix("go")?
            .to_owned(),
        Runtime::Deno => line.split_whitespace().nth(1)?.to_owned(),
        Runtime::Node => line.strip_prefix('v').unwrap_or(line).to_owned(),
    };

    (!version.is_empty()).then_some(version)
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

fn environment_name(runtime: Runtime) -> Option<String> {
    let value = match runtime {
        Runtime::Python => env::var_os("VIRTUAL_ENV")
            .and_then(|path| {
                std::path::PathBuf::from(path)
                    .file_name()
                    .map(OsString::from)
            })
            .or_else(|| env::var_os("CONDA_DEFAULT_ENV"))
            .or_else(|| {
                env::var_os("CONDA_PREFIX").and_then(|path| {
                    std::path::PathBuf::from(path)
                        .file_name()
                        .map(OsString::from)
                })
            }),
        Runtime::Perl => env::var_os("PERLBREW_PERL").or_else(|| env::var_os("PLENV_VERSION")),
        Runtime::Rust => env::var_os("RUSTUP_TOOLCHAIN"),
        Runtime::Ruby => env::var_os("RBENV_VERSION").or_else(|| env::var_os("RUBY_VERSION")),
        _ => None,
    }?;
    value.into_string().ok()
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
