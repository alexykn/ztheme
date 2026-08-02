# Segments

ztheme's prompt is assembled from segments. There are two kinds of segment
providers, and both are first-class layout entries.

## Two segment providers

**Shell-provided segments** run synchronously in the interactive Zsh process
during `precmd`. Each is one Zsh function:

- `directory`, `clock`, `status`, `character` — bundled with ztheme
- user-defined segments such as `time` or `load`

**Client-provided segments** are computed asynchronously by the per-shell
`ztheme __client-daemon` process and delivered as finished, pre-styled
fragments:

- `git`
- runtime segments such as `python`, `rust`, `node`

Git and runtime segments exist because their computation (gitstatusd queries,
version detection, caching) happens in the Rust process; they cannot be
reimplemented as Zsh segment files. Everything else is a shell function.

Both kinds are rendered in the same atomic final draw: the shell computes its
segments while the client resolves git/runtime values, and the complete prompt
is drawn once the fragments arrive or the shared deadline expires.

## Why custom segments are synchronous

A custom segment runs in the shell process, once per prompt, as an ordinary
function call. ztheme deliberately does not fork or spawn per segment:
process creation costs about 1–2 ms, while a typical segment (clock format,
`/proc` read) costs microseconds. Parallelizing microseconds of work by paying
milliseconds of fork overhead would be a net loss, so segments run
sequentially in `precmd`.

A segment function may invoke an external command, but that latency is paid on
every prompt and is the author's responsibility. Keep segments builtin-only
Zsh when you can.

## Opt-in loading

Custom segments are loaded only after explicit enablement. `config.toml`
(`$XDG_CONFIG_HOME/ztheme/config.toml`, falling back to
`$HOME/.config/ztheme/config.toml`) names the enabled ids:

```toml
version = 1
theme = "catppuccin-mocha"

[custom_segments]
enabled = ["time", "cpu"]
```

An id in `enabled` but absent from the active theme's layout is ignored. A
segment used by the active layout must be enabled, or `ztheme init zsh` /
`ztheme theme reload` fails with a message.

The allowlist is the whole gate: files that are not explicitly enabled are
never read or executed. The segments directory is never scanned.

## Header format

Every segment file begins with exactly one identity line:

```zsh
# ztheme-segment-v1: time
```

The value after the colon is the segment id and must equal the filename stem
(`time.zsh` → `time`) and the enablement entry. Bundled segments carry the
same header:

```zsh
# ztheme-segment-v1: directory
# ztheme-segment-v1: clock
# ztheme-segment-v1: status
```

During initialization, ztheme reads the first line of each active file and
requires this exact header. A file without it, with a different version, or
with a mismatched id fails initialization with a message. There is no general
metadata format in v1 — one versioned identity line only.

## Identifier restrictions

A custom id:

- is 1–64 bytes;
- matches `[a-z][a-z0-9_]*` (lowercase start; letters, digits, underscores);
- must not equal a bundled id (`directory`, `clock`, `git`, `character`, `status`);
- must not equal any supported runtime name (`python`, `perl`, `java`,
  `kotlin`, `scala`, `rust`, `go`, `node`, `ruby`, `dotnet`, `c`, `cpp`).

The reserved set is derived from the runtime table; there is no second manual
list to keep in sync. Invalid ids are rejected, never normalized — hyphens,
case changes, and punctuation are not translated into shell identifiers. The
strict grammar makes generated function and variable names safe to construct
without `eval`.

## File location

Each enabled segment lives at exactly:

```text
$XDG_CONFIG_HOME/ztheme/segments/<id>.zsh
```

Only the exact filename derived from the validated id is considered. The file
must be a regular file; symbolic links and other file types are rejected.

## Function contract

Every shell-provided segment defines one function:

```zsh
ztheme_segment_<id>() {
    emulate -L zsh

    # Compute the value and hand it to the shared renderer.
    _ztheme_segment_render <id> <value> [variant]
}
```

The dispatcher calls the function with the previous command status:

```zsh
ztheme_segment_time "$last_status"
```

Available context inside the function:

- `$1` — previous command status;
- `$PWD`, `$HOME`;
- ordinary shell and environment variables of the current shell.

Functions must not:

- depend on another segment's computed value;
- depend on invocation order;
- register their own `precmd` hook;
- start background work through ztheme;
- mutate ztheme's async state;
- apply colors, spacing, prefixes, or suffixes directly (styling comes from
  the theme).

## Shared rendering helper

`_ztheme_segment_render` wraps a value in its theme-provided styling:

