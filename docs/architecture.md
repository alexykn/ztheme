# Architecture

ztheme has two process layers:

1. Short-lived CLI and snapshot-helper processes.
2. A per-user background daemon.

The prompt renders its immediate directory, character, and status segments in
Zsh. A snapshot helper concurrently requests Git state and runtime values,
streams completed segments back to Zsh, and exits after a shared deadline.

The daemon hosts two separate long-lived capabilities:

- a persistent runtime-value cache;
- a persistent `gitstatusd` client.

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

### Prompt

`prompt/mod.rs` is the application-level coordinator. It:

- starts Git and runtime work concurrently;
- applies one shared 550 ms deadline;
- writes completed segments incrementally;
- cancels unfinished work;
- always writes the final `done` record;
- sanitizes errors before sending them to Zsh.

It does not manage daemon sockets, process startup, cache persistence, or the
`gitstatusd` process.

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

`daemon/protocol.rs` defines the versioned binary Unix-socket protocol used
between short-lived processes and the daemon. It carries runtime-cache
operations, Git queries, reset requests, compatibility responses, and bounded
payloads.

`prompt/protocol.rs` defines the line-oriented records written by a snapshot
helper and consumed by Zsh. Records carry generation IDs, segment fragments,
sanitized errors, and the final completion marker.

These protocols must remain separate. Changing prompt rendering does not
require changing daemon wire compatibility, and changing daemon transport does
not redefine the shell protocol.

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
