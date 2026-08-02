# ztheme shell integration
#
# Synchronous prompt segments (directory, command status, prompt character,
# user custom segments) are computed in Zsh during `precmd` through the
# generic `_ztheme_compute_sync_segments` dispatcher and held. Git and runtime
# values are styled in Rust and arrive as finished fragments from the per-shell
# `ztheme __client-daemon` process. The complete prompt is assembled and drawn
# in one atomic redraw once those fragments finish or the shared deadline
# expires.

_ztheme_initialize() {
autoload -Uz colors
autoload -Uz add-zsh-hook
autoload -Uz add-zle-hook-widget
autoload -Uz is-at-least

colors
setopt PROMPT_SUBST

# The client spawn relies on zsh/system's sysopen with close-on-exec so the
# prompt descriptors cannot leak into external commands or keep the client
# alive after the shell dies.
if (( $+functions[is-at-least] )) && ! is-at-least 5.9 "$ZSH_VERSION"; then
    print -u2 -- "ztheme: requires Zsh 5.9 or newer (running $ZSH_VERSION)"
    return 1
fi
if ! zmodload zsh/system 2>/dev/null; then
    print -u2 -- "ztheme: requires the zsh/system module"
    return 1
fi

if (( $+functions[_ztheme_stop_client] )); then
    { _ztheme_stop_client } 2>/dev/null
fi

# ---------------------------------------------------------------------------
# Compiled theme
# ---------------------------------------------------------------------------

@ZTHEME_COMPILED_THEME@

# ---------------------------------------------------------------------------
# Synchronous segment implementations
#
# Bundled segments are embedded assets shipped inside the binary. Custom
# segments were validated and allowlisted by the ztheme binary and are sourced
# here, once, during initialization; nothing in this section runs per prompt.
#
# Custom definitions are emitted first and bundled definitions afterward so
# bundled segment functions win under ordinary redefinition.
# ---------------------------------------------------------------------------

@ZTHEME_CUSTOM_SEGMENTS@
@ZTHEME_BUNDLED_SEGMENTS@

# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------

typeset -g __ZTHEME_BIN=@ZTHEME_BIN@
typeset -ga __ZTHEME_INSTANCE_ARGS=(@ZTHEME_INSTANCE_ARGS@)
typeset -g __ZTHEME_AUTOSUGGESTIONS=@ZTHEME_AUTOSUGGESTIONS@
typeset -g __ZTHEME_SYNTAX_HIGHLIGHTING=@ZTHEME_SYNTAX_HIGHLIGHTING@
typeset -g ZTHEME_PROMPT=""
typeset -g ZTHEME_RPROMPT=""
typeset -g ZTHEME_CONTEXT_KEY=""
typeset -g ZTHEME_LAST_ERROR=""
typeset -gi ZTHEME_GENERATION=0
typeset -gi ZTHEME_ASYNC_PENDING=0
typeset -g ZTHEME_CLIENT_PID=""
typeset -gi ZTHEME_CLIENT_READY=0
typeset -gi ZTHEME_REQ_FD=-1
typeset -gi ZTHEME_RESP_FD=-1
typeset -gi ZTHEME_FIFO_SEQUENCE=0
typeset -gi ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN=${ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN:-0}
typeset -gi ZTHEME_SYNTAX_WARNING_SHOWN=${ZTHEME_SYNTAX_WARNING_SHOWN:-0}
typeset -g ZSH_AUTOSUGGEST_MANUAL_REBIND=1
typeset -g __ZTHEME_TERM_ENTER="${__ZTHEME_TERM_ENTER:-}"
typeset -g __ZTHEME_TERM_LEAVE="${__ZTHEME_TERM_LEAVE:-}"
typeset -g __ZTHEME_FOCUS_ENTER=""
typeset -g __ZTHEME_FOCUS_LEAVE=""

if [[ "${ZTHEME_FOCUS_REPORTING:-1}" != 0 &&
      "${TERM:-dumb}" != dumb ]]
then
    __ZTHEME_FOCUS_ENTER=$'\e[?1004h\e[?25h'
    __ZTHEME_FOCUS_LEAVE=$'\e[?1004l\e[?25h'
fi

@ZTHEME_SHELL_DEFAULTS@

_ztheme_start_client() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    (( __ZTHEME_HAS_ASYNC )) || return 1
    [[ -x "$__ZTHEME_BIN" ]] || return 1

    local base="${TMPDIR:-/tmp}"
    local -i sequence=$(( ++ZTHEME_FIFO_SEQUENCE ))
    local prefix="$base/ztheme-$UID-$$-$sequence"
    local request="$prefix.req"
    local response="$prefix.resp"
    local -i request_client_fd=-1
    local -i response_bootstrap_fd=-1
    local -i response_client_fd=-1

    command rm -f "$request" "$response" 2>/dev/null
    if ! command mkfifo -m 600 "$request" "$response" 2>/dev/null; then
        return 1
    fi

    # Every descriptor is opened with close-on-exec so it cannot leak into
    # the client (which must not inherit a request writer, or EOF on its
    # stdin would never arrive) or into ordinary external commands. sysopen
    # reports open failures through its exit status, which commandless
    # `exec` redirections do not. Holding an O_RDWR endpoint on each FIFO
    # until both ends are open keeps every other open below from blocking.
    if ! sysopen -r -w -o cloexec -u ZTHEME_REQ_FD "$request" 2>/dev/null; then
        command rm -f "$request" "$response" 2>/dev/null
        return 1
    fi
    if ! sysopen -r -o cloexec -u request_client_fd "$request" 2>/dev/null; then
        _ztheme_close_client_fds "$request_client_fd" "$response_bootstrap_fd" "$response_client_fd"
        command rm -f "$request" "$response" 2>/dev/null
        return 1
    fi
    if ! sysopen -r -w -o cloexec -u response_bootstrap_fd "$response" 2>/dev/null; then
        _ztheme_close_client_fds "$request_client_fd" "$response_bootstrap_fd" "$response_client_fd"
        command rm -f "$request" "$response" 2>/dev/null
        return 1
    fi
    if ! sysopen -w -o cloexec -u response_client_fd "$response" 2>/dev/null; then
        _ztheme_close_client_fds "$request_client_fd" "$response_bootstrap_fd" "$response_client_fd"
        command rm -f "$request" "$response" 2>/dev/null
        return 1
    fi
    if ! sysopen -r -o cloexec -u ZTHEME_RESP_FD "$response" 2>/dev/null; then
        _ztheme_close_client_fds "$request_client_fd" "$response_bootstrap_fd" "$response_client_fd"
        command rm -f "$request" "$response" 2>/dev/null
        return 1
    fi

    # All endpoints are open. Drop the temporary bootstrap and unlink the
    # paths so a shell killed with SIGKILL leaves no entries in $TMPDIR, and
    # the predictable names cannot be interfered with.
    exec {response_bootstrap_fd}>&-
    response_bootstrap_fd=-1
    command rm -f "$request" "$response" 2>/dev/null

    # Spawn the client from the already-open endpoints: it receives the
    # request read end as stdin and the response write end as stdout. Every
    # other descriptor is close-on-exec and disappears during exec, so the
    # client never holds its own request writer. The shell passes its own PID
    # so the client can detect reparenting as a fallback to stdin EOF.
    local ztheme_bg_nice="$options[bgnice]"
    unsetopt BG_NICE
    command "$__ZTHEME_BIN" __client-daemon \
        --shell-pid "$$" \
        --theme "$__ZTHEME_ASYNC_THEME" \
        "${__ZTHEME_INSTANCE_ARGS[@]}" \
        <&"$request_client_fd" >&"$response_client_fd" 2>/dev/null &!
    [[ "$ztheme_bg_nice" == on ]] && setopt BG_NICE
    unset ztheme_bg_nice

    ZTHEME_CLIENT_PID=$!
    ZTHEME_CLIENT_READY=0
    if (( ZTHEME_CLIENT_PID <= 0 )) ||
        ! kill -0 "$ZTHEME_CLIENT_PID" 2>/dev/null
    then
        _ztheme_close_client_fds "$request_client_fd" "$response_bootstrap_fd" "$response_client_fd"
        return 1
    fi

    # The client holds the endpoints as its stdin/stdout; close the shell's
    # copies so the descriptors are not inherited twice.
    exec {request_client_fd}<&-
    request_client_fd=-1
    exec {response_client_fd}>&-
    response_client_fd=-1

    if builtin zle -F "$ZTHEME_RESP_FD" _ztheme_async_callback 2>/dev/null; then
        ZTHEME_CLIENT_READY=1
    fi
    return 0
}

