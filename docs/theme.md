# Theme schema

ztheme themes are TOML overlays. A custom theme inherits every omitted value
from the bundled [Catppuccin Mocha theme](../themes/catppuccin-mocha.toml), so
you only need to write the values you want to change.

## Select a theme

`$XDG_CONFIG_HOME/ztheme/config.toml` (or `~/.config/ztheme/config.toml`) selects
the active theme:

```toml
version = 1
theme = "my-theme"
```

`theme` can be a bundled selector (`catppuccin-mocha` or `vesper`), a simple
theme name resolved as `~/.config/ztheme/themes/<name>.toml`, or an absolute
path. Simple names may contain only letters, numbers, `-`, and `_`.

Every theme file must start with `version = 1`. Tables merge recursively with
Catppuccin Mocha; arrays and scalar values replace the inherited value.
Unknown keys are rejected.

## Common value types

### Colors and styles

Every color value is either a palette name or an RGB color in `#RRGGBB` form.
Palette keys use letters, numbers, `-`, and `_`. The merged palette must define
`accent`, `muted`, `success`, and `error`.

```toml
[palette]
path = "#89b4fa"
text = "#cdd6f4"

# A style may use any combination of these fields.
style = {
    foreground = "path",
    background = "#1e1e2e",
    bold = true,
    underline = false,
    standout = false,
}
```

`foreground` and `background` are optional. `bold`, `underline`, and `standout`
default to `false`. A style can be empty (`{}`), which deliberately applies no
attributes.

### Spacing

Every prompt segment supports this optional table:

```toml
spacing = { before = 1, after = 1 }
```

Both values default to `0` and must be between `0` and `16`. This spacing is
outside the segment's style: it does not extend a background color. Use it for
the gap after a colored prompt character, rather than putting that gap in a
styled suffix.

Prompt literals (symbols, prefixes, suffixes, and separators) cannot contain
control characters.

## Layout

```toml
[layout]
lines = [
    ["directory", "git", "python", "rust"],
    ["character"],
]
right = ["clock", "status"]
separator = " "
blank_line_before = true
```

| Key | Type | Meaning |
| --- | --- | --- |
| `lines` | array of arrays of segment names | Left prompt, from top to bottom. There must be 1–8 non-empty lines. |
| `right` | array of segment names | Right prompt. Only `directory`, `clock`, and `status` are allowed here. |
| `separator` | string | Inserted between non-empty segments on the same side. |
| `blank_line_before` | boolean | Adds one blank line before the left prompt. |

There can be at most 64 segment placements in total, and a segment can appear
only once. `character`, when used, must be the last segment of the last left
line. Git and runtime segments are asynchronous and are therefore left-prompt
only.

The accepted segment names are:

```text
directory  clock  git  character  status
python  perl  java  kotlin  scala  rust  go  bun  deno  node  ruby
php  dotnet  c  cpp  swift  lua  r  julia  elixir  dart  haskell  zig
```

## Prompt segments

### `segments.directory`

The directory is available immediately, before background Git/runtime results.

```toml
[segments.directory]
prefix = ""
suffix = ""
home_symbol = "~"
truncation_symbol = "…"
style = { foreground = "path", bold = true }
spacing = { before = 0, after = 0 }

[segments.directory.width]
percent = 45
minimum = 24
maximum = 72
```

`prefix`, `suffix`, and `spacing` are optional. `home_symbol`,
`truncation_symbol`, `style`, and `width` are required in the final merged
theme. The display-width budget is `percent` of terminal width, clamped between
`minimum` and `maximum`. `percent` must be 1–100; `minimum` must be positive
and no greater than `maximum`.

### `segments.clock`

`clock` is immediate and renders the local time as `HH:MM` without spawning a
process. It may appear on either prompt side.

```toml
[segments.clock]
prefix = " "
suffix = ""
style = { foreground = "muted" }
spacing = { before = 0, after = 0 }
```

Only `prefix`, `suffix`, and `spacing` are optional after inheritance.

### `segments.git`

Git is rendered asynchronously by gitstatusd. It is empty outside a Git worktree
or while its first result is still pending.

