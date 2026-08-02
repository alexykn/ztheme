# ztheme-segment-v1: directory

ztheme_segment_directory() {
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

    local value="%${budget}<${__ZTHEME_DIRECTORY_TRUNCATION}<"
    value+="${directory}%<<"

    _ztheme_segment_render directory "$value"
}
