# Architecture

ztheme has three process layers:

1. Short-lived CLI processes.
2. A per-shell client daemon that owns the prompt protocol and rendering.
3. A per-user background server daemon hosting shared cache and Git state.

This division exists because prompt rendering has conflicting requirements.
The shell-facing path must start quickly, finish within a strict deadline, and
never leave prompt work attached to the shell. Runtime discovery and Git
inspection, however, benefit from state that survives a single prompt render
and can be shared by multiple shells.

Zsh computes the immediate directory, character, and status segments itself.
For asynchronous segments it sends a request to its per-shell client daemon
before rendering the next prompt. The client starts the Git request first,
starts runtime detection immediately after that, renders the completed
fragments with the compiled theme, and finishes each request when the shared
deadline expires or the last fragment is written; the client itself keeps
running and serves the shell's next request. The client does not keep
per-request state between prompts other than its compiled theme and its
connection to the server daemon.

The shell stores the incoming fragments but does not redraw the prompt for each
one. By default it waits for the locked Git group's `complete` record, then
renders the complete prompt once. While a locked group is pending, Zsh leaves
the prompt empty; it only appears once every locked async segment has completed
or the deadline has expired. This makes the update atomic without adding a
second animation protocol.

Each asynchronous group (`git` and `runtime`) can be toggled through
`[async.lock]`. A lock tells the shell to wait for that group before rendering;
an unlock tells it not to, so the prompt renders as soon as every still-locked
group has delivered its `complete` record (immediately when none are locked)
and redraws each time an unlocked group's records arrive. By default the Git
group is locked (it stays fast via `gitstatusd`) and the runtime group is
unlocked, because a slow runtime version command should not delay every prompt.

The server daemon provides shared background state. It hosts two independent
long-lived capabilities:

- a persistent runtime-value cache, so runtime version commands are not
  executed for every prompt;
- a `gitstatusd` client started lazily on the first Git request, so Git status
  queries reuse one optimized repository-status process instead of starting
  Git tooling for every prompt.

The server daemon is not responsible for rendering. It returns structured Git
data and opaque encoded runtime snapshots; the client daemon renders those
values with the compiled theme.

Keeping a separate client daemon per shell removes the cost of spawning a
process for every prompt: on this project's benchmarks, the per-prompt spawn
was the largest single cost of a warm prompt, and a long-lived client turns it
into a one-time cost per shell. The client also isolates per-shell state
(generation counters, the active request, cancellation) and keeps slow runtime
command execution out of the shared daemon, where it would couple shells
through one event loop.

## Segments

Synchronous prompt segments (directory, clock, status, character, and user-defined
segments) are Zsh functions invoked once per prompt by a generic dispatcher.
Custom segments are opt-in per file, validated at init/reload time, and
embedded or sourced before the shell starts. Git and runtime segments remain
client-computed and asynchronous. See [Segments](segments.md) for the
complete contract, allowlist, header format, and examples.

## End-to-end prompt flow

```text
Zsh precmd/chpwd
├── send a request to the per-shell client daemon
│   └── ztheme __client-daemon (one per shell, spawned once at shell init)
│       ├── start Git task first
│       │   └── daemon::git_status
│       │       └── persistent gitstatus::Client
│       │           └── gitstatusd
│       └── start runtime task immediately after Git begins
│           ├── fresh project detection and PATH planning
│           ├── partition cacheable and volatile selections
│           ├── execute volatile selections on every request
│           └── acquire/execute/owned-put cacheable selections
├── compute synchronous shell segments while the client works
└── leave the prompt empty until the snapshot finishes

prompt protocol records
├── Zsh stores current-generation fragments without redrawing
└── each group's `complete` marker releases that group's rendering lock
    (an unlocked group redraws the prompt when its records arrive)

shared 550 ms deadline
├── cancel unfinished client tasks
└── write final done record

done record / all locked groups complete
└── Zsh renders the collected segments and redraws once
```

Git and runtime records may still arrive in either order on the prompt
protocol, but neither one is rendered by itself. Generation IDs prevent a slow
request for an old working directory from overwriting a newer prompt.

## Why a daemon

The server daemon is an optimization boundary and an ownership boundary, not a
second application tier.

