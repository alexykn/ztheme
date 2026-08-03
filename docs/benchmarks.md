# Benchmarks

Current measured behavior of the released binary. The warm, git, and
cold-miss numbers below were measured with ztheme v1.6.0 and are
statistically unchanged on v1.7.0; the realistic-workload numbers are
measured on the current development tree (see below). All runs were on an
Apple M1 Pro (macOS 26.5.2, arm64). Prompt latency is machine- and
filesystem-dependent; treat the absolute numbers as indicative and the
*relative* numbers (e.g. the marginal cost of a custom segment) as the
stable signal.

## Scope

Two distinct paths are measured:

- **Async prompt path** — the client ↔ daemon exchange that supplies Git and
  runtime segments. Measured by `tools/benchmark-runtime-cache.zsh`, which
  talks to the real client daemon over the real socket protocol.
- **Synchronous segment path** — the Zsh-side dispatcher (`_ztheme_compute_sync_segments`
  + `_ztheme_render_layout`) that renders directory, status, character, and
  custom segments once per prompt.

## Reproducing

```console
BENCHMARK_SKIP_BASELINE=1 ./tools/benchmark-runtime-cache.zsh
```

`BENCHMARK_SKIP_BASELINE=1` measures the current working tree only. Without
it, the harness additionally builds an isolated baseline from `HEAD` and gates
the candidate against it (5% p50 / 10% p95 latency allowances, and fewer
runtime executions in the realistic workload). The baseline-vs-candidate
comparison assumes an older `HEAD`; when the working tree is the released
version, use the skip flag.

The git scenarios use the **real installed gitstatusd** (resolved from
`$XDG_DATA_HOME` or `~/.local/share`, copied into an isolated fixture) and
repositories created with the real `git` CLI. Set `BENCHMARK_SKIP_GIT=1` to
skip them, or `BENCHMARK_GIT_LARGE_FILES` (default 20000) to resize the large
repository.

Environment variables: `BENCHMARK_MEASURED_PROMPTS` (default 1000 per run,
5 runs), `BENCHMARK_WARMUP_PROMPTS` (100), `BENCHMARK_REALISTIC_PROMPTS`
(5000), `BENCHMARK_SKIP_REALISTIC`, `BENCHMARK_SKIP_LATENCY`.

The synchronous-path numbers were measured by eval'ing the generated
integration into a fresh `zsh -dfc` and timing 5000 dispatcher+render cycles
(`_ztheme_compute_sync_segments 0; _ztheme_render_layout`), reporting the
total; no prompt-adjacent process spawns are included.

## Async prompt path

### Warm prompt latency

A cache hit with no runtime command execution, measured over 5 × 1000 prompts;
values are the median of the five per-run p50/p95 in microseconds. A warm hit
never executes a runtime command — the fixture's 20 ms command cost is never
paid here — so the warm numbers are client + daemon + cache-key work only.

| Scenario | p50 (µs) | p95 (µs) |
| --- | ---: | ---: |
| `rustup-warm` — four runtimes behind a rustup proxy | 643 | 786 |
| `shallow` — one runtime, git repository root | 520 | 645 |
| `ordinary` — one runtime, `.git` directory present | 272 | 346 |
| `distant` — one runtime, repository root 24 levels up | 828 | 1014 |
| `deep` — one runtime, 32-level nested directory tree | 958 | 1166 |
| `four-direct` — four runtimes in one project | 735 | 899 |

The directory-walking scenarios (`distant`, `deep`) are the slowest warm hits:
cache-key hashing walks up to the repository root, and the walk cost scales
with directory depth. `ordinary` (a `.git` directory inside a shallow tree) is
the fastest case.

### Git status in the loop

Real `gitstatusd` against real repositories, warm gitstatusd, 5 × 1000
prompts; values are the median of the five per-run p50/p95 in microseconds.
The theme shows only the git segment, so the whole cost is the worktree scan
plus the client-daemon round trip:

| Scenario | Repository | p50 (µs) | p95 (µs) |
| --- | --- | ---: | ---: |
| `git-small` | 64 files, clean, branch `main` | 429 | 575 |
| `git-dirty` | 64 files + 256 untracked + 1 modified | 1624 | 2162 |
| `git-large` | 20,000 files across 200 directories, clean | 18,406 | 20,229 |

The first git prompt after a daemon start is markedly slower (189 ms in this
run), but the harness does not isolate that cost: the first-request timing
combines client startup, the daemon's lazy `gitstatusd` spawn, and the initial
worktree scan. The warm rows above are the steady state after that one-time
startup.

Git status is the dominant prompt cost at scale: a small clean repository
(~0.4 ms) costs about the same as a runtime-only warm prompt, a dirty tree
adds roughly 1.2 ms of untracked scanning, and a 20,000-file repository
reaches ~18 ms p50 — two orders of magnitude above every other warm case.
That cost lives entirely inside `gitstatusd`'s worktree scan; the ztheme
pipeline around it stays in the low hundreds of microseconds.

### Cold miss

