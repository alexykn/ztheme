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

    builtin bindkey '^[[A' history-beginning-search-backward-end
    builtin bindkey '^[[B' history-beginning-search-forward-end
    builtin bindkey '^[OA' history-beginning-search-backward-end
    builtin bindkey '^[OB' history-beginning-search-forward-end
    builtin bindkey '^[[H' beginning-of-line
    builtin bindkey '^[OH' beginning-of-line
    builtin bindkey '^[[F' end-of-line
    builtin bindkey '^[OF' end-of-line
    builtin bindkey '^[[D' backward-char
    builtin bindkey '^[OD' backward-char
    builtin bindkey '^[[C' forward-char
    builtin bindkey '^[OC' forward-char
    builtin bindkey '^[[3~' delete-char
    builtin bindkey '^[[1;5D' backward-word
    builtin bindkey '^[[1;5C' forward-word
    builtin bindkey '^[b' backward-word
    builtin bindkey '^[f' forward-word

    if [[ -r "$__ZTHEME_AUTOSUGGESTIONS" ]] ||
        (( $+functions[_zsh_autosuggest_start] ))
    then
        builtin bindkey '^ ' autosuggest-accept
    fi

    if (( ! ${+VIRTUAL_ENV_DISABLE_PROMPT} )); then
        typeset -gx VIRTUAL_ENV_DISABLE_PROMPT=1
    fi
fi
