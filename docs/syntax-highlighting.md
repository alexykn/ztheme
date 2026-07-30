# Syntax highlighting and input schema

ztheme configures [zsh-syntax-highlighting](https://github.com/zsh-users/zsh-syntax-highlighting)
from the active theme. Run `ztheme setup` to install and source its managed
integration. These settings affect editable command-line input, not the prompt
segments described in [Theme schema](theme.md).

## Input settings

```toml
[input]
autosuggestion = { foreground = "muted" }
completion = { foreground = "accent", bold = true }
```

| Key | Applied to | Value |
| --- | --- | --- |
| `autosuggestion` | `ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE` | A [style](theme.md#colors-and-styles). |
| `completion` | zsh completion group headings | A [style](theme.md#colors-and-styles). |

Both keys are required in the final merged theme, but inherited from
Catppuccin Mocha if omitted in a custom theme.

## `input.syntax`

Each entry maps a supported zsh-syntax-highlighting style name to either a
style object or the literal string `"none"`:

```toml
[input.syntax]
command = { foreground = "success", bold = true }
double-hyphen-option = { foreground = "text" }
single-quoted-argument = { foreground = "accent" }
unknown-token = { foreground = "error", bold = true }
line = "none"
```

`"none"` explicitly disables that style. A style object accepts `foreground`,
`background`, `bold`, `underline`, and `standout`, exactly as described in the
[style reference](theme.md#colors-and-styles). Palette names and `#RRGGBB`
values both work for foreground/background. Every syntax entry is optional:
omitted entries inherit Catppuccin Mocha's mapping.

ztheme validates the names below and writes the corresponding
`ZSH_HIGHLIGHT_STYLES` entries. The highlighter itself determines which entries
it uses for a particular command line.

### Commands and command positions

```text
alias
arg0
arg0_<identifier>
autodirectory
builtin
command
function
global-alias
hashed-command
precommand
suffix-alias
unknown-token
```

`arg0_<identifier>` is an extensible family for command-specific first-argument
styles. The identifier must contain at least one letter, number, `-`, or `_`.

### Arguments, quoting, and expansion

```text
arithmetic-expansion
back-dollar-quoted-argument
back-double-quoted-argument
back-quoted-argument
back-quoted-argument-delimiter
back-quoted-argument-unclosed
command-substitution
command-substitution-delimiter
command-substitution-delimiter-quoted
command-substitution-delimiter-unquoted
command-substitution-quoted
command-substitution-unquoted
dollar-double-quoted-argument
dollar-quoted-argument
dollar-quoted-argument-unclosed
double-quoted-argument
double-quoted-argument-unclosed
globbing
history-expansion
rc-quote
single-quoted-argument
single-quoted-argument-unclosed
```

### Paths, options, file descriptors, and redirection

```text
double-hyphen-option
named-fd
numeric-fd
path
path_pathseparator
path_prefix
path_prefix_pathseparator
redirection
single-hyphen-option
```

### Shell structure and special tokens

```text
commandseparator
comment
default
process-substitution
process-substitution-delimiter
reserved-word
root
```

### Brackets, cursor, and current line

```text
bracket-error
bracket-level-<N>
cursor
cursor-matchingbracket
line
```

`bracket-level-<N>` accepts any non-empty decimal number, for example
`bracket-level-1` or `bracket-level-12`. It lets nested bracket levels use
different colors.

## Complete example

```toml
[input]
autosuggestion = { foreground = "muted" }
completion = { foreground = "accent" }

[input.syntax]
default = { foreground = "text" }
command = { foreground = "success", bold = true }
builtin = { foreground = "success" }
alias = { foreground = "success" }
path = { foreground = "path", underline = true }
path_pathseparator = { foreground = "path", underline = true }
single-hyphen-option = { foreground = "text" }
double-hyphen-option = { foreground = "text" }
single-quoted-argument = { foreground = "accent" }
double-quoted-argument = { foreground = "accent" }
comment = { foreground = "muted" }
unknown-token = { foreground = "error", bold = true }
bracket-level-1 = { foreground = "path" }
bracket-level-2 = { foreground = "success" }
cursor = { standout = true }
line = "none"
```

The bundled [Catppuccin Mocha theme](../themes/catppuccin-mocha.toml) shows all
fixed names supported by ztheme and is the best starting point for a complete
override.