Without it, every prompt request would have to start its own `gitstatusd`,
load and rewrite its own runtime cache, and coordinate concurrent shell
processes directly. That would put setup cost and filesystem races on the
latency-sensitive prompt path. A single per-user server daemon instead
provides:

- one owner for the `gitstatusd` subprocess;
- one in-memory runtime-cache state shared by all shells;
- serialized persistence and atomic cache replacement;
- a single place for daemon compatibility, startup, retry, and shutdown;
- isolation between production and explicitly named development instances.

The per-shell client daemon exists for a different reason: to keep the
short-lived process out of the prompt path. Spawning one process per prompt
was the largest single cost of a warm prompt; the client turns it into a
one-time cost per shell. It also keeps per-shell prompt state (generation,
active request, cancellation) in a process that owns it, and keeps runtime
command execution out of the shared daemon so one shell's cache miss cannot
slow another shell's prompt through a shared event loop.

The server daemon starts lazily on the first ordinary request and exits after
one hour without a newly accepted connection. It is therefore not a
permanently installed system service and requires no service manager. The
same `ztheme` binary launches its hidden `__daemon` command with standard
streams detached. The client daemon is spawned by the shell integration once
per shell; it exits when the shell's request pipe closes, which happens even
when the shell is killed without running cleanup hooks. The shell and the
client communicate over a pair of named pipes that are unlinked as soon as
both sides have opened them, so a killed shell leaves no stale filesystem
entries behind. Every prompt descriptor is opened close-on-exec
(`sysopen -o cloexec`) so it cannot leak into external commands or into the
client itself, and the client additionally checks every second that its
parent is still the shell that spawned it — an independent fallback if EOF
propagation is ever masked by descriptor leakage, transport changes, or an
unexpected wrapper process.

## Source layout

```text
src/
├── main.rs
├── cli.rs
├── utils.rs
├── daemon/
│   ├── mod.rs
│   └── protocol.rs
├── cache/
│   ├── mod.rs
│   └── disk.rs
├── runtime/
│   ├── mod.rs
│   ├── detect.rs
│   └── cache.rs
├── gitstatus/
├── prompt/
│   ├── mod.rs
│   ├── client.rs
│   └── protocol.rs
├── setup/
└── theme/
    ├── mod.rs
    ├── schema.rs
    ├── validate.rs
    ├── zsh.rs
    ├── async_theme.rs
    ├── manage.rs
    └── segments.rs
```

The larger `mod.rs` files intentionally keep each subsystem's primary
execution path local. New files are reserved for independent algorithms or
compatibility boundaries such as wire and disk formats. `theme/segments.rs`
holds the custom-segment id grammar, allowlist validation, and file/header
resolution used only at init and reload time.

## Subsystem ownership

### CLI

`cli.rs` owns Clap parsing, semantic argument validation, dispatch, exit
classes, stdout handling, broken-pipe handling, and construction of the
current-thread Tokio runtime used by asynchronous commands.

Hidden commands are shell implementation details and do not appear in public
help:

- `__daemon`
- `__client-daemon`
- `__theme-apply-zsh`
- `__theme-reload-zsh`

### Daemon

`daemon/mod.rs` owns the background process boundary:

- production and named development instances;
- socket and lock paths;
- safe socket-directory validation;
- process spawning and startup retries;
- protocol-version negotiation and old-daemon replacement;
- client operations and server request routing;
- daemon idle shutdown;
- the hosted `RuntimeCache`;
- the hosted `gitstatus::Client`.

The crate-private daemon client operations are deliberately explicit:

- `runtime_cache_acquire`
- `runtime_cache_put_owned`
- `runtime_cache_release`
- `runtime_cache_remove`
- `git_status`
- `reset`

Ordinary daemon-backed operations start or replace the daemon within a bounded
retry policy. `reset` does not start a missing daemon merely to clear data.

#### Local transport and instance isolation

Clients communicate with the daemon through a Unix domain stream socket. A
Unix socket fits this process boundary because all participants run for the
same local user:

- it does not expose a TCP port or network listener;
- the socket has a stable filesystem address that short-lived processes can
  discover without a registry or inherited file descriptor;
- normal Unix ownership and permission checks protect the endpoint;
- it carries arbitrary bytes, including non-UTF-8 repository paths;
- Tokio supports it directly with asynchronous `UnixStream` and
  `UnixListener`.

