#!/usr/bin/env zsh

emulate -LR zsh
setopt errexit nounset pipefail
unsetopt bgnice
zmodload zsh/datetime

readonly script_dir=$0:A:h
readonly repo_root=$script_dir:h
readonly temp_root=$(mktemp -d "/tmp/zt-runtime-cache-bench.XXXXXX")
trap 'shutdown_all_daemons; /bin/rm -rf -- "$temp_root"' EXIT INT TERM

readonly compiler=$(command -v cc)
readonly measured_prompts=${BENCHMARK_MEASURED_PROMPTS:-1000}
readonly warmup_prompts=${BENCHMARK_WARMUP_PROMPTS:-100}
readonly realistic_prompts=${BENCHMARK_REALISTIC_PROMPTS:-5000}
readonly skip_realistic=${BENCHMARK_SKIP_REALISTIC:-0}
readonly skip_latency=${BENCHMARK_SKIP_LATENCY:-0}
readonly skip_baseline=${BENCHMARK_SKIP_BASELINE:-0}
readonly skip_git=${BENCHMARK_SKIP_GIT:-0}
readonly git_large_files=${BENCHMARK_GIT_LARGE_FILES:-20000}
readonly user_gitstatusd="${XDG_DATA_HOME:-$HOME/.local/share}/ztheme/gitstatus/v1.5/gitstatusd"
typeset -a git_scenarios_measured=()
readonly candidate_target="$temp_root/candidate-target"

print "building current release binary"
cargo build --release --manifest-path "$repo_root/Cargo.toml" \
    --target-dir "$candidate_target" >/dev/null
readonly candidate_binary="$candidate_target/release/ztheme"
if (( skip_baseline )); then
    print "baseline comparison skipped; measuring the current binary only"
    readonly baseline_binary="$candidate_binary"
else
    readonly baseline_source="$temp_root/baseline-source"
    readonly baseline_target="$temp_root/baseline-target"
    readonly baseline_archive="$temp_root/baseline.tar"
    print "building isolated baseline from HEAD"
    git -C "$repo_root" archive HEAD -o "$baseline_archive"
    mkdir -p "$baseline_source"
    tar -xf "$baseline_archive" -C "$baseline_source"
    cargo build --release --manifest-path "$baseline_source/Cargo.toml" \
        --target-dir "$baseline_target" >/dev/null
    readonly baseline_binary="$baseline_target/release/ztheme"
fi
typeset -A realistic_execution_counts realistic_stale_counts realistic_hit_rates
typeset -A warm_p50_us warm_p95_us concurrent_execution_counts

(( ${+commands[awk]} )) || { print -u2 "benchmark requires awk"; exit 1; }
(( ${+commands[sort]} )) || { print -u2 "benchmark requires sort"; exit 1; }

counter_value() {
    local value
    read -r value < "$counter_file"
    print -r -- "$value"
}

median_values() {
    local -a sorted
    sorted=("${(@f)$(printf '%s\n' "$@" | sort -n)}")
    print -r -- "${sorted[$(( (${#sorted} + 1) / 2 ))]}"
}

compile_runtime() {
    local name=$1
    local output=$2
    local delay_ms=$3
    local source="$fixture/$name.c"
    cat > "$source" <<'EOF'
#include <fcntl.h>
#include <stdio.h>
#include <sys/file.h>
#include <unistd.h>
#ifndef COUNTER_PATH
#error COUNTER_PATH is required
#endif
#ifndef OUTPUT
#error OUTPUT is required
#endif
#ifndef DELAY_MS
#define DELAY_MS 0
#endif
int main(void) {
    int fd = open(COUNTER_PATH, O_CREAT | O_RDWR, 0600);
    if (fd < 0 || flock(fd, LOCK_EX) != 0) return 2;
    char value[32] = {0};
    int count = 0;
    if (read(fd, value, sizeof(value) - 1) > 0) sscanf(value, "%d", &count);
    ++count;
    lseek(fd, 0, SEEK_SET);
    ftruncate(fd, 0);
    dprintf(fd, "%d\n", count);
    flock(fd, LOCK_UN);
    close(fd);
    usleep(DELAY_MS * 1000);
    puts(OUTPUT);
    return 0;
}
EOF
    "$compiler" "-DCOUNTER_PATH=\"$counter_file\"" "-DOUTPUT=\"$output\"" \
        "-DDELAY_MS=$delay_ms" "$source" -o "$fixture/bin/$name"
    chmod 700 "$fixture/bin/$name"
}

request_payload() {
    local path=$1
    local generation=$2
    local -A field_value=(
        PATH "$path"
        HOME "$home"
        CONDA_DEFAULT_ENV "$conda_default_env"
        RUSTUP_TOOLCHAIN "$rustup_toolchain"
        GOTOOLCHAIN "$gotoolchain"
    )
    local request_line="ZTREQ"$'\0'"$wire_version"$'\0'"$generation"$'\0'"$project"$'\0'
    # The field order comes from the generated integration (see load_theme), so
    # the payload writer can never drift from the daemon's parser.
    local field
    for field in "${request_fields[@]}"; do
        request_line+="${field_value[$field]:-}"$'\0'
    done
    print -rn -- "$request_line"
}