```toml
[segments.git]
prefix = "on "
suffix = ""
symbol = " "
action_prefix = " "
action_suffix = ""
changes_prefix = " "
style = { foreground = "muted" }
action_style = { foreground = "accent" }
spacing = { before = 0, after = 0 }

[segments.git.symbols]
conflicted = "="
staged = "+"
modified = "!"
deleted = "✘"
untracked = "?"
ahead = "⇡"
behind = "⇣"
diverged = "⇕"
stash = "$"

[segments.git.styles]
conflicted = { foreground = "error", bold = true }
staged = { foreground = "success", bold = true }
modified = { foreground = "warning", bold = true }
deleted = { foreground = "error", bold = true }
untracked = { foreground = "git", bold = true }
ahead = { foreground = "success" }
behind = { foreground = "git" }
diverged = { foreground = "git" }
stash = { foreground = "accent" }
```

All Git fields except `prefix`, `suffix`, `action_suffix`, and `spacing` are
required after inheritance. `symbol` precedes the branch. `action_prefix` and
`action_suffix` wrap an in-progress Git action such as a rebase. `changes_prefix`
precedes the marker group. `diverged` is used instead of separate `ahead` and
`behind` symbols when both counts are non-zero; it is followed by `A/B`.
`stash` is followed by its count. Working-tree markers are symbols only.

### `segments.character`

The character is immediate and reflects the previous command's exit status.

```toml
[segments.character]
prefix = ""
suffix = " "
success = "❯"
error = "❯"
success_style = { foreground = "git" }
error_style = { foreground = "error" }
spacing = { after = 1 }
```

`success` is used after exit status 0; `error` is used otherwise. Only `prefix`,
`suffix`, and `spacing` are optional after inheritance.

### `segments.status`

`status` displays the exact non-zero exit code. It may be on either prompt side.

```toml
[segments.status]
prefix = ""
suffix = ""
show_success = false
success_symbol = "0"
style = { foreground = "error" }
success_style = { foreground = "success" }
spacing = { before = 0, after = 0 }
```

With `show_success = false`, a successful status is omitted. With it enabled,
`success_symbol` is shown for status 0 using `success_style`; other statuses use
their decimal code and `style`. `prefix`, `suffix`, and `spacing` are optional.

### Runtime segments

The runtime names are `python`, `perl`, `java`, `kotlin`, `scala`, `rust`, `go`,
`bun`, `deno`, `node`, `ruby`, `php`, `dotnet`, `c`, `cpp`, `swift`, `lua`, `r`,
`julia`, `elixir`, `dart`, `haskell`, and `zig`.
Each is configured with the same schema:

```toml
[segments.python]
prefix = "via "
suffix = ""
symbol = " "
version_prefix = "v"
style = { foreground = "accent" }
spacing = { before = 0, after = 0 }

[segments.python.environment]
prefix = " ("
suffix = ")"
style = { foreground = "muted" }
```

`prefix`, `suffix`, `version_prefix`, `spacing`, and the entire `environment`
table are optional. `symbol` and `style` are required after inheritance.
`environment.prefix`, `environment.suffix`, and `environment.style` each default
to an empty value. The environment subsegment is rendered only when detection
has an environment label; configuring it does not force one to appear.

Runtime detection is asynchronous. A runtime segment remains empty when ztheme
does not detect that runtime in the current project. When a runtime is detected
but its executable is missing, the segment renders its symbol and language name
without a version.

## Complete shape

This is the complete top-level structure. Most custom themes should override a
small subset rather than copy it.

```text
version
palette.<name>
input.autosuggestion
input.completion
input.syntax.<highlight-name>
layout.lines
layout.right
layout.separator
layout.blank_line_before
segments.directory.*
segments.directory.width.*
segments.clock.*
segments.git.*
segments.git.symbols.*
segments.git.styles.*
segments.character.*
segments.status.*
segments.<runtime>.*
segments.<runtime>.environment.*
```

For the exact complete, valid configuration—including every runtime section—use
[Catppuccin Mocha](../themes/catppuccin-mocha.toml) as the reference base.
Input and syntax-highlight keys are documented separately in
[Syntax highlighting](syntax-highlighting.md).