# Closes the descriptors opened by _ztheme_start_client on its failure paths.
# The three client-side endpoint copies are passed as arguments because they
# are local to the caller; ZTHEME_REQ_FD and ZTHEME_RESP_FD are global. Each
# fd is only closed when its variable is non-negative, which sysopen sets
# exclusively on success.
_ztheme_close_client_fds() {
    emulate -L zsh

    local -i request_client_fd="${1:--1}"
    local -i response_bootstrap_fd="${2:--1}"
    local -i response_client_fd="${3:--1}"
    (( request_client_fd >= 0 )) && { exec {request_client_fd}<&-; } 2>/dev/null
    (( response_bootstrap_fd >= 0 )) && { exec {response_bootstrap_fd}>&-; } 2>/dev/null
    (( response_client_fd >= 0 )) && { exec {response_client_fd}>&-; } 2>/dev/null
    if (( ZTHEME_REQ_FD >= 0 )); then
        { exec {ZTHEME_REQ_FD}<&-; } 2>/dev/null
        ZTHEME_REQ_FD=-1
    fi
    if (( ZTHEME_RESP_FD >= 0 )); then
        { exec {ZTHEME_RESP_FD}<&-; } 2>/dev/null
        ZTHEME_RESP_FD=-1
    fi
    return 0
}

