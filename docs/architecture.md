# Architecture

ztheme has two process layers:

1. Short-lived CLI and snapshot-helper processes.
2. A per-user background daemon.

This division exists because prompt rendering has conflicting requirements.
The shell-facing path must start quickly, finish within a strict deadline, and
never leave prompt work attached to the shell. Runtime discovery and Git
inspection, however, benefit from state that survives a single prompt render
and can be shared by multiple shells.

Zsh computes the immediate directory, character, and status segments itself.
For asynchronous segments it starts a short-lived `__snapshot` helper before
rendering the next prompt. The helper starts the Git request first, starts
runtime detection immediately after that, and exits after one shared deadline.
The helper does not keep background state of its own.

The shell stores the incoming fragments but does not redraw the prompt for each
one. It waits for the helper's final `done` record, then renders the complete
prompt once. While a snapshot is pending, Zsh leaves the prompt empty; it only
appears once every requested async segment has completed or the deadline has
expired. This makes the update atomic without adding a second animation
protocol.

The daemon provides that shared background state. It hosts two independent
long-lived capabilities:

- a persistent runtime-value cache, so runtime version commands are not
  executed for every prompt;
- a persistent `gitstatusd` client, so Git status queries reuse one optimized
  repository-status process instead of starting Git tooling for every prompt.

The daemon is not responsible for rendering. It returns structured Git data
and opaque encoded runtime snapshots; the short-lived helper renders those
values with the compiled theme.

## End-to-end prompt flow

```text
Zsh precmd/chpwd
├── start ztheme __snapshot
│   ├── start Git task first
│   │   └── daemon::git_status
│   │       └── persistent gitstatus::Client
│   │           └── gitstatusd
│   └── start runtime task immediately after Git begins
│       ├── detect project and active runtimes
│       ├── daemon::runtime_cache_get
│       ├── on miss: execute runtime snapshots
│       └── daemon::runtime_cache_put
├── compute synchronous shell segments while the helper runs
└── leave the prompt empty until the snapshot finishes

prompt protocol records
└── Zsh stores current-generation fragments without redrawing

shared 550 ms deadline
├── cancel unfinished helper tasks
└── write final done record

done record
└── Zsh renders all collected segments and redraws once
```

Git and runtime records may still arrive in either order on the prompt
protocol, but neither one is rendered by itself. Generation IDs prevent a slow
helper for an old working directory from overwriting a newer prompt.

## Why a daemon

The daemon is an optimization boundary and an ownership boundary, not a second
application tier.

Without it, every snapshot helper would have to start its own `gitstatusd`,
load and rewrite its own runtime cache, and coordinate concurrent shell
processes directly. That would put setup cost and filesystem races on the
latency-sensitive prompt path. A single per-user daemon instead provides:

- one owner for the `gitstatusd` subprocess;
- one in-memory runtime-cache state shared by all shells;
- serialized persistence and atomic cache replacement;
- a single place for daemon compatibility, startup, retry, and shutdown;
- isolation between production and explicitly named development instances.

