# ztheme-segment-v1: clock

zmodload -F zsh/datetime b:strftime || return 1

ztheme_segment_clock() {
    emulate -L zsh

    local value
    strftime -s value '%H:%M'
    _ztheme_segment_render clock "$value"
}