_ztheme_stop_client() {
    emulate -L zsh

    local -i client_pid="${ZTHEME_CLIENT_PID:-0}"
    local -i request_fd="${ZTHEME_REQ_FD:--1}"
    local -i response_fd="${ZTHEME_RESP_FD:--1}"
    ZTHEME_CLIENT_PID=""
    ZTHEME_CLIENT_READY=0
    ZTHEME_ASYNC_PENDING=0
    ZTHEME_REQ_FD=-1
    ZTHEME_RESP_FD=-1

    # Unregister the callback, then close the request writer first so the
    # client sees stdin EOF and exits on its own; the explicit kill is a
    # final guarantee for a client whose EOF was masked by a leaked writer.
    if (( response_fd >= 0 )); then
        builtin zle -F "$response_fd" 2>/dev/null
    fi
    if (( request_fd >= 0 )); then
        exec {request_fd}<&-
    fi
    if (( response_fd >= 0 )); then
        exec {response_fd}<&-
    fi
    if (( client_pid > 0 )); then
        kill "$client_pid" 2>/dev/null
    fi
    return 0
}

_ztheme_async_callback() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    local -i fd=$1
    local protocol generation kind segment fragment
    local -i redraw=0

    if ! IFS=$'\t' read -r -u "$fd" \
        protocol generation kind segment fragment; then
        _ztheme_stop_client
        _ztheme_render_layout
        builtin zle reset-prompt
        return
    fi

    # Records from superseded generations are ignored, never rendered; the
    # client daemon itself is long-lived, so nothing is torn down here.
    if [[ "$protocol" != ZTHEME1 ||
          "$generation" != "$ZTHEME_GENERATION" ]]; then
        return
    fi

    case "$kind" in
        segment)
            ZTHEME_LAST_ERROR=""
            _ztheme_assign_async_segment "$segment" "$fragment"
            ;;
        error)
            _ztheme_clear_async_segments
            if [[ -n "$fragment" && "$fragment" != "$ZTHEME_LAST_ERROR" ]]; then
                ZTHEME_LAST_ERROR="$fragment"
                builtin zle -M "ztheme: $fragment"
            fi
            ;;
        done)
            redraw=1
            ;;
        *)
            return
            ;;
    esac

    if (( redraw )); then
        ZTHEME_ASYNC_PENDING=0
        _ztheme_render_layout
        builtin zle reset-prompt
    fi
}