The command-running case: first prompt after a cache clear, with every runtime
command taking 20 ms (the fixture's modeled command cost — see
[Command-cost calibration](#command-cost-calibration) below).

| Scenario | elapsed (ms) |
| --- | ---: |
| one runtime | 34.2 |
| four runtimes | 35.1 |

The four-runtime cold miss stays close to the one-runtime cost because the
cache snapshot is singleflight and runtime commands run concurrently; the
serial case would be four × 20 ms plus overhead.

### Command-cost calibration

Why does the fixture cost each runtime command 20 ms? The real `--version`
commands ztheme runs on a cold cache were measured in ztheme's controlled
environment (`env -i` baseline with `LC_ALL=C TERM=dumb NO_COLOR=1`, stdin
from `/dev/null`), 100 warm iterations each, on this machine:

| runtime | p50 (ms) | p95 (ms) |
| --- | ---: | ---: |
| dotnet | 7.2 | 9.7 |
| lua | 7.8 | 9.9 |
| perl | 8.4 | 10.4 |
| go | 13.0 | 14.3 |
| ruby | 14.5 | 17.5 |
| rustc | 16.4 | 20.2 |
| python3 | 16.6 | 19.2 |
| java | 35.9 | 41.2 |
| node | 37.9 | 42.4 |
| php | 69.6 | 72.8 |
| swift | 136.1 | 144.7 |

Excluding the swift outlier (≈ 6× the mean), the average is ≈ 23 ms p50 with
a median of ≈ 15 ms. A fixed 20 ms sits between the two — a round, slightly
conservative constant that keeps cold-miss measurements deterministic instead
of sampling a noisy distribution. Swift's cost is also the argument for
running commands concurrently on a miss: four 20 ms commands complete in
≈ 35 ms, where four serial 136 ms swift-style commands would exceed half a
second.

### Cache correctness under stress

- **Singleflight**: 8 concurrent clients issuing the same cold request cause
  exactly **1** runtime execution.
- **Daemon restart**: a daemon restart reuses persisted cache entries — the
  runtime-execution counter is unchanged across the restart (26 → 26).
- **Realistic workload**: measured on the current development tree (the
  expanded workload covering the dart, zig, julia, and r cacheables is not
  part of any released binary): 5000 prompts across 40 project directories
  (mixed monorepos, python, node, rust, plus dart, zig, julia, and r as
  single-language projects; a pyenv shim group; PATH noise; a mid-run daemon
  restart; a python and a julia executable replacement; a `.python-version`
  selector switch):

  | Metric | Value |
  | --- | ---: |
  | cache hit rate | 99.64% (25 executions / 6878 opportunities) |
  | p50 latency | 853 µs |
  | p95 latency | 1216 µs |
  | maximum latency | 31.0 ms |
  | stale results | 0 |

  The executable replacements (python and julia) and the selector switch each
  invalidate exactly the affected entry: the immediately following prompt is
  current, and no stale fragment was observed for the rest of the run.

## Synchronous segment path

The Zsh-side per-prompt cost with the default theme (catppuccin-mocha, no
custom segments) and with three custom segments, 5000 cycles each:

| Theme | per prompt (µs) |
| --- | ---: |
| default, no custom segments | 104 |
| default + three custom segments | 197 |

The marginal cost of a custom segment is roughly **31 µs** per prompt. The
dispatcher performs no filesystem or subprocess work; this is pure Zsh
function dispatch and layout rendering.

## Coverage

Measured: warm and cold prompt latency, git status in the loop, cache hit rate
and stale-result behavior, singleflight concurrency, persistence across daemon
restart, and the synchronous segment path.

Not measured here: full shell startup/init time and memory usage. These are
candidates for future additions to `tools/benchmark-runtime-cache.zsh`.

## Comparison with the pre-1.4.0 architecture

`docs/ideas/async-architecture.md` contains a benchmark from before the
persistent per-shell client and the runtime snapshot cache. Its numbers are
p50/p95 in milliseconds; the table below converts the warm rows (old "aa" =
async client + async daemon, the design that shipped; new = v1.6.0):

| Warm cache | Old (ms) | New (ms) | p50 ratio |
| --- | ---: | ---: | ---: |
| 4 runtimes | 5.087 / 8.893 | 0.735 / 0.899 (`four-direct`) | ~6.9x |
| Git + 4 runtimes | 3.450 / 8.421 | no combined fixture | — |

Git alone is measured in the git section (0.4–18 ms by repository size); a
combined git + runtimes fixture does not exist yet.

Warm p50 is roughly 5–7x faster and the p95 tail narrows by roughly an order
of magnitude. The improvement is the sum of the changes since that study —
the persistent client (no per-prompt spawn), the runtime snapshot cache (no
per-prompt executions), and the subsequent hardening — not a change in the
async model, which that study had already selected.

The cache-miss rows are **not comparable**: the current fixture deliberately
costs each runtime command 20 ms, while the old fixture measured whatever the
local runtime commands happened to cost. The old doc also does not state its
machine, so the ratios assume similar hardware.

## History

The async-architecture numbers remain in `docs/ideas/async-architecture.md`
as the design record for why the async work model was chosen; they do not
reflect the current release.