The daemon starts lazily on the first ordinary request and exits after one hour
without a newly accepted connection. It is therefore not a permanently
installed system service and requires no service manager. The same `ztheme`
binary launches its hidden `__daemon` command with standard streams detached.

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
│   └── detect.rs
├── gitstatus/
├── prompt/
├── setup/
└── theme/
```

The larger `mod.rs` files intentionally keep each subsystem's primary
execution path local. New files are reserved for independent algorithms or
compatibility boundaries such as wire and disk formats.

## Subsystem ownership

### CLI

`cli.rs` owns Clap parsing, semantic argument validation, dispatch, exit
classes, stdout handling, broken-pipe handling, and construction of the
current-thread Tokio runtime used by asynchronous commands.

Hidden commands are shell implementation details and do not appear in public
help:

- `__daemon`
- `__snapshot`
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

- `runtime_cache_get`
- `runtime_cache_put`
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
`--dev NAME` into a distinct socket name. The name itself is not placed in the
filesystem path. This allows a development build and the installed production
binary to run simultaneously without sharing daemon state.

The transport is intentionally Unix-specific. ztheme targets Zsh because that
is what I use. Similarly it uses Unix process and filesystem semantics because 
I don't care about Windows.

#### Single ownership and lifecycle

Before binding the socket, a daemon acquires a non-blocking exclusive `flock`
on the instance's lock file. If another process owns the lock, the redundant
daemon exits successfully. This handles several clients racing to lazily start
the same instance without requiring coordination in the callers.

The lock owner removes a stale socket path, binds a new listener, creates the
runtime cache and `gitstatus::Client`, and starts cache load and flush tasks.
Each accepted connection is handled in its own Tokio task. Runtime-cache
requests can proceed concurrently; Git queries are serialized by the mutex
around the single stateful `gitstatusd` client.

The daemon stops when:

- no connection is accepted for one hour;
- a newer client reports a higher daemon-protocol version.

Orderly shutdown aborts outstanding client and background tasks, waits for
client tasks to terminate, flushes the newest cache revision, drops
`gitstatusd`, releases the lock, and removes the socket through a guard.
Unexpected process termination can leave the socket path behind; the next lock
owner removes that stale path before binding.

#### Client startup and replacement

Callers use `runtime_cache_get`, `runtime_cache_put`, and `git_status`; none of
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
old background process while preventing an older still-running shell helper
from downgrading the daemon.

`reset` is deliberately different. When the daemon exists it sends `RESET`,
which clears runtime cache state and restarts `gitstatusd`. When no daemon is
available it deletes persistent runtime-cache files directly and does not
start a process merely to clear state.

### Cache

`cache/mod.rs` owns runtime-cache policy and lifecycle:

- `CacheKey`;
- in-memory entries and freshness;
- LRU ordering and limits;
- load coordination;
- revisions and persistence scheduling;
- cache identity and disk path;
- clear and final-flush behavior.

`cache/disk.rs` owns the versioned persistent representation, validation,
private filesystem permissions, atomic replacement, directory synchronization,
and deletion of cache files.

Cache identity is independent from daemon protocol identity. A transport
change therefore does not automatically invalidate runtime values. Disk format
versioning changes only when the bytes stored by `cache/disk.rs` change.

The cache has no socket, process-spawning, Git-status, or daemon dependency.

The cache key represents the project state, detected runtimes, runtime command
specifications, and resolved executables that can affect a runtime snapshot.
The cached value is the runtime module's encoded snapshot. Cache deliberately
treats that value as opaque bytes: runtime serialization can evolve without
making cache policy aware of runtime fields.

Entries remain fresh for 24 hours and are limited to 500 entries and 16 KiB per
value. Reads update LRU order; stale entries are removed on access; insertion
trims the least recently used entries. Last-use timestamps are not persisted
on every hit, which avoids turning prompt reads into continuous disk writes.

Persistence is asynchronous:

1. startup begins loading the disk file while the daemon can already accept
   requests;
2. an epoch prevents a late load from restoring data after `clear`;
3. loaded entries merge without overwriting values created while loading;
4. mutations increment a revision and notify the flush loop;
5. writes are debounced for two seconds and retried after failures;
6. shutdown calls `flush_latest` to save the newest revision.

The disk file lives under the XDG cache directory, falling back to
`$HOME/.cache/ztheme`. Its filename includes an identity derived from an
independent cache-identity version, the disk-format version, package version,
and current executable path. This conservative identity permits a one-time
cold cache when the installed binary changes without coupling the decision to
daemon wire compatibility.

`cache/disk.rs` validates magic, format version, entry and value limits,
timestamps, duplicate keys, trailing data, file type, and private permissions.
Saving writes a mode-`0600` temporary file, flushes and synchronizes it, renames
it atomically, and synchronizes the containing mode-`0700` directory. A crash
therefore leaves either the preceding complete cache or the replacement,
rather than a partially written primary file.

### Runtime

`runtime/mod.rs` owns:

- stable runtime IDs and canonical names;
- runtime snapshot values and serialization;
- command selection;
- version parsing;
- compiler and environment labels;
- snapshot execution;
- runtime cache-key construction.

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
access belongs behind the daemon rather than in each snapshot helper.

`ztheme setup` installs the managed binary. `ztheme init zsh` refuses to
generate an integration when it is absent, so the daemon never silently
substitutes a different Git implementation with different behavior.

At daemon startup, `gitstatus::Client` launches the managed binary with piped
stdin and stdout, protocol compatibility restricted to `v1.5.*`, and its
parent PID. The child is killed when its owning Rust process is dropped.
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
the child if that limit is exceeded. The snapshot helper's 550 ms deadline is
much shorter: it can stop waiting and keep the prompt responsive without
discarding the daemon or its Git process merely because one prompt no longer
needs the result.

### Prompt

`prompt/mod.rs` is the application-level coordinator. It:

- starts the Git task first and lets it begin its daemon request;
- starts runtime detection immediately afterward;
- applies one shared 550 ms deadline;
- writes completed segments to the prompt stream;
- cancels unfinished work;
- always writes the final `done` record;
- sanitizes errors before sending them to Zsh.

It does not manage daemon sockets, process startup, cache persistence, or the
`gitstatusd` process.

The 550 ms limit is one deadline shared by both jobs, not 550 ms per operation.
Once it expires, the helper aborts unfinished Tokio tasks and writes `done`.
The shell treats that record as the rendering barrier, so a slow or missing
result cannot leave the prompt waiting forever. Dropping an in-flight daemon
request closes that helper's socket connection; the daemon remains independent
and available to later prompts.

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

`utils.rs` contains only the deterministic `HashBuilder` shared by daemon
instance naming, runtime cache keys, and persistent cache identity.

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

ztheme has two unrelated protocols.

### Daemon binary protocol

`daemon/protocol.rs` defines the versioned binary protocol used between
short-lived ztheme processes and the daemon. Each operation opens one Unix
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

Protocol version 1 defines these operation bytes:

| Byte | Operation | Request payload | Response |
| ---: | --- | --- | --- |
| `1` | runtime cache get | `u64` cache key | miss, or hit plus encoded value |
| `2` | runtime cache put | `u64` key plus length-prefixed value | OK |
| `4` | reset | none | OK |
| `5` | Git status | query kind plus length-prefixed path bytes | none, snapshot, or error |

Integers use Tokio's big-endian wire methods. Variable data is prefixed with a
`u32` length and validated before allocation or use. Runtime values are limited
to 16 KiB; paths and Git text fields to 16 KiB; daemon error text to 1 KiB.
Paths remain bytes across the protocol so valid non-UTF-8 Unix paths round-trip
without lossy conversion. Human-readable Git fields must be UTF-8.

The normal cache/reset exchange deadline is 25 ms. Git uses 500 ms because it
may require repository work, while still fitting beneath the helper's 550 ms
overall deadline in the usual case.

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

`prompt/protocol.rs` defines the line-oriented records written by
`__snapshot` to stdout and consumed by the Zsh integration:

```text
ZTHEME1<TAB>generation<TAB>segment<TAB>name<TAB>rendered-fragment<NL>
ZTHEME1<TAB>generation<TAB>error<TAB>source<TAB>message<NL>
ZTHEME1<TAB>generation<TAB>done<NL>
```

Each record is flushed immediately. A segment record is already rendered
prompt text, while daemon messages carry structured data. Percent characters
and control characters are escaped before dynamic text enters a prompt
fragment. Error records replace tabs, newlines, and other controls and are
bounded before output.

Zsh accepts and stores segment and error records for the current generation,
but does not redraw for each record. The final `done` record releases the
generation's rendering barrier and causes one complete prompt redraw. If the
worker cannot start or exits without `done`, the shell falls back to rendering
the immediate segments rather than leaving the prompt stuck.

The generation is allocated by the shell integration. Zsh ignores records from
superseded generations, allowing directory changes to cancel or outlive an
older helper safely. `done` is always emitted, including when there are no
asynchronous segments or the shared deadline expires, so the shell can finish
that generation's worker lifecycle.

These protocols remain separate because they solve different compatibility
problems. The daemon protocol is a local service API carrying binary structured
data between Rust processes. The prompt protocol is a streaming rendering API
carrying generation-scoped text into Zsh. Changing prompt rendering must not
invalidate a daemon, and changing daemon transport must not redefine shell
record parsing.

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
