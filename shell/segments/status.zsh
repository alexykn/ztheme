# ztheme-segment-v1: status

ztheme_segment_status() {
    emulate -L zsh

    local -i last_status=$1

    if (( last_status == 0 )); then
        if (( ! __ZTHEME_STATUS_SHOW_SUCCESS )); then
            REPLY=""
            return
        fi

        _ztheme_segment_render \
            status \
            "$__ZTHEME_STATUS_SUCCESS_SYMBOL" \
            success
        return
    fi

    _ztheme_segment_render status "$last_status" error
}