The socket directory is `/tmp/ztheme-<uid>`. The daemon creates it with mode
`0700` and refuses to use an existing path unless it is a directory owned by
the current user with no group or other permissions. The socket and lock file
use mode `0600`.

Production uses `daemon.sock`. A development instance hashes its validated
`--dev NAME` into a distinct socket name. The same stable hash selects
`runtime-v2-dev-<hash>.bin`; production continues to use `runtime-v2.bin`.
The name itself is not placed in either filesystem path. This allows a
development build and the installed production binary to run simultaneously
without sharing daemon or persistent-cache state.

The transport is intentionally Unix-specific. ztheme targets Zsh because that
is what I use. Similarly it uses Unix process and filesystem semantics because 
I don't care about Windows.

#### Single ownership and lifecycle

Before binding the socket, a daemon acquires a non-blocking exclusive `flock`
on the instance's lock file. If another process owns the lock, the redundant
daemon exits successfully. This handles several clients racing to lazily start
the same instance without requiring coordination in the callers.

The lock owner removes a stale socket path, binds a new listener, creates the
runtime cache, loads its persistent entries before accepting requests, and then
starts the flush loop. `gitstatusd` is started lazily on the first Git request
rather than during startup, so a daemon that only serves runtime-cache
operations never requires the managed binary. Each accepted connection is
handled in its own Tokio task. Runtime-cache requests can proceed concurrently;
Git queries are serialized by the mutex around the single stateful
`gitstatusd` client.

The daemon stops when:

- no connection is accepted for one hour;
- a newer client reports a higher daemon-protocol version.

Orderly shutdown aborts outstanding client and background tasks, waits for
client tasks to terminate, flushes the newest cache revision, drops
`gitstatusd`, releases the lock, and removes the socket through a guard.
Unexpected process termination can leave the socket path behind; the next lock
owner removes that stale path before binding.

#### Client startup and replacement

Callers use the acquire/owned-put/release/remove cache operations and
`git_status`; none of
them manually starts the daemon. The common request path:

1. connects and attempts the operation;
2. returns ordinary application or protocol errors immediately;
3. spawns `ztheme __daemon` when the socket is missing or refuses connections;
4. retries startup up to 10 times with 20 ms delays;
5. when the daemon reports itself outdated, waits for its socket transition,
   starts the replacement once the old socket disappears, and retries within
   the bounded replacement window.

An older client does not kill a newer daemon. It receives an unsupported
client error instead. This asymmetry lets a newly installed binary replace an
old background process while preventing an older still-running shell client
from downgrading the daemon.

`reset` is deliberately different. When the daemon exists it sends `RESET`,
which clears runtime cache state and restarts `gitstatusd` only when one is
already running. When no daemon is available it deletes persistent
runtime-cache files directly and does not start a process merely to clear
state.

### Cache

`cache/mod.rs` owns runtime-cache policy and lifecycle, while `cache/disk.rs`
owns the versioned persistent representation and its safe replacement. The
cache is hosted by the server daemon but has no socket, process-spawning,
Git-status, or daemon dependency.

Runtime selection is fresh for every prompt. A selected runtime is either
represented by a semantic SHA-256 cache identity or classified as volatile
and executed without entering the cache. The cache keeps at most 500 entries,
uses daemon-side singleflight for cold misses, and persists changes
asynchronously without a wall-clock expiry.

The complete key model, selection boundary, LRU and persistence behavior,
failure handling, and known limitations are documented in
[Runtime cache](cache.md).

### Runtime

`runtime/mod.rs` owns:

- stable runtime IDs and canonical names;
- runtime snapshot values and serialization;
- runtime command specifications and execution;
- version parsing;
- compiler and environment labels;
- snapshot execution;

`runtime/cache.rs` owns fresh selection planning, request-CWD-aware PATH
resolution, direct executable identity, the bounded pyenv/rbenv/nodenv/plenv
and rustup resolvers, the explicit `GOTOOLCHAIN=local` rule, and the SHA-256
semantic key. Python virtual/Conda selection is checked before PATH. Java and
.NET use the first executable selected from request PATH; .NET is currently
volatile because ztheme does not simulate SDK selection. Scripts, arbitrary
dispatchers, ambiguous selectors, and unsupported automatic Go toolchain
selection remain volatile rather than being simulated. The exact cacheability
boundary is documented in [Runtime cache](cache.md).