_ztheme_start_worker() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    (( ++ZTHEME_GENERATION ))
    _ztheme_clear_async_segments
    (( __ZTHEME_HAS_ASYNC )) || return 1
    [[ -x "$__ZTHEME_BIN" ]] || return 1

    if [[ -z "$ZTHEME_CLIENT_PID" ]] ||
        ! kill -0 "$ZTHEME_CLIENT_PID" 2>/dev/null
    then
        _ztheme_stop_client
        _ztheme_start_client || return 1
    fi
    if (( ! ZTHEME_CLIENT_READY )); then
        if builtin zle -F "$ZTHEME_RESP_FD" _ztheme_async_callback 2>/dev/null; then
            ZTHEME_CLIENT_READY=1
        else
            # Without a registered callback nobody will ever consume the
            # response records, so the prompt would stay empty; stop the
            # client and let the caller render without async segments.
            _ztheme_stop_client
            return 1
        fi
    fi

    local -i generation=$ZTHEME_GENERATION
    # zsh strings can hold NUL bytes, and backslash continuations would join
    # separate arguments, so the NUL-delimited request is assembled with
    # incremental appends, one assignment per line.
    local request_line="ZTREQ"$'\0'"2"$'\0'"$generation"$'\0'"$PWD"$'\0'
    request_line+="${PATH:-}"$'\0'"${HOME:-}"$'\0'
    request_line+="${GIT_DIR:-}"$'\0'"${GIT_WORK_TREE:-}"$'\0'
    request_line+="${GIT_CEILING_DIRECTORIES:-}"$'\0'
    request_line+="${VIRTUAL_ENV:-}"$'\0'"${CONDA_PREFIX:-}"$'\0'
    request_line+="${CONDA_DEFAULT_ENV:-}"$'\0'
    request_line+="${PERLBREW_PERL:-}"$'\0'"${PLENV_VERSION:-}"$'\0'
    request_line+="${PYENV_VERSION:-}"$'\0'"${PYENV_DIR:-}"$'\0'
    request_line+="${RUSTUP_TOOLCHAIN:-}"$'\0'"${RUSTUP_HOME:-}"$'\0'
    request_line+="${RBENV_DIR:-}"$'\0'"${RBENV_VERSION:-}"$'\0'
    request_line+="${NODENV_VERSION:-}"$'\0'"${NODENV_DIR:-}"$'\0'
    request_line+="${PLENV_DIR:-}"$'\0'"${RUBY_VERSION:-}"$'\0'
    request_line+="${JAVA_HOME:-}"$'\0'"${GOTOOLCHAIN:-}"$'\0'"${DOTNET_ROOT:-}"$'\0'
    if ! print -rn -- "$request_line" >&"$ZTHEME_REQ_FD" 2>/dev/null; then
        _ztheme_stop_client
        return 1
    fi

    ZTHEME_ASYNC_PENDING=1
    ZTHEME_PROMPT=""
    ZTHEME_RPROMPT=""
    return 0
}

# ---------------------------------------------------------------------------
# Immediate prompt state
# ---------------------------------------------------------------------------

# Wraps a computed segment value in its theme-provided styling. The OPEN/CLOSE
# style maps are generated by the theme compiler keyed by `id:variant`; the
# value is the segment's prompt fragment and is not escaped.
_ztheme_segment_render() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    local id=$1
    local value=$2
    local variant=${3:-default}
    local key="${id}:${variant}"

    REPLY="${__ZTHEME_SEGMENT_OPEN[$key]:-}"
    REPLY+="$value"
    REPLY+="${__ZTHEME_SEGMENT_CLOSE[$key]:-}"
}

# Computes every synchronous segment present in the active layout exactly
# once, in deterministic layout order. Each segment function receives the
# previous command status as $1 and reports its value through $REPLY, which is
# then stored in its ZTHEME_SEGMENT_<ID> state variable. A missing function or
# a failed segment leaves an empty fragment for that prompt only.
_ztheme_compute_sync_segments() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    local -i last_status=$1
    local id function_name variable_name

    for id in "${__ZTHEME_SYNC_SEGMENTS[@]}"; do
        function_name="ztheme_segment_${id}"
        variable_name="ZTHEME_SEGMENT_${(U)id}"
        REPLY=""

        if (( ! $+functions[$function_name] )); then
            # Initialization should already have validated this.
            typeset -g "$variable_name="
            continue
        fi

        if ! "$function_name" "$last_status"; then
            REPLY=""
        fi

        typeset -g "$variable_name=$REPLY"
    done
}

