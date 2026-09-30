# Runtime cache

Runtime version commands can dominate an otherwise warm prompt. In
particular, starting Python through a virtual environment or version-manager
shim can take several milliseconds or more even when project detection and
rendering are already optimized. ztheme therefore keeps successful per-runtime
values in the shared server daemon and persists them across daemon restarts.

The cache is an optimization, not the source of project identity. Project and
language detection still runs for every prompt, and runtime selection is
resolved from the current request before lookup. A hit is valid only when the
selected command and every input visible to that cacheable command have the
same semantic identity.

See [Architecture](architecture.md) for the surrounding process model,
subsystem ownership, and wire protocols.

## Request flow

For each prompt, the client:

1. performs fresh detection, executable resolution, and selector planning in
   one bounded filesystem job, considering only detected, theme-enabled runtimes;
2. classifies each selected runtime as cacheable or volatile;
3. starts one independent task per selected runtime;
4. executes volatile plans on every request without cache access;
5. acquires each cacheable runtime's own semantic key from the server daemon;
6. on a hit, validates a singleton value with that runtime's stable ID;
7. on a cold miss, executes that runtime, then refreshes its selection and
   identity before publishing through its owned singleflight token;
8. if selection changed during execution, releases ownership and retries once
   for that runtime only; an unstable second attempt is neither cached nor
   rendered as current;
9. materializes presentation labels from the current request and emits each
   completed runtime immediately, without waiting for slow siblings.

The first value rendered for each runtime comes from the current request.
ztheme does not render a stale snapshot and replace it later. The shell's
runtime group lock controls whether these independent events are buffered or
redrawn incrementally; by default runtimes are unlocked.

One volatile runtime does not disable caching for unrelated cacheable runtimes
in the same prompt.

## Cacheability boundary

A runtime plan is cacheable only when ztheme can identify the concrete native
executable that will answer the version command and can describe every command
input that may change the answer.

### Direct executables

An ordinary native executable is identified by:

- the requested path;
- symlink metadata;
- the canonical target path;
- target metadata, including device, inode, size, mode, change time, and
  modification time.

Replacing the executable or changing a symlink therefore changes the key. Raw
PATH is not hashed: PATH is resolved afresh, and the selected executable is the
semantic result that matters.

PATH resolution uses the request working directory. Empty PATH components mean
the request directory, and relative components are resolved beneath it.

Java and .NET select the first matching executable from request PATH. `JAVA_HOME`
and `DOTNET_ROOT` remain available as command context, but they do not replace
that PATH selection. .NET project and installed-SDK state is intentionally not
fingerprinted; detected .NET plans execute as volatile. macOS system compiler,
Swift, and Java launchers also stay volatile because they can select Xcode,
Command Line Tools, or a JDK contextually. Both the requested path and canonical
routing target are checked, so a symlink cannot make a known launcher cacheable.
Actual SDK/JDK binaries with the same basename can still be cacheable.

### Python environments

Python selection checks, in order:

1. `$VIRTUAL_ENV/bin/python`;
2. `$CONDA_PREFIX/bin/python`;
3. `python` on PATH;
4. `python3` on PATH.

This covers ordinary activated environments created by `python -m venv`,
`uv venv`, and equivalent tools. The selected executable receives a direct
identity; ztheme does not hash the entire virtual-environment directory.

### Supported shim layouts

ztheme has bounded selection support for conventional pyenv, rbenv, nodenv,
and plenv layouts. It recognizes a verified manager root, applies that
manager's explicit version variable or nearest selector file, falls back to
the manager's root version file, and identifies the selected executable under
the existing `versions/<name>/bin` layout.

Manager directory variables are resolved against the request working
directory when relative. Selector searches stop after 32 directories.

The resolved target is cacheable only when it is a concrete native executable.
If it is itself a script or dispatcher, ztheme executes the original shim as
volatile instead of caching a partial simulation.

### Rustup

The rustup resolver intentionally supports only ordinary, already-installed
toolchains. Selection precedence is:

1. `RUSTUP_TOOLCHAIN`;
2. the closest directory override, `rust-toolchain`, or
   `rust-toolchain.toml` while walking toward the filesystem root;
3. at the same directory, an override before either file and
   `rust-toolchain` before `rust-toolchain.toml`;
4. the configured default toolchain.

Supported selector values are a single toolchain name, a
`[toolchain].channel`, or an absolute `[toolchain].path`. The selected
toolchain must resolve unambiguously to an existing native `rustc`. The search
is bounded to 32 ancestor directories and a bounded number of installed
toolchain entries.