send_request() {
    local generation=$1
    local path=$2
    last_python_fragment=""
    last_rust_fragment=""
    last_dart_fragment=""
    last_zig_fragment=""
    last_julia_fragment=""
    last_r_fragment=""
    last_response_summary=""
    request_payload "$path" "$generation" >&"${request_fd}"
    local protocol response_generation kind name fragment
    while true; do
        if ! IFS=$'\t' read -r -t 10 -u "${response_fd}" \
            protocol response_generation kind name fragment; then
            print -u2 -- "timed out waiting for generation $generation from $binary"
            return 1
        fi
        [[ "$response_generation" == "$generation" ]] || continue
        if [[ "$kind" == segment && "$name" == python ]]; then
            last_python_fragment=$fragment
        fi
        if [[ "$kind" == segment && "$name" == rust ]]; then
            last_rust_fragment=$fragment
        fi
        if [[ "$kind" == segment && "$name" == dart ]]; then
            last_dart_fragment=$fragment
        fi
        if [[ "$kind" == segment && "$name" == zig ]]; then
            last_zig_fragment=$fragment
        fi
        if [[ "$kind" == segment && "$name" == julia ]]; then
            last_julia_fragment=$fragment
        fi
        if [[ "$kind" == segment && "$name" == r ]]; then
            last_r_fragment=$fragment
        fi
        if [[ "$kind" == segment || "$kind" == error ]]; then
            last_response_summary+="$kind:$name=$fragment;"
        fi
        if [[ "$kind" == done ]]; then
            return 0
        fi
    done
}

start_client() {
    (( ++client_sequence ))
    local request_path="$fixture/request-$client_sequence"
    local response_path="$fixture/response-$client_sequence"
    mkfifo -m 600 "$request_path" "$response_path"
    exec {request_fd}<>"$request_path"
    exec {response_fd}<>"$response_path"
    rm -f -- "$request_path" "$response_path"
    "$binary" __client-daemon \
        --shell-pid "$$" \
        --theme "$theme_hex" \
        --dev "$instance" \
        <&"${request_fd}" >&"${response_fd}" 2>"$fixture/client-$client_sequence.err" &!
    client_pid=$!
}

stop_client() {
    exec {request_fd}>&-
    exec {response_fd}<&-
    kill "$client_pid" 2>/dev/null || true
    wait "$client_pid" 2>/dev/null || true
}

stop_concurrent_clients() {
    local i close_fd
    for i in {1..8}; do
        if [[ -n "${concurrent_request_fds[$i]:-}" ]]; then
            close_fd=${concurrent_request_fds[$i]}
            exec {close_fd}>&-
        fi
        if [[ -n "${concurrent_response_fds[$i]:-}" ]]; then
            close_fd=${concurrent_response_fds[$i]}
            exec {close_fd}<&-
        fi
        if [[ -n "${concurrent_clients[$i]:-}" ]]; then
            kill -KILL "${concurrent_clients[$i]}" 2>/dev/null || true
            wait "${concurrent_clients[$i]}" 2>/dev/null || true
        fi
    done
}

run_concurrent_cold_miss() {
    local cold_bin="$fixture/concurrent-cold-bin"
    local cold_path="$cold_bin/python"
    local before after
    mkdir -p "$cold_bin"
    cp "$fixture/bin/python" "$cold_path"
    chmod 700 "$cold_path"
    warm_executable "$cold_path"
    before=$(counter_value)

    typeset -a concurrent_clients concurrent_request_fds concurrent_response_fds
    local i request_path response_path fd
    for i in {1..8}; do
        request_path="$fixture/concurrent-$i.req"
        response_path="$fixture/concurrent-$i.resp"
        mkfifo -m 600 "$request_path" "$response_path"
        exec {fd}<>"$request_path"
        concurrent_request_fds[$i]=$fd
        exec {fd}<>"$response_path"
        concurrent_response_fds[$i]=$fd
        rm -f -- "$request_path" "$response_path"
        "$binary" __client-daemon \
            --shell-pid "$$" \
            --theme "$theme_hex" \
            --dev "$instance" \
            <&"${concurrent_request_fds[$i]}" \
            >&"${concurrent_response_fds[$i]}" \
            2>"$fixture/concurrent-$i.err" &!
        concurrent_clients[$i]=$!
    done

    for i in {1..8}; do
        request_payload "$cold_bin" 1 >&"${concurrent_request_fds[$i]}"
    done
    local protocol response_generation kind name fragment
    for i in {1..8}; do
        while true; do
            if ! IFS=$'\t' read -r -t 10 -u "${concurrent_response_fds[$i]}" \
                protocol response_generation kind name fragment
            then
                print -u2 -- "$label: timed out waiting for concurrent client $i"
                stop_concurrent_clients
                return 1
            fi
            [[ "$response_generation" == 1 ]] || continue
            [[ "$kind" == done ]] && break
        done
    done
    stop_concurrent_clients
    after=$(counter_value)
    concurrent_execution_counts[$label]=$(( after - before ))
    print -r -- "$label concurrent-cold: clients=8 additional_runtime_execs=${concurrent_execution_counts[$label]}"
    if [[ "$label" == candidate ]] && (( concurrent_execution_counts[$label] != 1 )); then
        print -u2 -- "$label: concurrent cold miss executed ${concurrent_execution_counts[$label]} runtime commands"
        exit 1
    fi
}

