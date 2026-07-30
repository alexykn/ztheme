# ---------------------------------------------------------------------------
# Interactive shell defaults
# ---------------------------------------------------------------------------

if [[ "${ZTHEME_SHELL_DEFAULTS:-1}" != 0 ]] &&
    (( ! ${ZTHEME_SHELL_DEFAULTS_INITIALIZED:-0} ))
then
    typeset -gi ZTHEME_SHELL_DEFAULTS_INITIALIZED=1

    local ztheme_default_histfile="${ZDOTDIR:-$HOME}/.zsh_history"

    # macOS defines this standard path with very small system defaults. A
    # different path is treated as an explicit, user-owned history setup.
    if [[ "${ZTHEME_HISTORY_DEFAULTS:-1}" != 0 ]] &&
        { [[ -z "${HISTFILE+x}" ]] ||
            [[ "$HISTFILE" == "$ztheme_default_histfile" ]] }
    then
        typeset -g HISTFILE="$ztheme_default_histfile"
        typeset -gi HISTSIZE=100000
        typeset -gi SAVEHIST=100000

        builtin setopt EXTENDED_HISTORY
        builtin setopt HIST_EXPIRE_DUPS_FIRST
        builtin setopt HIST_FCNTL_LOCK
        builtin setopt HIST_FIND_NO_DUPS
        builtin setopt HIST_IGNORE_ALL_DUPS
        builtin setopt HIST_IGNORE_DUPS
        builtin setopt HIST_IGNORE_SPACE
        builtin setopt HIST_REDUCE_BLANKS
        builtin setopt HIST_SAVE_NO_DUPS
        builtin setopt HIST_VERIFY
        builtin setopt SHARE_HISTORY
    fi

    builtin setopt AUTO_CD
    builtin setopt AUTO_PUSHD
    builtin setopt PUSHD_IGNORE_DUPS
    builtin setopt PUSHD_SILENT
    builtin setopt INTERACTIVE_COMMENTS
    builtin setopt NO_BEEP

    if (( ! $+functions[compdef] )); then
        builtin autoload -Uz compinit

        local ztheme_zcompdump="${ZDOTDIR:-$HOME}/.zcompdump-${ZSH_VERSION}"
        local -a ztheme_stale_zcompdump
        ztheme_stale_zcompdump=("${ztheme_zcompdump}"(N.mh+24))

        if [[ -f "$ztheme_zcompdump" &&
              ${#ztheme_stale_zcompdump} -eq 0 ]]; then
            compinit -C -d "$ztheme_zcompdump"
        else
            compinit -d "$ztheme_zcompdump"
        fi
    fi

    local ztheme_style
    local -a ztheme_styles

    builtin zstyle -s ':completion:*' menu ztheme_style ||
        builtin zstyle ':completion:*' menu select
    builtin zstyle -a ':completion:*' matcher-list ztheme_styles ||
        builtin zstyle ':completion:*' matcher-list \
            'm:{a-zA-Z}={A-Za-z}' \
            'r:|[._-]=* r:|=*'
    builtin zstyle -s ':completion:*' group-name ztheme_style ||
        builtin zstyle ':completion:*' group-name ''

    if [[ -n "${LS_COLORS:-}" ]]; then
        builtin zstyle -a ':completion:*' list-colors ztheme_styles ||
            builtin zstyle ':completion:*' list-colors "${(s.:.)LS_COLORS}"
    fi

    if (( $+parameters[_comp_options] )) &&
        (( ! ${_comp_options[(Ie)globdots]} ))
    then
        _comp_options+=(globdots)
    fi

    builtin autoload -Uz history-search-end
    builtin zle -N history-beginning-search-backward-end history-search-end
    builtin zle -N history-beginning-search-forward-end history-search-end

    if [[ "${ZTHEME_KEY_BINDINGS:-1}" != 0 &&
          "${TERM:-dumb}" != dumb ]]
    then
        local -i ztheme_has_terminfo=$+parameters[terminfo]
        if (( ! ztheme_has_terminfo )) &&
            builtin zmodload -F zsh/terminfo p:terminfo
        then
            ztheme_has_terminfo=1
        fi

        local -a ztheme_up_keys ztheme_down_keys
        local -a ztheme_home_keys ztheme_end_keys
        local -a ztheme_left_keys ztheme_right_keys ztheme_delete_keys
        local -a ztheme_word_left_keys ztheme_word_right_keys
        local -a ztheme_kill_left_keys ztheme_kill_right_keys

        if (( ztheme_has_terminfo )); then
            [[ -n "${terminfo[kcuu1]-}" ]] &&
                ztheme_up_keys+=("$terminfo[kcuu1]")
            [[ -n "${terminfo[kcud1]-}" ]] &&
                ztheme_down_keys+=("$terminfo[kcud1]")
            [[ -n "${terminfo[khome]-}" ]] &&
                ztheme_home_keys+=("$terminfo[khome]")
            [[ -n "${terminfo[kend]-}" ]] &&
                ztheme_end_keys+=("$terminfo[kend]")
            [[ -n "${terminfo[kcub1]-}" ]] &&
                ztheme_left_keys+=("$terminfo[kcub1]")
            [[ -n "${terminfo[kcuf1]-}" ]] &&
                ztheme_right_keys+=("$terminfo[kcuf1]")
            [[ -n "${terminfo[kdch1]-}" ]] &&
                ztheme_delete_keys+=("$terminfo[kdch1]")

            __ZTHEME_TERM_ENTER="${terminfo[smkx]-}"
            __ZTHEME_TERM_LEAVE="${terminfo[rmkx]-}"
        fi

        # CSI and SS3 fallbacks cover terminals whose terminfo entry describes
        # only one of their normal and application cursor modes.
        ztheme_up_keys+=($'\e[A' $'\eOA')
        ztheme_down_keys+=($'\e[B' $'\eOB')
        ztheme_home_keys+=($'\e[H' $'\eOH' $'\e[1~')
        ztheme_end_keys+=($'\e[F' $'\eOF' $'\e[4~')
        ztheme_left_keys+=($'\e[D' $'\eOD')
        ztheme_right_keys+=($'\e[C' $'\eOC')
        ztheme_delete_keys+=($'\e[3~')

        # Modern xterm modifiers plus the compact rxvt forms. Escape-prefixed
        # b/f and Backspace are what macOS terminals emit when Option is Alt.
        ztheme_word_left_keys+=(
            $'\eb' $'\e[1;3D' $'\e[1;5D' $'\e[5D' $'\eOd'
        )
        ztheme_word_right_keys+=(
            $'\ef' $'\e[1;3C' $'\e[1;5C' $'\e[5C' $'\eOc'
        )
        ztheme_kill_left_keys+=(
            $'\e\x7f' $'\e\x08' $'\e[127;5u' $'\e[8;5u'
        )
        ztheme_kill_right_keys+=($'\e[3;3~' $'\e[3;5~')

        local ztheme_key
        local -a ztheme_bindings
        for ztheme_key in "${ztheme_up_keys[@]}"; do
            ztheme_bindings+=(
                "$ztheme_key" history-beginning-search-backward-end
            )
        done
        for ztheme_key in "${ztheme_down_keys[@]}"; do
            ztheme_bindings+=(
                "$ztheme_key" history-beginning-search-forward-end
            )
        done
        for ztheme_key in "${ztheme_home_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" beginning-of-line)
        done
        for ztheme_key in "${ztheme_end_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" end-of-line)
        done
        for ztheme_key in "${ztheme_left_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" backward-char)
        done
        for ztheme_key in "${ztheme_right_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" forward-char)
        done
        for ztheme_key in "${ztheme_delete_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" delete-char)
        done
        for ztheme_key in "${ztheme_word_left_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" backward-word)
        done
        for ztheme_key in "${ztheme_word_right_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" forward-word)
        done
        for ztheme_key in "${ztheme_kill_left_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" backward-kill-word)
        done
        for ztheme_key in "${ztheme_kill_right_keys[@]}"; do
            ztheme_bindings+=("$ztheme_key" kill-word)
        done

        local ztheme_keymap
        for ztheme_keymap in emacs viins vicmd; do
            builtin bindkey -M "$ztheme_keymap" "${ztheme_bindings[@]}"
        done

        if [[ -r "$__ZTHEME_AUTOSUGGESTIONS" ]] ||
            (( $+functions[_zsh_autosuggest_start] ))
        then
            for ztheme_keymap in emacs viins vicmd; do
                builtin bindkey -M "$ztheme_keymap" '^ ' autosuggest-accept
            done
        fi
    fi

    if (( ! ${+VIRTUAL_ENV_DISABLE_PROMPT} )); then
        typeset -gx VIRTUAL_ENV_DISABLE_PROMPT=1
    fi
fi
