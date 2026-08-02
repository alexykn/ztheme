# ztheme-segment-v1: character

ztheme_segment_character() {
    emulate -L zsh

    local -i last_status=$1

    if (( last_status == 0 )); then
        _ztheme_segment_render \
            character \
            "$__ZTHEME_CHARACTER_SUCCESS_SYMBOL" \
            success
        return
    fi

    _ztheme_segment_render \
        character \
        "$__ZTHEME_CHARACTER_ERROR_SYMBOL" \
        error
}