load_theme() {
    local theme_name=$1
    local init_output
    init_output=$("$binary" init zsh --theme "$theme_name" --dev "$instance" 2>/dev/null)
    theme_hex=$(print -r -- "$init_output" |
        awk -F"'" '/__ZTHEME_ASYNC_THEME/ { print $2; exit }')
    [[ -n "$theme_hex" ]] || { print -u2 "could not extract compiled theme $theme_name"; exit 1; }

    # Derive the request protocol (version and field order) from the generated
    # integration instead of hand-writing it, so the payload writer can never
    # drift from the daemon's parser.
    wire_version=$(print -r -- "$init_output" |
        awk -F'"' '/ZTREQ/ { print $4; exit }')
    [[ "$wire_version" =~ '^[0-9]+$' ]] || {
        print -u2 "could not extract the request protocol version"; exit 1
    }
    request_fields=($(print -r -- "$init_output" |
        sed -n 's/.*request_line+="\${\([A-Z_][A-Z_]*\):-}".*/\1/p'))
    (( ${#request_fields[@]} >= 23 )) || {
        print -u2 "could not extract the request protocol fields"; exit 1
    }
}

start_scenario() {
    local theme_name=$1
    project=$2
    load_theme "$theme_name"
    start_client
}

warm_executable() {
    local path=$1
    "$path" >/dev/null 2>/dev/null
}

measure_warm_scenario() {
    local scenario=$1
    local theme_name=$2
    local cwd=$3
    local request_path=$4
    local before after started ended generation i run count p50_index p95_index
    local executions opportunities p50 p95
    local -F 6 cold_elapsed
    local -a p50_runs p95_runs samples sorted_samples

    start_scenario "$theme_name" "$cwd"
    generation=1
    before=$(counter_value)
    started=$EPOCHREALTIME
    send_request "$generation" "$request_path"
    ended=$EPOCHREALTIME
    cold_elapsed=$(( (ended - started) * 1000.0 ))
    (( ++generation ))

    for (( i = 1; i <= warmup_prompts; i++ )); do
        send_request "$generation" "$request_path"
        (( ++generation ))
    done

    for run in {1..5}; do
        print -r -- "$label/$scenario: measuring run $run/5 ($measured_prompts prompts)"
        samples=()
        for (( i = 1; i <= measured_prompts; i++ )); do
            started=$EPOCHREALTIME
            send_request "$generation" "$request_path"
            ended=$EPOCHREALTIME
            samples+=($(( (ended - started) * 1000000.0 )))
            (( ++generation ))
        done
        count=${#samples}
        p50_index=$(( (count + 1) / 2 ))
        p95_index=$(( (count * 95 + 99) / 100 ))
        sorted_samples=("${(@f)$(printf '%s\n' "${samples[@]}" | sort -n)}")
        p50_runs+=("${sorted_samples[$p50_index]}")
        p95_runs+=("${sorted_samples[$p95_index]}")
    done
    after=$(counter_value)
    executions=$(( after - before ))
    opportunities=$(( measured_prompts * 5 + warmup_prompts + 1 ))
    if [[ "$scenario" == four-direct ]]; then
        opportunities=$(( opportunities * 4 ))
    fi
    p50=$(median_values "${p50_runs[@]}")
    p95=$(median_values "${p95_runs[@]}")
    warm_p50_us["$label/$scenario"]=$p50
    warm_p95_us["$label/$scenario"]=$p95
    print -r -- "$label $scenario warm-hit: cold_ms=$cold_elapsed p50_us=$p50 p95_us=$p95 measured_prompts=$(( measured_prompts * 5 )) runtime_execs=$executions"
    if [[ "$scenario" == rustup-warm && "$last_rust_fragment" != *1.80.0* ]]; then
        print -u2 -- "$label rustup-warm: expected stable Rust 1.80.0, got $last_response_summary"
        exit 1
    fi
    if [[ "$label" == candidate && "$scenario" == rustup-warm && "$executions" != 1 ]]; then
        print -u2 -- "$label rustup-warm: expected one cold runtime execution, got $executions"
        exit 1
    fi
    if [[ "$label" == candidate ]] && (( executions * 100 >= opportunities )); then
        print -u2 -- "$label $scenario: steady-state hit rate did not exceed 99%"
        exit 1
    fi
    stop_client
}

measure_cold_scenario() {
    local scenario=$1
    local theme_name=$2
    local cwd=$3
    local request_path=$4
    local before after started ended
    local -F 6 elapsed

    start_scenario "$theme_name" "$cwd"
    generation=1
    before=$(counter_value)
    started=$EPOCHREALTIME
    send_request "$generation" "$request_path"
    ended=$EPOCHREALTIME
    elapsed=$(( (ended - started) * 1000.0 ))
    after=$(counter_value)
    print -r -- "$label $scenario cold-miss: elapsed_ms=$elapsed runtime_execs=$(( after - before ))"
    stop_client
}

shutdown_server_process() {
    local socket lock server_pid i
    for socket in "$runtime"/*.sock(N); do
        lock="${socket%.sock}.lock"
        if [[ -r "$lock" ]]; then
            read -r server_pid < "$lock"
            if kill -0 "$server_pid" 2>/dev/null; then
                "$shutdown_helper" "$socket" || true
            else
                /bin/rm -f -- "$socket" "$lock"
            fi
        else
            /bin/rm -f -- "$socket"
        fi
    done
    for (( i = 1; i <= 100; i++ )); do
        local remaining=0
        for socket in "$runtime"/*.sock(N); do
            remaining=1
        done
        (( remaining == 0 )) && return 0
        sleep 0.01
    done
    print -u2 -- "$label: daemon did not stop during restart"
    return 1
}

shutdown_all_daemons() {
    local runtime_dir helper socket lock server_pid
    for runtime_dir in "$temp_root"/*/runtime(N); do
        helper="${runtime_dir:h}/shutdown-daemon"
        [[ -x "$helper" ]] || continue
        for socket in "$runtime_dir"/*.sock(N); do
            lock="${socket%.sock}.lock"
            if [[ -r "$lock" ]]; then
                read -r server_pid < "$lock"
                if kill -0 "$server_pid" 2>/dev/null; then
                    "$helper" "$socket" || true
                else
                    /bin/rm -f -- "$socket" "$lock"
                fi
            else
                /bin/rm -f -- "$socket"
            fi
        done
    done
}

measure_persisted_restart() {
    local cwd=$1
    local request_path=$2
    local before after

    start_scenario bench "$cwd"
    generation=1
    send_request "$generation" "$request_path"
    before=$(counter_value)
    stop_client
    shutdown_server_process

    start_scenario bench "$cwd"
    generation=1
    send_request "$generation" "$request_path"
    after=$(counter_value)
    print -r -- "$label daemon-restart: runtime_execs_before=$before runtime_execs_after=$after"
    (( after == before )) || { print -u2 -- "$label: persisted cache miss after restart"; exit 1; }
    stop_client
}

run_realistic_workload() {
    local realistic_bin=$1
    local pyenv_root=$2
    local replacement=$3
    local julia_replacement=$4
    local before after started ended request_path idx group i
    local stale_results=0 runtime_opportunities=0
    local selector_pending=0
    local -F 6 elapsed
    local -a samples

    start_scenario realistic "${realistic_dirs[1]}"
    generation=1
    before=$(counter_value)
    samples=()
    for (( i = 1; i <= realistic_prompts; i++ )); do
        if (( i == realistic_prompts / 2 + 1 )); then
            stop_client
            shutdown_server_process
            start_scenario realistic "${realistic_dirs[1]}"
            generation=1
        fi

        if (( i == 900 )); then
            : > "${realistic_dirs[1]}/README.md"
        fi
        if (( i == 1200 )); then
            cp "$replacement" "$realistic_bin/python"
            chmod 700 "$realistic_bin/python"
        fi
        if (( i == 1700 )); then
            print -r -- "3.12" > "${realistic_dirs[40]}/.python-version"
            selector_pending=1
        fi
        if (( i == 2405 )); then
            cp "$julia_replacement" "$realistic_bin/julia"
            chmod 700 "$realistic_bin/julia"
        fi

        if (( selector_pending )); then
            idx=40
            selector_pending=0
        elif (( i % 10 < 7 )); then
            idx=$(( i % 8 + 1 ))
        else
            idx=$(( i % 40 + 1 ))
        fi
        project=${realistic_dirs[$idx]}
        group=${realistic_groups[$idx]}
        request_path="$realistic_bin:/usr/bin:/bin"
        if [[ "$group" == pyenv ]]; then
            request_path="$pyenv_root/shims:$request_path"
        fi
        if (( i % 11 == 0 )); then
            request_path+=":$fixture/irrelevant-$(( i % 5 ))"
        fi
        if (( i % 17 == 0 )); then
            conda_default_env="env-$(( i % 3 ))"
        else
            conda_default_env=""
        fi
        runtime_opportunities=$(( runtime_opportunities + realistic_opportunities[$idx] ))
        started=$EPOCHREALTIME
        send_request "$generation" "$request_path"
        ended=$EPOCHREALTIME
        samples+=($(( (ended - started) * 1000000.0 )))
        if (( i == 1200 )) && [[ "$last_python_fragment" != *3.13.0* ]]; then
            print -u2 -- "$label stale result after executable replacement: $last_response_summary"
            (( ++stale_results ))
        fi
        if (( i == 1700 )) && [[ "$last_python_fragment" != *3.12.0* ]]; then
            print -u2 -- "$label stale result after pyenv selector switch: $last_response_summary"
            (( ++stale_results ))
        fi
        if (( i == 2405 )) && [[ "$last_julia_fragment" != *1.12.0* ]]; then
            print -u2 -- "$label stale result after julia executable replacement: $last_response_summary"
            (( ++stale_results ))
        fi
        (( ++generation ))
    done
    after=$(counter_value)
    local executions=$(( after - before ))
    local count=${#samples}
    local p50_index=$(( (count + 1) / 2 ))
    local p95_index=$(( (count * 95 + 99) / 100 ))
    local -a sorted_samples
    sorted_samples=("${(@f)$(printf '%s\n' "${samples[@]}" | sort -n)}")
    local p50=${sorted_samples[$p50_index]}
    local p95=${sorted_samples[$p95_index]}
    local maximum=${sorted_samples[-1]}
    local -F 6 hit_rate=$(( 1.0 - 1.0 * executions / runtime_opportunities ))
    realistic_execution_counts[$label]=$executions
    realistic_stale_counts[$label]=$stale_results
    realistic_hit_rates[$label]=$hit_rate
    print -r -- "$label realistic: prompt_count=$realistic_prompts runtime_opportunities=$runtime_opportunities actual_runtime_execs=$executions cache_hit_rate=$hit_rate p50_us=$p50 p95_us=$p95 maximum_us=$maximum stale_result_count=$stale_results"
    if [[ "$label" == candidate ]] && (( hit_rate <= 0.99 )); then
        print -u2 -- "$label: realistic steady-state hit rate did not exceed 99%"
        exit 1
    fi
    if [[ "$label" == candidate && $stale_results -ne 0 ]]; then
        print -u2 -- "$label: stale result detected"
        exit 1
    fi
    stop_client
}

create_git_repo() {
    local repo_dir=$1
    local directory_count=$2
    local files_per_directory=$3
    local dirty=$4
    local i j
    mkdir -p "$repo_dir"
    git -C "$repo_dir" init -q -b main
    for (( i = 1; i <= directory_count; i++ )); do
        mkdir -p "$repo_dir/dir-$i"
        for (( j = 1; j <= files_per_directory; j++ )); do
            print -r -- "content $i/$j" > "$repo_dir/dir-$i/file-$j.txt"
        done
    done
    git -C "$repo_dir" add -A
    git -C "$repo_dir" -c user.name=bench -c user.email=bench@localhost commit -q -m bench
    if (( dirty )); then
        print -r -- "modified" >> "$repo_dir/dir-1/file-1.txt"
        for (( i = 1; i <= 256; i++ )); do
            print -r -- "untracked" > "$repo_dir/untracked-$i.txt"
        done
    fi
}

measure_build() {
    emulate -L zsh
    setopt errexit nounset pipefail
    unsetopt bgnice
    local label=$1
    binary=$2

    fixture="$temp_root/$label-fixture"
    home="$fixture/home"
    config="$fixture/config"
    cache="$fixture/cache"
    runtime="$fixture/runtime"
    project="$fixture/project"
    instance="runtime-cache-$label"
    mkdir -p "$home" "$config/ztheme/themes" "$cache" "$runtime" \
        "$project" "$fixture/bin" "$fixture/scenarios"
    chmod 700 "$runtime"

    git_available=0
    if (( ! skip_git )) && [[ -x "$user_gitstatusd" ]]; then
        mkdir -p "$fixture/data/ztheme/gitstatus/v1.5"
        cp "$user_gitstatusd" "$fixture/data/ztheme/gitstatus/v1.5/gitstatusd"
        chmod 700 "$fixture/data/ztheme/gitstatus/v1.5/gitstatusd"
        git_available=1
    elif (( skip_git )); then
        print -r -- "$label: git scenarios skipped (BENCHMARK_SKIP_GIT=1)"
    else
        print -r -- "$label: git scenarios skipped: gitstatusd not found at $user_gitstatusd"
    fi

    counter_file="$fixture/counter"
    print -r -- 0 > "$counter_file"
    compile_runtime python "Python 3.12.0" 20
    compile_runtime node "v22.0.0" 20
    compile_runtime rustc "rustc 1.80.0" 20
    compile_runtime go "go version go1.23.0 darwin/arm64" 20
    compile_runtime dart "Dart SDK version: 3.7.2 (stable)" 20
    compile_runtime zig "0.14.1" 20
    compile_runtime julia "julia version 1.11.5" 20
    compile_runtime R "R version 4.5.1 (2025-06-02)" 20
    warm_executable "$fixture/bin/python"
    warm_executable "$fixture/bin/node"
    warm_executable "$fixture/bin/rustc"
    warm_executable "$fixture/bin/go"
    warm_executable "$fixture/bin/dart"
    warm_executable "$fixture/bin/zig"
    warm_executable "$fixture/bin/julia"
    warm_executable "$fixture/bin/R"
    print -r -- 0 > "$counter_file"

    cat > "$config/ztheme/themes/bench.toml" <<'EOF'
version = 1
[layout]
lines = [["python"]]
right = []
separator = " | "
blank_line_before = false
[segments.python]
symbol = "py"
EOF
    cat > "$config/ztheme/themes/four.toml" <<'EOF'
version = 1
[layout]
lines = [["python", "node", "rust", "go"]]
right = []
separator = " | "
blank_line_before = false
[segments.python]
symbol = "py"
[segments.node]
symbol = "node"
[segments.rust]
symbol = "rust"
[segments.go]
symbol = "go"
EOF
    cat > "$config/ztheme/themes/realistic.toml" <<'EOF'
version = 1
[layout]
lines = [["python", "node", "rust", "go", "dart", "zig", "julia", "r"]]
right = []
separator = " | "
blank_line_before = false
[segments.python]
symbol = "py"
[segments.node]
symbol = "node"
[segments.rust]
symbol = "rust"
[segments.go]
symbol = "go"
[segments.dart]
symbol = "dart"
[segments.zig]
symbol = "zig"
[segments.julia]
symbol = "julia"
[segments.r]
symbol = "r"
EOF

    export HOME="$home"
    export XDG_CONFIG_HOME="$config"
    export XDG_DATA_HOME="$fixture/data"
    export XDG_CACHE_HOME="$cache"
    export ZTHEME_RUNTIME_DIR="$runtime"
    export TERM=xterm-256color
    export NO_COLOR=1
    export PATH="$fixture/bin:/usr/bin:/bin"
    rustup_toolchain=""
    rustup_home=""
    gotoolchain="local"
    conda_default_env=""
    last_python_fragment=""
    local client_sequence=0

    local shutdown_source="$fixture/shutdown-daemon.c"
    cat > "$shutdown_source" <<'EOF'
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return 3;
    struct sockaddr_un address = {0};
    address.sun_family = AF_UNIX;
    strncpy(address.sun_path, argv[1], sizeof(address.sun_path) - 1);
    if (connect(fd, (struct sockaddr *)&address, sizeof(address)) != 0) return 4;
    unsigned char request[] = {'Z', 'T', 0, 3};
    if (write(fd, request, sizeof(request)) != (ssize_t)sizeof(request)) return 5;
    unsigned char response;
    int result = read(fd, &response, 1) == 1 && response == 0xfe ? 0 : 6;
    close(fd);
    return result;
}
EOF
    shutdown_helper="$fixture/shutdown-daemon"
    "$compiler" "$shutdown_source" -o "$shutdown_helper"
    chmod 700 "$shutdown_helper"

    local shallow="$fixture/scenarios/shallow"
    local ordinary="$fixture/scenarios/ordinary"
    local distant_root="$fixture/scenarios/distant-root"
    local distant="$distant_root/deep"
    local deep_root="$fixture/scenarios/deep-root"
    local deep="$deep_root/level-1"
    local four_project="$fixture/scenarios/four-project"
    mkdir -p "$shallow" "$ordinary/.git" "$distant_root/.git" "$distant" \
        "$deep_root" "$four_project"
    : > "$shallow/pyproject.toml"
    : > "$ordinary/pyproject.toml"
    : > "$distant_root/pyproject.toml"
    : > "$deep_root/pyproject.toml"
    for i in {2..24}; do
        distant="$distant/level-$i"
        mkdir -p "$distant"
    done
    for i in {2..32}; do
        deep="$deep/level-$i"
        mkdir -p "$deep"
    done
    : > "$four_project/pyproject.toml"
    : > "$four_project/package.json"
    : > "$four_project/Cargo.toml"
    : > "$four_project/go.mod"

    if (( ! skip_latency )); then
        local rustup_warm_home="$fixture/rustup-warm/home"
        local rustup_warm_project="$fixture/rustup-warm/project"
        local rustup_warm_bin="$rustup_warm_home/.cargo/bin"
        local rustup_warm_rustup_home="$rustup_warm_home/.rustup"
        local rustup_warm_toolchain_rustc="$rustup_warm_rustup_home/toolchains/stable/bin/rustc"
        mkdir -p "$rustup_warm_project" "$rustup_warm_bin" "$rustup_warm_rustup_home/toolchains/stable/bin"
        : > "$rustup_warm_project/Cargo.toml"
        compile_runtime rustup-proxy "rustc 1.80.0" 0
        compile_runtime rustup-toolchain "rustc 1.80.0" 0
        ln "$fixture/bin/rustup-proxy" "$rustup_warm_bin/rustup"
        ln "$rustup_warm_bin/rustup" "$rustup_warm_bin/rustc"
        cp "$fixture/bin/rustup-toolchain" "$rustup_warm_toolchain_rustc"
        chmod 700 "$rustup_warm_bin/rustup" "$rustup_warm_bin/rustc" "$rustup_warm_toolchain_rustc"
        cat > "$rustup_warm_rustup_home/settings.toml" <<'EOF'
default_toolchain = "stable"
EOF
        warm_executable "$rustup_warm_bin/rustup"
        warm_executable "$rustup_warm_toolchain_rustc"
        print -r -- 0 > "$counter_file"
        local previous_home=$home
        local previous_rustup_home=$rustup_home
        home=$rustup_warm_home
        rustup_home=$rustup_warm_rustup_home
        measure_warm_scenario rustup-warm four "$rustup_warm_project" "$rustup_warm_bin:/usr/bin:/bin"
        home=$previous_home
        rustup_home=$previous_rustup_home

        local scenario_bin scenario_path
        for scenario in shallow ordinary distant deep; do
        scenario_bin="$fixture/scenarios/$scenario/bin"
        mkdir -p "$scenario_bin"
        cp "$fixture/bin/python" "$scenario_bin/python"
        chmod 700 "$scenario_bin/python"
        warm_executable "$scenario_bin/python"
        scenario_path="$scenario_bin:/usr/bin:/bin"
        case "$scenario" in
            shallow) scenario_cwd=$shallow ;;
            ordinary) scenario_cwd=$ordinary ;;
            distant) scenario_cwd=$distant ;;
            deep) scenario_cwd=$deep ;;
        esac
        measure_warm_scenario "$scenario" bench "$scenario_cwd" "$scenario_path"
        done

        measure_warm_scenario four-direct four "$four_project" "$fixture/bin:/usr/bin:/bin"

        if (( git_available )); then
            cat > "$config/ztheme/themes/gittheme.toml" <<'GEOF'
version = 1
[layout]
lines = [["git"]]
right = []
separator = " | "
blank_line_before = false
[segments.git]
prefix = "on "
symbol = "git"
action_prefix = " "
changes_prefix = " "
style = { foreground = "#f9e2af" }
action_style = { foreground = "#f38ba8" }
[segments.git.symbols]
conflicted = "="
staged = "+"
modified = "!"
deleted = "✘"
untracked = "?"
ahead = "⇡"
behind = "⇣"
diverged = "⇕"
stash = "$"
[segments.git.styles]
conflicted = { foreground = "#f38ba8", bold = true }
staged = { foreground = "#a6e3a1", bold = true }
modified = { foreground = "#fab387", bold = true }
deleted = { foreground = "#f38ba8", bold = true }
untracked = { foreground = "#cba6f7", bold = true }
ahead = { foreground = "#a6e3a1" }
behind = { foreground = "#cba6f7" }
diverged = { foreground = "#cba6f7" }
stash = { foreground = "#89b4fa" }
GEOF

            local git_small="$fixture/scenarios/git-small"
            local git_dirty="$fixture/scenarios/git-dirty"
            local git_large="$fixture/scenarios/git-large"
            local git_large_dirs=200
            local git_large_files_per_dir=$(( git_large_files / git_large_dirs ))
            print -r -- "$label: creating git repositories (large = $git_large_dirs dirs x $git_large_files_per_dir files)"
            create_git_repo "$git_small" 4 16 0
            create_git_repo "$git_dirty" 4 16 1
            create_git_repo "$git_large" "$git_large_dirs" "$git_large_files_per_dir" 0

            git_scenarios_measured=(git-small git-large git-dirty)
            local git_path="$fixture/bin:/usr/bin:/bin"
            measure_warm_scenario git-small gittheme "$git_small" "$git_path"
            if [[ "$last_response_summary" != *main* ]]; then
                print -u2 -- "$label git-small: expected clean main branch, got $last_response_summary"
                exit 1
            fi
            measure_warm_scenario git-large gittheme "$git_large" "$git_path"
            if [[ "$last_response_summary" != *main* ]]; then
                print -u2 -- "$label git-large: expected clean main branch, got $last_response_summary"
                exit 1
            fi
            measure_warm_scenario git-dirty gittheme "$git_dirty" "$git_path"
            if [[ "$last_response_summary" != *'?'* ]]; then
                print -u2 -- "$label git-dirty: expected untracked marker, got $last_response_summary"
                exit 1
            fi
        fi

        local cold_one="$fixture/scenarios/cold-one"
        mkdir -p "$cold_one"
        cp "$fixture/bin/python" "$cold_one/python"
        chmod 700 "$cold_one/python"
        warm_executable "$cold_one/python"
        measure_cold_scenario one-20ms bench "$shallow" "$cold_one:/usr/bin:/bin"

        local cold_four="$fixture/scenarios/cold-four"
        mkdir -p "$cold_four"
        for runtime_name in python node rustc go; do
            cp "$fixture/bin/$runtime_name" "$cold_four/$runtime_name"
            chmod 700 "$cold_four/$runtime_name"
            warm_executable "$cold_four/$runtime_name"
        done
        measure_cold_scenario four-20ms four "$four_project" "$cold_four:/usr/bin:/bin"

        project=$shallow
        load_theme bench
        run_concurrent_cold_miss

        local restart_bin="$fixture/scenarios/restart/bin"
        mkdir -p "$restart_bin"
        cp "$fixture/bin/python" "$restart_bin/python"
        chmod 700 "$restart_bin/python"
        warm_executable "$restart_bin/python"
        measure_persisted_restart "$shallow" "$restart_bin:/usr/bin:/bin"
    fi

    local realistic_bin="$fixture/realistic-bin"
    mkdir -p "$realistic_bin"
    for runtime_name in python node rustc go dart zig julia R; do
        cp "$fixture/bin/$runtime_name" "$realistic_bin/$runtime_name"
        chmod 700 "$realistic_bin/$runtime_name"
    done
    for runtime_name in python node rustc go dart zig julia R; do
        warm_executable "$realistic_bin/$runtime_name"
    done

    realistic_dirs=()
    realistic_groups=()
    realistic_opportunities=()
    for (( i = 1; i <= 40; i++ )); do
        local realistic_directory="$fixture/realistic/project-$i"
        mkdir -p "$realistic_directory"
        realistic_dirs[$i]="$realistic_directory"
        case $(( i % 8 )) in
            0)
                : > "$realistic_directory/pyproject.toml"
                : > "$realistic_directory/package.json"
                : > "$realistic_directory/Cargo.toml"
                : > "$realistic_directory/go.mod"
                realistic_groups[$i]=mixed
                realistic_opportunities[$i]=4
                ;;
            1)
                : > "$realistic_directory/pyproject.toml"
                realistic_groups[$i]=python
                realistic_opportunities[$i]=1
                ;;
            2)
                : > "$realistic_directory/package.json"
                realistic_groups[$i]=node
                realistic_opportunities[$i]=1
                ;;
            3)
                : > "$realistic_directory/Cargo.toml"
                realistic_groups[$i]=rust
                realistic_opportunities[$i]=1
                ;;
            4)
                # Flutter app: Dart is the only detected runtime.
                print -r -- "name: app" > "$realistic_directory/pubspec.yaml"
                mkdir -p "$realistic_directory/lib"
                : > "$realistic_directory/lib/main.dart"
                : > "$realistic_directory/analysis_options.yaml"
                realistic_groups[$i]=dart
                realistic_opportunities[$i]=1
                ;;
            5)
                # Zig project with a source tree.
                print -r -- "const std = @import(\"std\");" > "$realistic_directory/build.zig"
                mkdir -p "$realistic_directory/src"
                : > "$realistic_directory/src/main.zig"
                realistic_groups[$i]=zig
                realistic_opportunities[$i]=1
                ;;
            6)
                # Julia package with a project manifest.
                print -r -- "name = \"Pkg\"" > "$realistic_directory/Project.toml"
                : > "$realistic_directory/Manifest.toml"
                mkdir -p "$realistic_directory/src"
                : > "$realistic_directory/src/Pkg.jl"
                realistic_groups[$i]=julia
                realistic_opportunities[$i]=1
                ;;
            7)
                # R package layout.
                print -r -- "Package: pkg" > "$realistic_directory/DESCRIPTION"
                : > "$realistic_directory/NAMESPACE"
                mkdir -p "$realistic_directory/R"
                : > "$realistic_directory/R/pkg.R"
                realistic_groups[$i]=r
                realistic_opportunities[$i]=1
                ;;
        esac
    done

    local pyenv_root="$fixture/realistic-pyenv"
    mkdir -p "$pyenv_root/bin" "$pyenv_root/shims" \
        "$pyenv_root/versions/3.11/bin" "$pyenv_root/versions/3.12/bin"
    cat > "$pyenv_root/bin/pyenv" <<'EOF'
#!/bin/sh
exit 0
EOF
    cat > "$pyenv_root/shims/python" <<'EOF'
#!/bin/sh
exit 1
EOF
    chmod 700 "$pyenv_root/bin/pyenv" "$pyenv_root/shims/python"
    compile_runtime pyenv311 "Python 3.11.0" 0
    compile_runtime pyenv312 "Python 3.12.0" 0
    cp "$fixture/bin/pyenv311" "$pyenv_root/versions/3.11/bin/python"
    cp "$fixture/bin/pyenv312" "$pyenv_root/versions/3.12/bin/python"
    chmod 700 "$pyenv_root/versions/3.11/bin/python" "$pyenv_root/versions/3.12/bin/python"
    warm_executable "$pyenv_root/versions/3.11/bin/python"
    warm_executable "$pyenv_root/versions/3.12/bin/python"
    local pyenv_project=${realistic_dirs[40]}
    print -r -- "3.11" > "$pyenv_project/.python-version"
    realistic_groups[40]=pyenv
    realistic_opportunities[40]=4

    compile_runtime replacement "Python 3.13.0" 0
    compile_runtime julia-replacement "julia version 1.12.0" 0
    warm_executable "$fixture/bin/replacement"
    warm_executable "$fixture/bin/julia-replacement"
    print -r -- 0 > "$counter_file"
    (( skip_realistic )) || run_realistic_workload "$realistic_bin" "$pyenv_root" \
        "$fixture/bin/replacement" "$fixture/bin/julia-replacement"
}

check_latency_regressions() {
    (( skip_latency )) && return 0

    local scenario
    local -F 6 baseline_p50 candidate_p50 baseline_p95 candidate_p95 allowed
    for scenario in rustup-warm shallow ordinary distant deep four-direct "${git_scenarios_measured[@]}"; do
        baseline_p50=${warm_p50_us["baseline/$scenario"]}
        candidate_p50=${warm_p50_us["candidate/$scenario"]}
        allowed=$(( baseline_p50 * 0.05 ))
        (( allowed < 150.0 )) && allowed=150.0
        if (( candidate_p50 > baseline_p50 + allowed )); then
            print -u2 -- "candidate $scenario: warm p50 regression exceeds 5% or 150us"
            exit 1
        fi

        baseline_p95=${warm_p95_us["baseline/$scenario"]}
        candidate_p95=${warm_p95_us["candidate/$scenario"]}
        allowed=$(( baseline_p95 * 0.10 ))
        (( allowed < 500.0 )) && allowed=500.0
        if (( candidate_p95 > baseline_p95 + allowed )); then
            print -u2 -- "candidate $scenario: warm p95 regression exceeds 10% or 500us"
            exit 1
        fi
    done
}

if (( skip_baseline )); then
    measure_build candidate "$candidate_binary"
else
    measure_build baseline "$baseline_binary"
    measure_build candidate "$candidate_binary"
    check_latency_regressions

    if (( realistic_execution_counts[candidate] >= realistic_execution_counts[baseline] )); then
        print -u2 -- "candidate realistic workload did not execute fewer runtime commands than baseline"
        exit 1
    fi
fi