Components, targets, profiles, automatic installation, plugins, and other
rustup behavior are not simulated.

### Go

Go is cacheable only when automatic toolchain switching is explicitly disabled
with `GOTOOLCHAIN=local`. Other modes execute as volatile because the selected
toolchain can depend on state not represented by the direct executable.

### Volatile plans

Scripts, unknown dispatchers, ambiguous or unreadable selectors, unsupported
selection modes, and contextual .NET selection are volatile. A volatile plan:

- executes on every prompt where the runtime is detected;
- is never stored in the semantic cache;
- receives the current known runtime-selection environment from the request;
- cannot poison cacheable values for other runtimes.

Volatile execution is the conservative fallback. It preserves current results
without requiring ztheme to become a general-purpose version or package
manager.

Dart, Zig, Julia, and R are cacheable when they resolve to a direct native
SDK executable. Script-based frontends (such as Flutter or version-manager
wrappers), shims, and dispatchers remain volatile through shebang and shim
detection.

Juliaup remains volatile: it performs contextual channel selection (channel
environment, directory overrides, project/manifest selection, then its
configured default channel), so its launcher identity alone cannot prove which
Julia will run. R's standard Unix launcher is a shell script and remains
volatile because it selects the sub-architecture through `R_ARCH`; that
variable is forwarded to volatile execution so `R --version` observes the same
selection as the shell. The `elixir` and `haskell` runtimes are currently
always volatile while their caching model is being evaluated. They execute on
every prompt where detected and are never stored in the semantic cache.

## Command environment

Every runtime command starts from a cleared environment with deterministic
output controls such as `LC_ALL=C`, `TERM=dumb`, and `NO_COLOR=1`.

Cacheable commands receive only their declared runtime-specific inputs. Every
visible value is therefore either fixed by ztheme or included in that
runtime's semantic cache context. Python's declared cache inputs do not include
the Rust toolchain selector, because changing Rust selection cannot affect a
direct Python `--version` command.

Volatile commands additionally receive the current request's PATH, HOME, and
known runtime-selection variables. Git routing variables are never exposed to
runtime commands.

Presentation-only environment names are not stored in cached values. They are
materialized from the current request after cache lookup, so renaming an
environment does not return a stale label or invalidate an otherwise identical
runtime version.

## Semantic key

Each runtime's key is SHA-256 over a length-delimited, namespaced serialization
of its own cacheable plan. Adding, removing, failing, or changing a sibling
runtime does not invalidate that value. It includes:

- the `ztheme-runtime-value-v3` semantic domain and runtime-value version `3`
  (independent of disk-format and daemon-protocol version `2`);
- that runtime's stable ID;
- command arguments and output mode;
- the selected executable or supported selection context;
- only environment values declared for that runtime command.

It deliberately excludes:

- the raw PATH;
- the complete process environment;
- unrelated runtime selectors;
- project marker files after fresh detection has selected the active runtimes;
- working-directory paths for ordinary direct executables;
- presentation-only labels;
- the ztheme package version;
- the ztheme executable path.

Semantic changes increment the appropriate key namespace. They do not require
a new persistent filename while the disk representation remains compatible.
Old semantic entries remain harmless and disappear through normal LRU
eviction.

## Singleflight

Cold misses use daemon-side singleflight. The first acquire for a key receives
an ownership token and executes that runtime command in its per-shell client.
Later clients wait on the same key and receive the owned value instead of
starting duplicate commands.

Ownership expires after 400 ms so a killed client cannot block the key
indefinitely. An expired token is rejected even if no replacement has yet
acquired the key. Tokens are ownership-specific: a late owner cannot overwrite
a newer value. Failed executions release ownership; cancellation may leave a
lease to expire. Expired leases are reclaimed across all keys on cold misses,
not just when their original key is revisited, and reclamation wakes waiters.
Warm hits do not scan leases. At most 500 owner leases coexist; a new miss at
live capacity fails softly and runs uncached rather than evicting a live owner.
A waiter on an existing key can still join at capacity.

Waiting occurs on the daemon connection rather than through polling. Runtime
commands remain outside the shared daemon, so a slow child process does not
block the daemon's event loop.

## LRU and persistence

The cache holds at most 500 entries, each with a maximum encoded value size of
16 KiB. Each new semantic entry encodes exactly one successful runtime value;
aggregate entries from older key namespaces are never mistaken for a current
singleton. Reads move entries to the most-recently-used end in memory. Insertion
past the limit removes the least-recently-used entry.

Recency persistence is intentionally approximate. A hit updates memory
immediately, but the disk file is not rewritten for every prompt. Killing the
daemon immediately after a hit may therefore lose that final recency movement;
the cached value itself remains valid. This tradeoff avoids turning every
prompt into a disk write.

