# ztheme

A Zsh prompt for Git and runtime context, customizable themes, syntax
highlighting, and autosuggestions.

Git state and language versions are resolved asynchronously. ztheme then
displays the complete prompt in one update, avoiding partial or visually
jumping prompt redraws.

<p align="center">
  <img src="img/image-two.png" alt="Catppuccin Mocha ztheme prompt with Git status, Rust version, and syntax highlighting">
  <br>
  <sub>The bundled Catppuccin Mocha theme.</sub>
</p>

<p align="center">
  <img src="img/image-one.png" alt="Vesper ztheme prompt with syntax highlighting">
  <br>
  <sub>The bundled Vesper theme.</sub>
</p>

## Highlights

- Responsive prompt with asynchronous Git and runtime detection
- Git state powered by
  [gitstatusd](https://github.com/romkatv/gitstatus), without implicit fetches
- Themes for layout, colors, symbols, spacing, and input highlighting
- Automatic project detection for Rust, Python, Node.js, Go, Java, and more
- Sensible Zsh history, completion, navigation, and key-binding defaults
- No telemetry or network access during normal prompt use

## Install

ztheme supports Zsh 5.9 or newer (including the `zsh/system` module, which
ships with Zsh) on macOS and Linux. A
[Nerd Font](https://www.nerdfonts.com/) is recommended for the default symbols.

```console
cargo install ztheme --locked
ztheme setup
```

Add ztheme near the end of `.zshrc`:

```zsh
eval "$(ztheme init zsh)"
```

Start a new shell:

```console
exec zsh
```

`ztheme setup` installs the required Git helper and can also install
[zsh-syntax-highlighting](https://github.com/zsh-users/zsh-syntax-highlighting)
and [zsh-autosuggestions](https://github.com/zsh-users/zsh-autosuggestions).
Use `ztheme setup --yes` for unattended setup.

<details>
<summary>Other installation methods</summary>

Build from source with Rust 1.97 or newer:

```console
git clone https://github.com/alexykn/ztheme.git
cd ztheme
cargo install --locked --path . --root ~/.local
ztheme setup
```

Ensure `~/.local/bin` is on `PATH`.

Native archives for Apple Silicon, Intel macOS, x86_64 Linux, and arm64 Linux
are available on the [Releases](https://github.com/alexykn/ztheme/releases)
page. Verify the archive against `SHA256SUMS`, place `ztheme` on `PATH`, and run
`ztheme setup`.

</details>

## Themes

Explore available themes:

```console
ztheme theme list
```

Catppuccin Mocha is selected by default. Vesper is bundled as an alternative:

```console
ztheme theme apply vesper
```

<p align="center">
  <img src="img/image-big.png" alt="ztheme theme list showing palettes, layouts, symbols, and prompt previews" width="900">
</p>

Custom themes live in:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/ztheme/themes/
```

The active theme is selected in
`${XDG_CONFIG_HOME:-$HOME/.config}/ztheme/config.toml`:

```toml
version = 1
theme = "my-theme"
```

Custom themes inherit every omitted value from Catppuccin Mocha. Save one as
`themes/my-theme.toml`; a useful theme can stay small:

```toml
version = 1

[palette]
path = "#89b4fa"
muted = "#7f849c"
accent = "#f9e2af"
success = "#a6e3a1"
error = "#f38ba8"

[layout]
lines = [
    ["directory", "git", "python", "rust"],
    ["character"],
]
right = ["clock", "status"]

[segments.git]
prefix = "on "
symbol = " "

[segments.character]
success = "❯"
error = "❯"
spacing = { after = 1 }
```

Apply it, then use the edit-and-reload loop:

```console
ztheme theme apply my-theme
ztheme theme edit my-theme
ztheme theme reload
```

Layouts may use `directory`, `clock`, `git`, `character`, `status`, and the runtime
segments `python`, `perl`, `java`, `kotlin`, `scala`, `rust`, `go`, `bun`,
`deno`, `node`, `ruby`, `php`, `dotnet`, `c`, `cpp`, `swift`, and `lua`.

Colors accept palette names or `#RRGGBB`. Styles support `foreground`,
`background`, `bold`, `underline`, and `standout`.

See the complete [theme schema](docs/theme.md), including every layout and
segment key.

## Syntax highlighting

Syntax highlighting is part of the theme, not a separate color configuration.
Run `ztheme setup` to install the managed
[zsh-syntax-highlighting](https://github.com/zsh-users/zsh-syntax-highlighting)
integration.

Catppuccin Mocha already styles commands, arguments, paths, options, quotes,
errors, and shell syntax. Override only what your theme needs:

```toml
[input]
autosuggestion = { foreground = "muted" }
completion = { foreground = "accent" }

[input.syntax]
command = { foreground = "success", bold = true }
builtin = { foreground = "success" }
alias = { foreground = "success" }
path = { foreground = "path", underline = true }
single-hyphen-option = { foreground = "text" }
double-hyphen-option = { foreground = "text" }
single-quoted-argument = { foreground = "accent" }
double-quoted-argument = { foreground = "accent" }
comment = { foreground = "muted" }
unknown-token = { foreground = "error", bold = true }
```

Every entry under `input.syntax` is optional and uses the same style fields as
prompt segments. Set a style to `"none"` to disable it explicitly:

```toml
[input.syntax]
cursor = "none"
```

Supported names follow zsh-syntax-highlighting's main, brackets, cursor, line,
and root highlighters. The built-in
[`themes/catppuccin-mocha.toml`](themes/catppuccin-mocha.toml) is the complete
reference and a good starting point for fine-grained overrides.

See [syntax highlighting and input schema](docs/syntax-highlighting.md) for the
full list of accepted syntax-style names and configuration keys.

## Shell defaults

ztheme provides a practical interactive baseline:

- persistent, deduplicated shared history
- cached, case-insensitive completion with hidden files
- prefix-aware Up and Down history search
- terminal-aware Home, End, Delete, word, and arrow-key bindings
- directory-stack navigation and interactive comments
- asynchronous autosuggestions with Right Arrow, End, or Ctrl-Space to accept

These are ordinary Zsh settings. Put overrides after initialization and your
`.zshrc` remains authoritative:

```zsh
eval "$(ztheme init zsh)"

bindkey -v
unsetopt AUTO_CD
HISTSIZE=50000
```

To use only the prompt and input integrations:

```zsh
ZTHEME_SHELL_DEFAULTS=0
eval "$(ztheme init zsh)"
```

Set `ZTHEME_HISTORY_DEFAULTS=0` instead to disable only history defaults.
Set `ZTHEME_KEY_BINDINGS=0` before initialization to disable ztheme's
navigation and autosuggestion bindings. Set `ZTHEME_FOCUS_REPORTING=0` to
disable terminal focus tracking.

ztheme uses the active terminal's terminfo entry and supports both Emacs and Vi
keymaps. On macOS, Option-based word movement and deletion require the terminal
to send Option as Alt/Escape; this is usually named **Esc+** in iTerm2 and
**Option as Alt** in other terminals.

ztheme does not manage `PATH`, aliases, editor selection, terminal-emulator
preferences, or application environment variables.

## Commands

| Command | Purpose |
| --- | --- |
| `ztheme setup` | Install or repair prompt helpers |
| `ztheme theme list` | Preview available themes |
| `ztheme theme apply NAME` | Apply and remember a theme |
| `ztheme theme edit NAME` | Open a theme in `$VISUAL` or `$EDITOR` |
| `ztheme theme reload` | Reload the current theme after editing |
| `ztheme clear` | Clear runtime values and reset Git status |

## Updating and removal

Update a Cargo installation with:

```console
cargo install ztheme --locked --force
```

To remove ztheme, delete the executable and its initialization line. Managed
data is stored below `${XDG_DATA_HOME:-$HOME/.local/share}/ztheme`, cached data
below `${XDG_CACHE_HOME:-$HOME/.cache}/ztheme`, and themes below
`${XDG_CONFIG_HOME:-$HOME/.config}/ztheme`.

## Architecture

A per-shell client daemon renders prompts and talks to a per-user server
daemon that hosts runtime caching and the persistent `gitstatusd` client.
Keeping the client alive avoids spawning a process for every prompt.
See [Architecture](docs/architecture.md) for subsystem ownership, protocols,
and the runtime-extension workflow. See [Runtime cache](docs/cache.md) for
cache identities, persistence, and limitations. See [Segments](docs/segments.md)
for the synchronous segment system, including user-defined segments.

## Built with

- [Catppuccin](https://github.com/catppuccin/catppuccin) provides the Mocha
  palette used by the default theme.
- [Vesper](https://github.com/raunofreiberg/vesper) provides the palette used
  by the bundled Vesper theme.
- [gitstatusd](https://github.com/romkatv/gitstatus) provides fast Git status.
- [zsh-syntax-highlighting](https://github.com/zsh-users/zsh-syntax-highlighting)
  highlights commands as they are typed.
- [zsh-autosuggestions](https://github.com/zsh-users/zsh-autosuggestions)
  provides history-based suggestions.

## License

[MIT](LICENSE). gitstatusd is a separate GPL-3.0-licensed process.
