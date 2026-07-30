# ztheme shell integration
#
# Immediate prompt state is rendered in Zsh. Git and runtime values are styled
# in Rust and arrive as finished fragments through the hidden asynchronous
# `ztheme __snapshot` protocol.

_ztheme_initialize() {
autoload -Uz colors
autoload -Uz add-zsh-hook
autoload -Uz add-zle-hook-widget

colors
setopt PROMPT_SUBST

(( $+functions[_ztheme_close_worker] )) && _ztheme_close_worker

# ---------------------------------------------------------------------------
# Compiled theme
# ---------------------------------------------------------------------------

@ZTHEME_COMPILED_THEME@

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
typeset -gi ZTHEME_ASYNC_FD=-1
typeset -gi ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN=${ZTHEME_AUTOSUGGESTIONS_WARNING_SHOWN:-0}
typeset -gi ZTHEME_SYNTAX_WARNING_SHOWN=${ZTHEME_SYNTAX_WARNING_SHOWN:-0}
typeset -g ZSH_AUTOSUGGEST_MANUAL_REBIND=1

@ZTHEME_SHELL_DEFAULTS@

_ztheme_close_worker() {
    emulate -L zsh

    local -i fd=$ZTHEME_ASYNC_FD
    (( fd >= 0 )) || return

    builtin zle -F "$fd" 2>/dev/null
    exec {fd}<&- 2>/dev/null
    ZTHEME_ASYNC_FD=-1
}

_ztheme_async_callback() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    local -i fd=$1
    local protocol generation kind segment fragment
    local -i redraw=0

    if ! IFS=$'\t' read -r -u "$fd" \
        protocol generation kind segment fragment; then
        _ztheme_close_worker
        return
    fi

    if [[ "$protocol" != ZTHEME1 ||
          "$generation" != "$ZTHEME_GENERATION" ]]; then
        _ztheme_close_worker
        return
    fi

    case "$kind" in
        segment)
            ZTHEME_LAST_ERROR=""
            _ztheme_assign_async_segment "$segment" "$fragment"
            case $? in
                0) redraw=1 ;;
                1) ;;
                *)
                    _ztheme_close_worker
                    return
                    ;;
            esac
            ;;
        error)
            _ztheme_clear_async_segments
            redraw=1
            if [[ -n "$fragment" && "$fragment" != "$ZTHEME_LAST_ERROR" ]]; then
                ZTHEME_LAST_ERROR="$fragment"
                builtin zle -M "ztheme: $fragment"
            fi
            ;;
        done)
            _ztheme_close_worker
            ;;
        *)
            _ztheme_close_worker
            return
            ;;
    esac

    if (( redraw )); then
        _ztheme_render_layout
        builtin zle reset-prompt
    fi
}

_ztheme_start_worker() {
    emulate -L zsh
    setopt localoptions no_shwordsplit

    _ztheme_close_worker
    (( ++ZTHEME_GENERATION ))
    (( __ZTHEME_HAS_ASYNC )) || return
    [[ -x "$__ZTHEME_BIN" ]] || return

    local -i generation=$ZTHEME_GENERATION
    local cwd="$PWD"
    exec {ZTHEME_ASYNC_FD}< <(
        command "$__ZTHEME_BIN" __snapshot \
            --generation "$generation" \
            --cwd "$cwd" \
            --theme "$__ZTHEME_ASYNC_THEME" \
            "${__ZTHEME_INSTANCE_ARGS[@]}" 2>/dev/null
    )

    if ! builtin zle -F "$ZTHEME_ASYNC_FD" \
        _ztheme_async_callback 2>/dev/null
    then
        _ztheme_close_worker
    fi
}

# ---------------------------------------------------------------------------
# Immediate prompt state
# ---------------------------------------------------------------------------