_ztheme_precmd() {
    local -i last_status=$?

    emulate -L zsh
    setopt localoptions no_shwordsplit

    local -i worker_started=0
    _ztheme_start_worker && worker_started=1

    _ztheme_compute_sync_segments "$last_status"

    local context_key
    context_key="$PWD|${GIT_DIR:-}|${GIT_WORK_TREE:-}"
    context_key+="|${VIRTUAL_ENV:-}|${CONDA_PREFIX:-}"
    context_key+="|${CONDA_DEFAULT_ENV:-}|${PERLBREW_PERL:-}|${PLENV_VERSION:-}"
    context_key+="|${PYENV_VERSION:-}|${PYENV_DIR:-}"
    context_key+="|${RUSTUP_TOOLCHAIN:-}|${RUSTUP_HOME:-}"
    context_key+="|${RBENV_DIR:-}|${RBENV_VERSION:-}"
    context_key+="|${NODENV_VERSION:-}|${NODENV_DIR:-}|${PLENV_DIR:-}"
    context_key+="|${RUBY_VERSION:-}|${JAVA_HOME:-}|${GOTOOLCHAIN:-}|${DOTNET_ROOT:-}"
    context_key+="|${NVM_BIN:-}|$PATH"

    if [[ "$context_key" != "$ZTHEME_CONTEXT_KEY" ]]; then
        ZTHEME_CONTEXT_KEY="$context_key"
        _ztheme_clear_async_segments
        ZTHEME_LAST_ERROR=""
    fi

    if (( ! worker_started )); then
        _ztheme_render_layout
    fi
}

_ztheme_load_shell_plugins() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    precmd_functions=(
        ${precmd_functions:#_ztheme_load_shell_plugins}
    )

    if (( ! $+functions[_zsh_autosuggest_start] )); then
        if [[ -r "$__ZTHEME_AUTOSUGGESTIONS" ]]; then
            builtin source "$__ZTHEME_AUTOSUGGESTIONS"
        fi
    fi

    if (( $+functions[_zsh_autosuggest_start] )); then
        add-zle-hook-widget -D line-init \
            _ztheme_initialize_autosuggestions 2>/dev/null
        add-zle-hook-widget line-init _ztheme_initialize_autosuggestions
    elif (( ! ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN )); then
        builtin print -u2 -r -- \
            "ztheme: autosuggestions are unavailable; run \`ztheme setup\` to install them."
        ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN=1
    fi

    if [[ -n "${ZSH_HIGHLIGHT_VERSION:-}" ]] ||
        (( $+functions[_zsh_highlight] ))
    then
        return 0
    fi

    if [[ -r "$__ZTHEME_SYNTAX_HIGHLIGHTING" ]]; then
        if builtin source "$__ZTHEME_SYNTAX_HIGHLIGHTING" &&
            { [[ -n "${ZSH_HIGHLIGHT_VERSION:-}" ]] ||
                (( $+functions[_zsh_highlight] )) }
        then
            return 0
        fi
    fi

    if (( ! ZTHEME_SYNTAX_WARNING_SHOWN )); then
        builtin print -u2 -r -- \
            "ztheme: syntax highlighting is unavailable; run \`ztheme setup\` to install it."
        ZTHEME_SYNTAX_WARNING_SHOWN=1
    fi
    return 0
}

_ztheme_initialize_autosuggestions() {
    emulate -L zsh

    add-zle-hook-widget -D line-init \
        _ztheme_initialize_autosuggestions 2>/dev/null
    _zsh_autosuggest_start

    if (( $+widgets[autosuggest-accept] )); then
        return 0
    fi

    if (( ! ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN )); then
        builtin zle -M \
            "ztheme: autosuggestions failed to initialize; run \`ztheme setup\` to reinstall them."
        ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN=1
    fi
    return 0
}

_ztheme_preexec() {
    emulate -L zsh

    (( ++ZTHEME_GENERATION ))
}

_ztheme_chpwd() {
    emulate -L zsh

    (( ++ZTHEME_GENERATION ))
    ZTHEME_CONTEXT_KEY=""
    _ztheme_clear_async_segments
}

_ztheme_zshexit() {
    emulate -L zsh

    _ztheme_stop_client
    if [[ -o interactive &&
          -n "$__ZTHEME_TERM_LEAVE$__ZTHEME_FOCUS_LEAVE" ]]
    then
        builtin printf '%s%s' \
            "$__ZTHEME_TERM_LEAVE" "$__ZTHEME_FOCUS_LEAVE"
    fi
}

# ---------------------------------------------------------------------------
# Public command bridge
# ---------------------------------------------------------------------------