```zsh
_ztheme_segment_render <id> <value> [variant]
```

The theme compiler generates the styling as two associative arrays keyed by
`id:variant`:

```zsh
typeset -gA __ZTHEME_SEGMENT_OPEN
typeset -gA __ZTHEME_SEGMENT_CLOSE
```

Entries exist for `directory:default`, `clock:default`, `status:success`, `status:error`,
`character:success`, `character:error`, and `<custom-id>:default`. The
function owns only the middle value; the renderer applies
`<spacing-before><style><prefix>` before it and `<suffix><style-reset><spacing-after>`
after it.

The value is treated as a prompt fragment and is not escaped. If a segment
renders data that could contain `%`, quotes, or other prompt syntax, the
author must escape it (`${value//\%/%%}` etc.). There is no second escaping
mode in v1.

A segment that cannot produce a value clears `REPLY` and returns; the
dispatcher stores an empty fragment for that prompt. One failed segment does
not affect the others, and there is no per-prompt warning output.

## Theme layout and styling

The theme layout lists segment ids, including custom ones:

```toml
[layout]
lines = [["directory", "git", "character"]]
right = ["time"]

[segments.custom.time]
prefix = "🕐 "
suffix = ""
style = { foreground = "muted" }
spacing = { before = 1, after = 0 }
```

Rules:

- a custom segment used in the layout must have a `segments.custom.<id>` entry;
- unused `segments.custom.*` entries are allowed (optional segments);
- custom segments may appear on the left or the right prompt;
- duplicates across the layout remain rejected;
- `character` must remain the final segment of the final left line;
- git and runtime segments keep their existing right-prompt restrictions.

Custom segments use one style each in v1; per-variant styling exists only for
the bundled `character` and `status` segments.

## Bundled clock

`clock` renders the local time as `HH:MM` without spawning a process. It is a
normal synchronous layout segment and needs no file or configuration entry. It
uses Zsh's `strftime` builtin from the standard `zsh/datetime` module:

```toml
[layout]
right = ["clock", "status"]

[segments.clock]
prefix = " "
style = { foreground = "muted" }
spacing = { after = 1 }
```

Vesper enables this layout by default.

## Reload workflow

`ztheme theme reload` re-reads `config.toml`, re-resolves the active theme's
custom segments, revalidates headers, regenerates the integration, and applies
it to the current shell. Newly enabled segments are available immediately;
segments removed from the layout are no longer dispatched. Previously defined
segment functions may remain defined after a reload; they are simply never
called again. No new command was added for this.

## What this does and does not guarantee

- Custom segments run with your user's normal shell privileges. There is no
  isolated execution environment.
- The header and allowlist prevent unlisted files from being sourced at all —
  a stray file in the directory is never executed. They do not review the
  contents of a file you enabled; an enabled file runs as-is.
- Custom segments cannot replace bundled or runtime ids; reserved names are
  rejected.
- Custom segments cannot depend on each other and are not ordered guarantees.
- Slow segment functions delay prompt availability. There is no time budget
  enforcement; latency is the author's responsibility.

## Performance

The prompt hot path gained no filesystem work:

- no directory scan, header read, config parse, or subprocess during `precmd`;
- exactly one function call per active synchronous segment;
- only segments present in the active layout are called;
- git/runtime computation still overlaps with shell-side segment computation.

All filesystem discovery and validation happens once, during
`ztheme init zsh` or `ztheme theme reload`.

## Example: time

```text
~/.config/ztheme/segments/time.zsh
```

```zsh
# ztheme-segment-v1: time

zmodload -F zsh/datetime b:strftime || return 1

ztheme_segment_time() {
    emulate -L zsh
    local value
    strftime -s value '%H:%M'
    _ztheme_segment_render time "$value"
}
```

```toml
# ~/.config/ztheme/config.toml
[custom_segments]
enabled = ["time"]
```

```toml
# theme overlay
[layout]
lines = [["directory", "git", "character"]]
right = ["time"]

[segments.custom.time]
prefix = "🕐 "
style = { foreground = "muted" }
spacing = { before = 1, after = 0 }
```

## Example: Linux load average

```text
~/.config/ztheme/segments/load.zsh
```

```zsh
# ztheme-segment-v1: load

ztheme_segment_load() {
    emulate -L zsh

    local value
    builtin read -r value _ </proc/loadavg || {
        REPLY=""
        return
    }

    _ztheme_segment_render load "$value"
}
```

This example is platform-specific and shown for illustration only; platform
examples are not shipped as bundled segments.