_ztheme_format_directory() {
    emulate -L zsh

    local directory
    local -i budget=$(( ${COLUMNS:-80} * __ZTHEME_DIRECTORY_PERCENT / 100 ))

    (( budget < __ZTHEME_DIRECTORY_MINIMUM )) &&
        budget=$__ZTHEME_DIRECTORY_MINIMUM
    (( budget > __ZTHEME_DIRECTORY_MAXIMUM )) &&
        budget=$__ZTHEME_DIRECTORY_MAXIMUM

    if [[ "$PWD" == "$HOME" ]]; then
        directory="$__ZTHEME_DIRECTORY_HOME"
    elif [[ "$PWD" == "$HOME"/* ]]; then
        directory="${__ZTHEME_DIRECTORY_HOME}/${PWD#$HOME/}"
    else
        directory="$PWD"
    fi

    directory="${(V)directory}"
    directory="${directory//\%/%%}"
    ZTHEME_SEGMENT_DIRECTORY="${__ZTHEME_DIRECTORY_OPEN}"
    ZTHEME_SEGMENT_DIRECTORY+="%${budget}<${__ZTHEME_DIRECTORY_TRUNCATION}<"
    ZTHEME_SEGMENT_DIRECTORY+="${directory}%<<${__ZTHEME_DIRECTORY_CLOSE}"
}

_ztheme_format_status() {
    emulate -L zsh

    local -i last_status=$1
    if (( last_status == 0 )); then
        ZTHEME_SEGMENT_CHARACTER="$__ZTHEME_CHARACTER_SUCCESS"
    else
        ZTHEME_SEGMENT_CHARACTER="$__ZTHEME_CHARACTER_ERROR"
    fi

    if (( last_status == 0 )); then
        if (( __ZTHEME_STATUS_SHOW_SUCCESS )); then
            ZTHEME_SEGMENT_STATUS="$__ZTHEME_STATUS_SUCCESS"
        else
            ZTHEME_SEGMENT_STATUS=""
        fi
        return
    fi

    ZTHEME_SEGMENT_STATUS="${__ZTHEME_STATUS_OPEN}${last_status}"
    ZTHEME_SEGMENT_STATUS+="${__ZTHEME_STATUS_CLOSE}"
}

_ztheme_precmd() {
    local -i last_status=$?

    emulate -L zsh
    setopt localoptions no_shwordsplit

    _ztheme_format_directory
    _ztheme_format_status "$last_status"

    local context_key
    context_key="$PWD|${GIT_DIR:-}|${GIT_WORK_TREE:-}"
    context_key+="|${VIRTUAL_ENV:-}|${CONDA_PREFIX:-}"
    context_key+="|${PERLBREW_PERL:-}|${PLENV_VERSION:-}"
    context_key+="|${RUSTUP_TOOLCHAIN:-}|${RBENV_VERSION:-}"
    context_key+="|${NVM_BIN:-}|$PATH"

    if [[ "$context_key" != "$ZTHEME_CONTEXT_KEY" ]]; then
        ZTHEME_CONTEXT_KEY="$context_key"
        _ztheme_clear_async_segments
        ZTHEME_LAST_ERROR=""
    fi

    _ztheme_render_layout
    _ztheme_start_worker
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

    _ztheme_close_worker
    (( ++ZTHEME_GENERATION ))
}

_ztheme_chpwd() {
    emulate -L zsh

    _ztheme_close_worker
    (( ++ZTHEME_GENERATION ))
    ZTHEME_CONTEXT_KEY=""
    _ztheme_clear_async_segments
}

_ztheme_zshexit() {
    _ztheme_close_worker
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
# Ghostty focus integration
# ---------------------------------------------------------------------------

_ztheme_focus_in() {
    emulate -L zsh

    builtin printf '\e[?25h'
    _ztheme_format_directory
    _ztheme_render_layout
    builtin zle reset-prompt
}

_ztheme_focus_out() {
    emulate -L zsh
    builtin printf '\e[?25l'
}

_ztheme_zle_line_init() {
    emulate -L zsh
    builtin printf '\e[?1004h\e[?25h'
}

_ztheme_zle_line_finish() {
    emulate -L zsh
    builtin printf '\e[?1004l\e[?25h'
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

builtin zle -N _ztheme_focus_in
builtin zle -N _ztheme_focus_out
bindkey $'\e[I' _ztheme_focus_in
bindkey $'\e[O' _ztheme_focus_out

add-zle-hook-widget -D line-init _ztheme_zle_line_init 2>/dev/null
add-zle-hook-widget line-init _ztheme_zle_line_init
add-zle-hook-widget -D line-finish _ztheme_zle_line_finish 2>/dev/null
add-zle-hook-widget line-finish _ztheme_zle_line_finish

PROMPT='${ZTHEME_PROMPT}'
RPROMPT='${ZTHEME_RPROMPT}'

if (( __ZTHEME_HAS_ASYNC )); then
    local ztheme_bg_nice="$options[bgnice]"
    unsetopt BG_NICE
    command "$__ZTHEME_BIN" __daemon \
        "${__ZTHEME_INSTANCE_ARGS[@]}" >/dev/null 2>&1 &!
    [[ "$ztheme_bg_nice" == on ]] && setopt BG_NICE
    unset ztheme_bg_nice
fi

typeset -g ZTHEME_SHELL_INITIALIZED=1
}

_ztheme_initialize
unfunction _ztheme_initialize