ztheme() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    if [[ "$1" == theme && "$2" == apply ]]; then
        if (( $# != 3 )); then
            command "$__ZTHEME_BIN" "$@"
            return
        fi

        local integration
        integration="$(
            command "$__ZTHEME_BIN" __theme-apply-zsh \
                --theme "$3" "${__ZTHEME_INSTANCE_ARGS[@]}"
        )" || return
        builtin eval "$integration" || return
        builtin print -r -- \
            "Applied the selected theme to this shell and future shells."
        return
    fi

    if [[ "$1" == theme && "$2" == reload ]]; then
        if (( $# != 2 )); then
            command "$__ZTHEME_BIN" "$@"
            return
        fi

        local integration
        integration="$(
            command "$__ZTHEME_BIN" __theme-reload-zsh \
                --theme "$__ZTHEME_THEME_SELECTOR" \
                "${__ZTHEME_INSTANCE_ARGS[@]}"
        )" || return
        builtin eval "$integration" || return
        builtin print -r -- "Reloaded the current theme."
        return
    fi

    command "$__ZTHEME_BIN" "$@"
}

# ---------------------------------------------------------------------------
# Terminal state
# ---------------------------------------------------------------------------

_ztheme_focus_in() {
    emulate -L zsh

    builtin printf '\e[?25h'
    REPLY=""
    ztheme_segment_directory
    typeset -g ZTHEME_SEGMENT_DIRECTORY="$REPLY"
    _ztheme_render_layout
    builtin zle reset-prompt
}

_ztheme_focus_out() {
    emulate -L zsh
    builtin printf '\e[?25l'
}

_ztheme_zle_line_init() {
    emulate -L zsh
    builtin printf '%s%s' \
        "$__ZTHEME_TERM_ENTER" "$__ZTHEME_FOCUS_ENTER"
}

_ztheme_zle_line_finish() {
    emulate -L zsh
    builtin printf '%s%s' \
        "$__ZTHEME_TERM_LEAVE" "$__ZTHEME_FOCUS_LEAVE"
}

# ---------------------------------------------------------------------------
# Hooks
# ---------------------------------------------------------------------------

precmd_functions=(${precmd_functions:#_ztheme_precmd})
precmd_functions=(${precmd_functions:#_ztheme_load_shell_plugins})
precmd_functions=(_ztheme_precmd $precmd_functions)
precmd_functions+=(_ztheme_load_shell_plugins)

for hook_spec in \
    preexec:_ztheme_preexec \
    chpwd:_ztheme_chpwd \
    zshexit:_ztheme_zshexit
do
    hook="${hook_spec%%:*}"
    function="${hook_spec#*:}"
    add-zsh-hook -D "$hook" "$function" 2>/dev/null
    add-zsh-hook "$hook" "$function"
done
unset hook_spec hook function

if [[ -n "$__ZTHEME_FOCUS_ENTER" &&
      ! ${ZTHEME_FOCUS_BINDINGS_INITIALIZED:-0} -eq 1 ]]
then
    typeset -gi ZTHEME_FOCUS_BINDINGS_INITIALIZED=1
    builtin zle -N _ztheme_focus_in
    builtin zle -N _ztheme_focus_out

    local ztheme_focus_keymap
    for ztheme_focus_keymap in emacs viins vicmd; do
        builtin bindkey -M "$ztheme_focus_keymap" \
            $'\e[I' _ztheme_focus_in \
            $'\e[O' _ztheme_focus_out
    done
    unset ztheme_focus_keymap
fi

add-zle-hook-widget -D line-init _ztheme_zle_line_init 2>/dev/null
add-zle-hook-widget -D line-finish _ztheme_zle_line_finish 2>/dev/null
if [[ -n "$__ZTHEME_TERM_ENTER$__ZTHEME_FOCUS_ENTER" ]]; then
    add-zle-hook-widget line-init _ztheme_zle_line_init
fi
if [[ -n "$__ZTHEME_TERM_LEAVE$__ZTHEME_FOCUS_LEAVE" ]]; then
    add-zle-hook-widget line-finish _ztheme_zle_line_finish
fi

PROMPT='${ZTHEME_PROMPT}'
RPROMPT='${ZTHEME_RPROMPT}'

if (( __ZTHEME_HAS_ASYNC )); then
    local ztheme_bg_nice="$options[bgnice]"
    unsetopt BG_NICE
    command "$__ZTHEME_BIN" __daemon \
        "${__ZTHEME_INSTANCE_ARGS[@]}" >/dev/null 2>&1 &!
    _ztheme_start_client
    [[ "$ztheme_bg_nice" == on ]] && setopt BG_NICE
    unset ztheme_bg_nice
fi

typeset -g ZTHEME_SHELL_INITIALIZED=1
}

if ! _ztheme_initialize; then
    unfunction _ztheme_initialize
    return 1
fi
unfunction _ztheme_initialize