Persistence is asynchronous:

1. daemon startup loads the disk file before accepting requests;
2. an epoch prevents a late load from restoring data after `clear`;
3. loaded entries merge without overwriting newer in-memory values;
4. mutations increment a revision and notify the flush loop;
5. writes are debounced for two seconds and retried after failure;
6. orderly shutdown calls `flush_latest` for the newest revision.

Production uses `$XDG_CACHE_HOME/ztheme/runtime-v2.bin`, falling back to
`$HOME/.cache/ztheme/runtime-v2.bin`. Named development instances use
`runtime-v2-dev-<stable-instance-hash>.bin`, so they cannot overwrite
production or one another. `ztheme clear` selects the same instance-specific
path as daemon startup and flushing.

The persistent identity depends on the disk format, not the package version or
executable path. Cache disk-format versioning is independent from both daemon
and prompt protocol versions.

## Disk safety

`cache/disk.rs` validates:

- magic and disk-format version;
- entry count and value size;
- timestamps and duplicate keys;
- trailing data;
- file type and private permissions.

Saving creates a mode-`0600` temporary file, flushes and synchronizes it,
renames it atomically, and synchronizes the containing mode-`0700` directory.
A crash therefore leaves either the preceding complete file or the replacement
rather than a partially written primary file.

Corruption, incompatible formats, and persistence failures do not make prompt
rendering fail. Invalid files are ignored or removed, and a failed write does
not discard the freshly calculated runtime value.

## Invalidation

There is no wall-clock expiry. A valid entry can remain indefinitely until it
is evicted from the 500-entry LRU.

Correctness comes from fresh selection and semantic identities:

- selecting a different executable produces a different key;
- replacing or relinking an executable changes its identity;
- changing a supported selector changes the selected target or context;
- changing a declared command environment value changes that runtime's key;
- adding or removing a project marker changes fresh runtime detection before
  lookup;
- unsupported dynamic selection executes as volatile.

`ztheme clear` remains the explicit recovery and diagnostic operation. It is
not part of ordinary freshness behavior.

## Limitations

The cache deliberately does not model every way a runtime can be selected.
The boundary is conservative: an unmodeled selection should cost a runtime
command, not return a cached guess.

Current limitations are:

- asdf, mise, and arbitrary custom dispatchers are not selection-resolved;
  they execute as volatile;
- scripts and nested dispatchers are never treated as stable direct
  executables;
- .NET SDK selection is not fingerprinted, so `dotnet` is volatile;
- macOS system compiler/Swift/Java launchers and Juliaup select contextual
  targets and are volatile; native implementation format alone is not proof
  of stable selection;
- Go automatic toolchain selection is volatile unless
  `GOTOOLCHAIN=local` makes it explicit;
- rustup support is limited to the documented selector precedence and
  already-installed, unambiguous toolchains;
- pyenv/rbenv/nodenv/plenv support assumes their conventional directory
  layouts and simple single-version selectors;
- selector searches stop after 32 ancestors, and bounded directory scans
  become volatile when their limits are exceeded;
- volatile commands receive the environment fields carried by the prompt
  protocol, not an arbitrary dump of every shell variable;
- LRU recency on disk is batched and can lag the exact in-memory order;
- the cache does not eliminate cold execution after a genuine executable or
  supported selector change.

These are support boundaries, not invitations to reproduce tool-manager
internals. New cacheable selection modes should be added only when their
concrete executable and complete command inputs can be represented simply and
tested without placing substantial work on the warm prompt path.

## Verification

Cache changes should preserve these properties:

- the first prompt after a supported selector or executable change is current;
- repeated warm prompts do not execute runtime commands;
- concurrent cold requests share one execution per stable cacheable runtime
  within its lease;
- a daemon restart reuses persisted entries;
- volatile runtimes never enter persistent state;
- the realistic workload has no stale results and retains a high hit rate;
- the `rustup-warm` fixture covers proxy selection and installed-toolchain
  resolution on the warm prompt path;
- shallow and deep warm-prompt latency remain within the benchmark gates.

The benchmark harness is `tools/benchmark-runtime-cache.zsh`; see
[Benchmarks](benchmarks.md) for current measured values and how to reproduce
them.

Benchmark note: the first invocation can include host CPU and filesystem/cache
warm-up, and the candidate is measured after the baseline. For manual latency
comparisons, run the harness twice and treat the second run as more indicative,
or compare medians across repeated runs. This affects timing stability, not
cache correctness.
