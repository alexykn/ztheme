# Async Architecture

## Is async still worth it?

Prompt rendering now waits until every requested segment is ready. Async work
no longer exists to show part of the prompt early; its remaining purpose is to
run independent Git and runtime work in parallel.

To see whether that is still worth the complexity, I benchmarked all four
combinations of async and blocking client and daemon cores. The Unix socket and
client-daemon protocol stayed async in every variant, so only the internal work
model changed:

- `aa`: async client, async daemon;
- `as`: async client, blocking daemon;
- `sa`: blocking client, async daemon;
- `ss`: blocking client, blocking daemon.

Each result below contains 1,000 measured samples. Values are p50 / p95 in
milliseconds. Cold daemon startup is intentionally omitted because it mostly
affects the first prompt after opening a terminal and does not represent normal
prompt refreshes.

| Scenario | Workload | `aa` | `as` | `sa` | `ss` |
| --- | --- | ---: | ---: | ---: | ---: |
| warm cache | Git + 4 runtimes | 3.450 / 8.421 | 4.308 / 8.252 | 4.093 / 7.347 | 3.724 / 7.589 |
| warm cache | 4 runtimes | 5.087 / 8.893 | 5.100 / 10.283 | 5.041 / 10.449 | 4.800 / 9.825 |
| cache miss | 1 runtime | 8.696 / 15.304 | 7.782 / 14.362 | 8.729 / 15.570 | 8.601 / 15.109 |
| cache miss | 4 runtimes | 9.334 / 14.578 | 9.278 / 15.868 | 20.371 / 28.875 | 20.335 / 28.245 |

The warm-cache differences are small and inconsistent. There is no meaningful
fixed async penalty here: depending on the row and percentile, every variant
can appear slightly ahead.

The four-runtime cache miss is different. Both async-client variants finish in
about 9 ms at p50, while both blocking-client variants need about 20 ms. Once
runtime commands actually have to execute, running them sequentially more than
doubles prompt latency.

Changing only the daemon core does not produce a consistent improvement. It
also cannot remove Tokio because the socket transport remains async. A blocking
daemon core would instead add blocking-worker boundaries, standard mutexes, and
separate cache and `gitstatusd` implementations without buying a stable latency
gain.

## Result of the experiment

Keep the current async client and async daemon cores.

The async client preserves useful parallelism when the cache misses, while its
overhead is lost in the noise on cache hits. The async daemon already matches
the transport model and handles concurrent clients without another execution
layer. Maintaining sync alternatives would add more architecture than the
measurements justify.
