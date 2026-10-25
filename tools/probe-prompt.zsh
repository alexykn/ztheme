#!/usr/bin/env zsh
# Drives real `ztheme __client-daemon` processes the way prompting shells do
# and reports per-prompt latency and errors. Each simulated shell owns one
# client and sends one request per directory per round, waiting for `done`
# before the next, so parallel shells reproduce cross-shell Git contention.
#
# usage: tools/probe-prompt.zsh [-b BINARY] [-n SHELLS] [-r ROUNDS] [-c] [-q] DIR...
#   -b BINARY  ztheme binary to probe (default: build this tree's release)
#   -n SHELLS  simulated shells prompting in parallel (default 1)
#   -r ROUNDS  prompts per directory and shell (default 3)
#   -c         measure a cold daemon: skip the unmeasured warm-up prompt that
#              otherwise starts the daemon and gitstatusd for every DIR
#   -q         print only the summary, not every record
#
# The probe uses the caller's config and managed gitstatusd but a private
# runtime directory, so its server daemon never touches a production or
# development instance; that daemon is stopped on exit.

emulate -LR zsh
setopt errexit nounset pipefail
zmodload zsh/datetime zsh/system

readonly repo_root=${0:A:h:h}
local binary= quiet=0 cold=0
local -i shells=1 rounds=3
while getopts 'b:n:r:cq' option; do
    case $option in
        b) binary=${OPTARG:A} ;;
        n) shells=$OPTARG ;;
        r) rounds=$OPTARG ;;
        c) cold=1 ;;
        q) quiet=1 ;;
        *) print -u2 "usage: $0 [-b BINARY] [-n SHELLS] [-r ROUNDS] [-c] [-q] DIR..."; exit 2 ;;
    esac
done
shift $(( OPTIND - 1 ))
(( $# )) || { print -u2 "usage: $0 [-b BINARY] [-n SHELLS] [-r ROUNDS] [-c] [-q] DIR..."; exit 2 }
local -a directories=(${@:A})

if [[ -z $binary ]]; then
    cargo build --release --quiet --manifest-path "$repo_root/Cargo.toml"
    binary=$repo_root/target/release/ztheme
fi

readonly work=$(mktemp -d /tmp/zt-probe.XXXXXX)
export ZTHEME_RUNTIME_DIR=$work/run
mkdir -m 700 $ZTHEME_RUNTIME_DIR
stop_daemon() {
    local lock pid
    for lock in $ZTHEME_RUNTIME_DIR/*.lock(N); do
        read -r pid < $lock && kill $pid 2>/dev/null || true
    done
    rm -rf -- $work
}
trap stop_daemon EXIT INT TERM

# The generated integration owns the compiled theme and the request field
# order; reuse both rather than restating the request protocol here.
$binary init zsh --dev probe > $work/init.zsh
eval "$(grep '^typeset -g __ZTHEME_ASYNC_THEME=' $work/init.zsh)"
readonly theme=$__ZTHEME_ASYNC_THEME
local -a fields=("${(@f)$(grep -E '^[[:space:]]*request_line\+=' $work/init.zsh | sed 's/^[[:space:]]*//')}")

# One simulated shell: prints `shell gen elapsed-ms record` per record.
probe_shell() {
    local -i shell=$1 rounds=$2 generation=0 round
    local directory line request_line t0
    # The client verifies its parent: this background subshell, not $$.
    # Expand the PID here; coproc arguments expand in the forked child.
    local -i parent=$sysparams[pid]
    coproc exec $binary __client-daemon --shell-pid $parent --theme $theme --dev probe
    for (( round = 1; round <= rounds; round++ )); do
        for directory in $directories; do
            builtin cd -q -- $directory
            (( ++generation ))
            request_line="ZTREQ"$'\0'"3"$'\0'"$generation"$'\0'"$PWD"$'\0'
            for line in $fields; do eval "$line"; done
            t0=$EPOCHREALTIME
            print -rn -- "$request_line" >&p
            while IFS= read -r line <&p; do
                printf '%d %d %.1f %s\n' $shell $generation \
                    $(( (EPOCHREALTIME - t0) * 1000 )) "${(V)line}"
                [[ $line == *$'\t'done ]] && break
            done
        done
    done
    coproc :
}

(( cold )) || probe_shell 0 1 > /dev/null
local -i shell
for (( shell = 1; shell <= shells; shell++ )); do
    probe_shell $shell $rounds > $work/shell-$shell.log &
done
wait

cat $work/shell-*.log(n) > $work/all.log
(( quiet )) || cat $work/all.log
# Summary: Git completion latency per prompt, and every Git error record.
local -i prompts=$(awk '$4 ~ /\\tdone$/' $work/all.log | wc -l)
local -i errors=$(awk '$4 ~ /\\terror\\tgit/' $work/all.log | wc -l)
local -i timeouts=$(awk '$4 ~ /\\terror\\tgit/ && /timed out/' $work/all.log | wc -l)
local -a git_ms=($(awk '$4 ~ /\\tcomplete\\tgit$/ { print $3 }' $work/all.log | sort -n))
local summary="prompts=$prompts git_complete=${#git_ms} git_errors=$errors git_timeouts=$timeouts"
if (( ${#git_ms} )); then
    summary+=" git_ms p50=${git_ms[$(( (${#git_ms} + 1) / 2 ))]}"
    summary+=" p90=${git_ms[$(( ${#git_ms} * 9 / 10 > 0 ? ${#git_ms} * 9 / 10 : 1 ))]} max=${git_ms[-1]}"
fi
print -r -- $summary