Runtime identity is declared once by `define_runtimes!`. IDs are explicit
because they are persisted and must not change when declaration order changes.
Each runtime has one exhaustive `RuntimeSpec` selection that groups its
program, arguments, output handling, version parser, label, and optional
environment resolver.

`runtime/detect.rs` owns directory traversal and project detection. Detection
remains explicit where runtimes differ, including JavaScript runtime
precedence, C versus C++, nested Scala markers, Git roots, home directories,
and Git ceiling directories.

### Git status

`gitstatus` owns installation and the `gitstatusd` process implementation.
The daemon owns one client instance, but the process protocol and query model
remain inside `gitstatus`.

ztheme uses
[`gitstatusd`](https://github.com/romkatv/gitstatus) because prompt Git status
needs repository metadata and working-tree change summaries on a path where
repeated `git status` process startup and repository scanning would be visible.
`gitstatusd` is designed to remain alive, reuse repository state, and answer
successive queries efficiently. That persistent design is the main reason Git
access belongs behind the server daemon rather than in each prompt request.

`ztheme setup` installs the managed binary. When the selected layout includes
Git, `ztheme init zsh` refuses to generate an integration while the binary is
absent, so the daemon never silently substitutes a different Git
implementation with different behavior; runtime-only layouts initialize
without it.

On the first Git request, the daemon lazily starts `gitstatus::Client`, which
launches the managed binary with piped stdin and stdout, protocol
compatibility restricted to `v1.5.*`, and its parent PID. The child is killed
when its owning Rust process is dropped.
Repository-related environment variables are removed from the child; each
request carries the intended directory or explicit `GIT_DIR`, rather than
letting the daemon's own launch environment accidentally select a repository.

The native `gitstatusd` protocol uses request IDs, unit-separator-delimited
fields, and a record-separator terminator. ztheme validates matching IDs,
repository flags, numeric fields, response termination, and a 64 KiB response
limit before translating the result into its smaller `gitstatus::Snapshot`:

- worktree, commit ID, branch, and repository action;
- ahead, behind, and stash counts;
- conflicted, deleted, staged, unstaged, and untracked change bits.

That internal process protocol is implemented only in
`gitstatus/process.rs`. It is not exposed directly to CLI processes and is not
the same as either ztheme protocol.

The client retries a failed query once after replacing `gitstatusd`. The daemon
also places a 30-second safety timeout around the process query and restarts
the child if that limit is exceeded. The prompt request's 550 ms deadline is
much shorter: it can stop waiting and keep the prompt responsive without
discarding the daemon or its Git process merely because one prompt no longer
needs the result.

### Prompt

`prompt/mod.rs` is the application-level coordinator. Its `snapshot` function
is the per-request engine used by the client daemon:

- starts the Git task first and lets it begin its daemon request;
- starts runtime detection immediately afterward;
- applies one shared 550 ms deadline;
- writes completed segments to the prompt stream, followed by that group's
  `complete` marker so the shell can release its rendering lock;
- cancels unfinished work;
- always writes the final `done` record;
- sanitizes errors before sending them to Zsh.

`prompt/client.rs` owns the per-shell client daemon: it parses requests from
the shell, dispatches each request to `snapshot`, and aborts the in-flight
request when a newer generation arrives. The client's records go to its
stdout, which the shell integration connects to its response pipe. It
verifies at startup that its parent is the shell it was spawned for, and
re-checks once per second while idle or serving, so it cannot outlive a
killed shell even if EOF propagation is masked.

The client never mutates its process environment. Each request carries the
prompt-controlled variables (`src/environment.rs`), and that value is threaded
explicitly through Git query construction, fresh runtime detection, selection
planning, and the environment of every runtime child command. Git routing
fields are used only to build the Git query; they are removed from runtime
children. Every runtime child starts from a cleared environment with fixed
`LC_ALL=C`, `TERM=dumb`, `NO_COLOR=1`, and .NET output controls. Cacheable
children receive only their declared runtime inputs; volatile children receive
the current known request environment, including PATH, HOME, and runtime
selection variables, set where present and absent otherwise. The shared server
daemon therefore always inherits the client's stable startup environment rather
than one arbitrary prompt request's transient values.

It does not manage server sockets, process startup, cache persistence, or the
`gitstatusd` process.

The 550 ms limit is one deadline shared by both jobs, not 550 ms per operation.
Once it expires, the engine aborts unfinished Tokio tasks and writes `done`.
Runtime discovery runs on Tokio's blocking pool, so a slow filesystem walk
cannot stall the client event loop or the deadline; the walk may outlive the
expired request, but its result is simply discarded. The shell treats `done`
as the rendering barrier, so a slow or missing result cannot leave the prompt
waiting forever. Dropping an in-flight server request closes that request's
socket connection; the server daemon remains independent and available to
later prompts.

Runtime-cache failures are soft on the rendering path. A failed read falls
back to executing the runtime snapshot, and a failed write does not discard
the freshly calculated result. Git errors and task failures are written as
sanitized protocol records so they can be diagnosed without injecting control
characters or record delimiters into the shell stream.

### Theme and setup

`theme` owns theme loading, validation, compilation, rendering, management,
and Zsh generation. Runtime segment configuration remains a flattened map
keyed by canonical runtime name.

`setup` owns installation of managed shell integrations. Installation logic
does not live in CLI dispatch.

`utils.rs` contains only the deterministic `HashBuilder` used for daemon
instance naming. Runtime cache keys use their own SHA-256 field builder.

## Dependency direction

```text
main
└── cli

cli
├── daemon
├── prompt
├── setup
└── theme

prompt
├── daemon
├── gitstatus
├── runtime
└── theme

daemon
├── cache
├── gitstatus
└── utils

runtime
├── cache
└── utils

cache
└── utils

theme
└── runtime
```

In particular:

- `cache` does not depend on `daemon` or `gitstatus`;
- `gitstatus` does not depend on `daemon`;
- `daemon` hosts cache and Git-status services;
- `runtime` may construct `CacheKey`, while cache remains unaware of runtime
  values;
- prompt orchestration may depend on all user-visible data providers.

## Protocol boundaries

ztheme has three unrelated protocols.

### Daemon binary protocol

`daemon/protocol.rs` defines the versioned binary protocol used between
ztheme processes and the server daemon. Each operation opens one Unix
stream connection, writes one request, reads one response, and drops the
connection. There is no multiplexing or connection pool: state is held by the
daemon services, while per-request connections keep client cancellation and
failure isolation straightforward.

Every request starts with:

```text
2 bytes    magic: "ZT"
u16        protocol version, big-endian
u8         operation
...        operation-specific payload
```

Protocol version 2 defines these operation bytes:

| Byte | Operation | Request payload | Response |
| ---: | --- | --- | --- |
| `1` | runtime cache acquire | 32-byte key | hit plus value, or owner token |
| `2` | runtime cache put owned | 32-byte key, token, value | OK or rejected |
| `3` | runtime cache release | 32-byte key plus token | OK or rejected |
| `4` | reset | none | OK |
| `5` | Git status | query kind plus length-prefixed path bytes | none, snapshot, or error |
| `6` | runtime cache remove | 32-byte key | OK |

Integers use Tokio's big-endian wire methods. Variable data is prefixed with a
`u32` length and validated before allocation or use. Runtime values are limited
to 16 KiB; paths and Git text fields to 16 KiB; daemon error text to 1 KiB.
Paths remain bytes across the protocol so valid non-UTF-8 Unix paths round-trip
without lossy conversion. Human-readable Git fields must be UTF-8.

The normal mutation/reset exchange deadline is 25 ms. Cache acquire uses
500 ms because it may wait for the daemon-side 400 ms singleflight lease. Git
uses 500 ms because it may require repository work, while still fitting
beneath the request's 550 ms overall deadline in the usual case.

Compatibility is negotiated before reading an operation:

- matching versions continue with the operation byte;
- a newer client is told that the daemon is outdated (`0xfe`), and the daemon
  begins shutdown so the client can replace it;
- an older client is told that the client is outdated (`0xff`) and must not
  replace the newer daemon.

The version remains unchanged when symbolic names or Rust ownership change but
the bytes do not. It changes only for an incompatible wire-format or semantic
change. The daemon-protocol version is intentionally unrelated to the cache
disk-format and cache-identity versions.

### Prompt-to-Zsh protocol

`prompt/protocol.rs` defines the line-oriented records written by the client
daemon to stdout and consumed by the Zsh integration:

```text
ZTHEME1<TAB>generation<TAB>segment<TAB>name<TAB>rendered-fragment<NL>
ZTHEME1<TAB>generation<TAB>error<TAB>source<TAB>message<NL>
ZTHEME1<TAB>generation<TAB>complete<TAB>group<NL>
ZTHEME1<TAB>generation<TAB>done<NL>
```

Each record is flushed immediately. A segment record is already rendered
prompt text, while daemon messages carry structured data. Percent characters
and control characters are escaped before dynamic text enters a prompt
fragment. Error records replace tabs, newlines, and other controls and are
bounded before output.

A `complete` record is written immediately after a group's segment records
(`git` or `runtime`) by the client. It lets the shell release that group's
rendering barrier as soon as the group is done, rather than holding the prompt
blank until every asynchronous group finishes.

By default the Git group is *locked* and the runtime group is *unlocked*: the
shell stores Git segment and error records without redrawing until Git's
`complete` record releases its barrier (or `done` arrives), while runtime
records arrive as unlocked and redraw the prompt as they complete. A group can
be locked via `[async.lock]` to make the shell wait for it instead. The `done`
record remains the safety net: if a locked group never completed before
the shared deadline, `done` forces the render even though one was expected. If
the worker cannot start or exits without `done`, the shell falls back to
rendering the immediate segments rather than leaving the prompt stuck.

The generation is allocated by the shell integration. Zsh ignores records from
superseded generations, allowing directory changes to cancel or outlive an
older request safely. `done` is always emitted, including when there are no
asynchronous segments or the shared deadline expires, so the shell can finish
that generation's worker lifecycle.

### Zsh-to-client request protocol

`prompt/client.rs` defines the request the shell writes to the client daemon's
stdin for every asynchronous prompt. It is a single NUL-delimited record, so
fields need no escaping and non-UTF-8 paths round-trip byte-exact:

```text
ZTREQ<NUL>2<NUL>generation<NUL>cwd<NUL>
PATH<NUL>HOME<NUL>GIT_DIR<NUL>GIT_WORK_TREE<NUL>GIT_CEILING_DIRECTORIES<NUL>
VIRTUAL_ENV<NUL>CONDA_PREFIX<NUL>CONDA_DEFAULT_ENV<NUL>
PERLBREW_PERL<NUL>PLENV_VERSION<NUL>PYENV_VERSION<NUL>PYENV_DIR<NUL>
RUSTUP_TOOLCHAIN<NUL>RUSTUP_HOME<NUL>RBENV_DIR<NUL>RBENV_VERSION<NUL>
NODENV_VERSION<NUL>NODENV_DIR<NUL>PLENV_DIR<NUL>RUBY_VERSION<NUL>
JAVA_HOME<NUL>GOTOOLCHAIN<NUL>DOTNET_ROOT<NUL>
```

`ZTREQ` and version `2` guard against garbage input. The environment subset is
exactly what runtime detection, command resolution, and the Git query read.
The client does not apply it to its own process: the values are threaded
through the request explicitly and applied only to the child commands that
need them (set when present, removed when empty), because the client outlives
the shell's per-prompt environment changes. An empty environment field means
the variable is unset. The shell owns the write end of the request pipe, so
EOF on the client's stdin means the shell is gone and the client exits.

These protocols remain separate because they solve different compatibility
problems. The daemon protocol is a local service API carrying binary structured
data between Rust processes. The prompt protocols are streaming rendering APIs
carrying generation-scoped text and requests between the shell and its client.
Changing prompt rendering must not invalidate a daemon, and changing daemon
transport must not redefine shell record parsing.

## Adding a runtime

1. Add its explicit stable ID and canonical name to `define_runtimes!` in
   `runtime/mod.rs`. Never renumber existing IDs.
2. Add one `RuntimeSpec` match arm with its command, arguments, output routing,
   version parser, label, and environment resolver.
3. Add its marker, extension, precedence, or environment detection rules in
   `runtime/detect.rs`.
4. Add its segment configuration to `themes/catppuccin-mocha.toml`.
5. Add it to the default layout if it should render by default.
6. Add focused identity, parsing, serialization, and detection tests.
7. Update the supported-runtime list in `README.md`.

Bundled overlay themes need an explicit runtime entry only when they override
the Catppuccin base appearance.
