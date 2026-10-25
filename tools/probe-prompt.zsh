#!/usr/bin/env zsh
# Drives real `ztheme __client-daemon` processes the way prompting shells do
# and reports per-prompt latency and errors. Each simulated shell owns one
# client and sends one request per directory per round, so parallel shells
# reproduce cross-shell Git contention. A shell waits for `done` and, when the
# theme has Git, for the Git group's `complete`, which may follow `done`.
#
# usage: tools/probe-prompt.zsh [-b BINARY] [-n SHELLS] [-r ROUNDS] [-c] [-d] [-q] DIR...
#   -b BINARY  ztheme binary to probe (default: build this tree's release)
#   -n SHELLS  simulated shells prompting in parallel (default 1)
#   -r ROUNDS  prompts per directory and shell (default 3)
#   -c         measure a cold daemon: skip the unmeasured warm-up prompt that
#              otherwise starts the daemon and gitstatusd for every DIR
#   -d         send the next request at `done`, like a user pressing Enter as
#              soon as the prompt appears: a later fresh Git value is
#              superseded, so prompts show the stale one only
#   -q         print only the summary, not every record
#
# The probe uses the caller's config and managed gitstatusd but a private
# runtime directory, so its server daemon never touches a production or
# development instance; that daemon is stopped on exit.

emulate -LR zsh
setopt errexit nounset pipefail
zmodload zsh/datetime zsh/system

readonly repo_root=${0:A:h:h}
local binary= quiet=0 cold=0 at_done=0
local -i shells=1 rounds=3
readonly usage="usage: $0 [-b BINARY] [-n SHELLS] [-r ROUNDS] [-c] [-d] [-q] DIR..."
while getopts 'b:n:r:cdq' option; do
    case $option in
        b) binary=${OPTARG:A} ;;
        n) shells=$OPTARG ;;
        r) rounds=$OPTARG ;;
        c) cold=1 ;;
        d) at_done=1 ;;
        q) quiet=1 ;;
        *) print -u2 $usage; exit 2 ;;
    esac
done
shift $(( OPTIND - 1 ))
(( $# )) || { print -u2 $usage; exit 2 }
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
eval "$(grep -E '^typeset -gi? __ZTHEME_(ASYNC_THEME|HAS_GIT)=' $work/init.zsh)"
readonly theme=$__ZTHEME_ASYNC_THEME
# Without Git in the theme there is no Git completion to wait for.
(( __ZTHEME_HAS_GIT )) || at_done=1
local -a fields=("${(@f)$(grep -E '^[[:space:]]*request_line\+=' $work/init.zsh | sed 's/^[[:space:]]*//')}")

# One simulated shell: prints `shell gen elapsed-ms record` per record.
probe_shell() {
    local -i shell=$1 rounds=$2 generation=0 round done git
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
            done=0 git=$at_done
            # The client's own Git cap is 10 s; never wait much past it.
            while (( ! done || ! git )) && IFS= read -r -t 12 line <&p; do
                printf '%d %d %.1f %s\n' $shell $generation \
                    $(( (EPOCHREALTIME - t0) * 1000 )) "${(V)line}"
                [[ $line == *$'\t'done ]] && done=1
                [[ $line == *$'\t'complete$'\t'git ]] && git=1
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
# Summary per prompt (shell and generation): when its first Git value and
# its Git completion arrived, whether that completion followed `done`, and
# every Git error. A prompt without any Git value lost it.
awk '
    { key = $1 " " $2 }
    $4 ~ /\\tdone$/ { prompts[key] = 1; done_ms[key] = $3 }
    $4 ~ /\\tsegment\\tgit\\t./ && !(key in first) { first[key] = $3 }
    $4 ~ /\\tcomplete\\tgit$/ { complete[key] = $3; if (key in done_ms) late++ }
    $4 ~ /\\terror\\tgit/ { errors++; if (/timed out/) timeouts++ }
    END {
        for (key in prompts) {
            if (!(key in first)) missing++
            if (key in first) print "first", first[key] > "/dev/stderr"
            if (key in complete) print "complete", complete[key] > "/dev/stderr"
        }
        printf "prompts=%d without_git=%d git_complete_after_done=%d git_errors=%d git_timeouts=%d\n",
            length(prompts), missing, late, errors, timeouts
    }
' $work/all.log 2> $work/latency.log
local kind
local -a values
for kind in first complete; do
    values=($(awk -v kind=$kind '$1 == kind { print $2 }' $work/latency.log | sort -n))
    (( ${#values} )) || continue
    print -r -- "git_${kind}_ms n=${#values} p50=${values[$(( (${#values} + 1) / 2 ))]}" \
        "p90=${values[$(( ${#values} * 9 / 10 > 0 ? ${#values} * 9 / 10 : 1 ))]} max=${values[-1]}"
done
