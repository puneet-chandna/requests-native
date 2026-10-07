# Local benchmark harness

This directory compares the frozen Python Requests oracle, the rewrite's
Rust-backed Python API, and the native Rust async and blocking APIs against one
HTTP/1.1 loopback fixture. The paired release evaluator adds a regression gate;
measurements do not authorize compatibility changes.

## Local and CI regression evaluation

From an existing development environment, run a quick local check:

```console
.venv/bin/python benchmarks/evaluate.py --base v1.0.0-beta --candidate WORKTREE --smoke
```

This captures local uncommitted source without switching branches or committing.
It runs one short pair and checks the pipeline; the performance decision is
**inconclusive**, not release qualification. Untracked runtime files under
`src/` and `crates/` are included; other untracked files are excluded.
Use `--candidate HEAD` for the current committed revision.

For the release performance gate:

```console
.venv/bin/python benchmarks/evaluate.py --base v1.0.0-beta --candidate HEAD --gate
```

The `.github/workflows/evaluate.yml` workflow runs this identical command on
`namespace-profile-puneet-chandna`; its smoke input selects the quick check.
Windows/macOS packaging and qualification retain their GitHub runners. Neither
mode publishes packages or creates a GitHub release.
Manual CI exposes the same pair/request/warm-up counts and per-case deadline,
with a 90- or 180-minute job deadline. Defaults remain five pairs, 100 requests,
four warm-ups and a 60-second per-case deadline. Select a longer job deadline
deliberately when increasing samples; it can consume more runner time.

Prerequisites: an isolated Python environment with maturin and runtime
dependencies, Git, rustup, a clean frozen oracle checkout (the sibling `requests`
directory or `REQUESTS_ORACLE_ROOT`), and disk space for two release builds.
Linux is the validated local platform; native CPU/RSS accounting needs Linux.
The oracle's behavioral files must match `ORACLE.lock`; its later documentation
commit is permitted. A fresh environment can use the same dependencies as CI:

```console
python3 -m venv .venv
.venv/bin/python -m pip install maturin==1.15.0 charset_normalizer==3.5.1 idna==3.19 urllib3==2.8.0 certifi==2026.7.22
```

Rust dependencies are prepared online by default; `--offline` needs a populated
Cargo cache. Git references must already exist locally. Both revisions use the
candidate's Rust toolchain and identical Python dependencies. The current
evaluator and native driver run against both snapshots; the driver's dependency
lock is seeded from each revision's release lock. Source, driver-lock,
release-lock and evaluator hashes identify what ran. This compares source on a
common toolchain, rather than previously published binary artifacts.
The evaluator checks that preinstalled dependencies satisfy both revisions and
disables dependency installation while switching native editable artifacts.
An incompatible environment fails preflight without upgrading your packages.

The snapshots share the caller's Python dependency environment. `maturin develop`
switches its editable Requests Native install during evaluation and restores the
caller's checkout afterward. Keep other native Python tests and development
processes out of this environment during evaluation; use a dedicated evaluation
venv if your development server must remain running.

Default qualification runs five alternating pairs, 100 measured requests per
case and four untimed requests per logical worker. Warm-up retains pooled
clients, checks bytes/checksum/native routing, then releases workers through a
shared timing barrier. All 64 cases cover four surfaces, one-shot/pooled clients,
buffered/streaming reads, small/large bodies and serial/concurrent workloads.
Allocation replay is separate from timed measurements.
The first Namespace run took about seven minutes per side; allow roughly
75 minutes for the full gate, including builds. Use smoke mode for a quick
local pipeline check; the full gate is a deliberate release qualification run.
On Linux the worker resets its own resident-memory high-water mark at this
barrier using `/proc/self/clear_refs`, so a released warm-up peak cannot conceal
a measured-phase RSS increase. If reset is unavailable, the scope is recorded
as process lifetime and release evidence is inconclusive.

Evidence goes to `target/evaluations/<timestamp>/`: raw reports, local build/
error logs and `comparison.json`. CI retains JSON evidence and diagnostic logs on failure. A nonempty
output directory is never overwritten; use `--output DIRECTORY` to choose one.

The initial policy allows a configurable 20% increase in cost per case: inverse
throughput, p95 latency, peak RSS and allocation replay. CPU is summed per surface
to reduce Linux tick granularity; totals below 50 ms are insufficient evidence.
A deterministic paired bootstrap reports a marginal 95% interval for each
median cost ratio. An interval entirely above budget is a regression; one
crossing budget is inconclusive. Qualification passes only when every measured
case is within budget with sufficient evidence. The unchanged Python oracle
controls machine drift in either direction; noisy controls make the overall
decision inconclusive. Calibrate this initial budget on the workload; it is not
a universal statistical guarantee.

`--gate` returns nonzero on regression, inconclusive evidence or execution/
correctness errors, and rejects smoke mode and uncommitted source. Larger samples
can resolve uncertainty without discarding a failing case:

```console
.venv/bin/python benchmarks/evaluate.py --base v1.0.0-beta --gate --pairs 7 --requests 300 --warmup 8 --case-timeout-seconds 120 --output target/evaluations/release-check
.venv/bin/python -m unittest benchmarks.test_run benchmarks.test_evaluate
```

For local calibration, fixed per-surface counts can lengthen fast cases without
multiplying the slow Python/Rust workload. Both `run.py` and `evaluate.py` accept
repeated `--surface-requests SURFACE=N` options; other surfaces retain the
`--requests` default. Timing and allocation replay use the same count. Each
report records the complete resolved map, which must match across every pair;
release evidence still requires at least 100 requests for every surface.
Overrides also apply in smoke mode; omit them to keep smoke checks short.
Smoke reports remain ineligible for release qualification regardless of counts.

This is an **unqualified calibration configuration**, not a passing result:

```console
.venv/bin/python benchmarks/evaluate.py --base v1.0.0-beta --candidate HEAD --gate --pairs 5 --requests 100 --surface-requests python-oracle=1000 --surface-requests rust-async=3000 --surface-requests rust-blocking=3000 --warmup 8 --case-timeout-seconds 120 --output target/evaluations/surface-calibration
```

All four surfaces, 64 cases, full metrics and the 20% budget remain in force.
Noisy oracle controls or uncertain comparisons still block qualification.
For the same unqualified configuration in manually dispatched CI, set
`surface_requests` to `python-oracle=1000 rust-async=3000 rust-blocking=3000`,
keep `requests=100` and `pairs=5`, and set `warmup=8` and
`case_timeout_seconds=120`. The optional input defaults to empty, preserving
the existing CI sampling defaults. These settings do not establish calibration
success; local and Namespace runs must each produce their own evidence.

Keep the machine idle during measurements. Loopback results do not predict
internet/proxy/TLS/DNS performance or close the Windows TLS issue. Source CI
separately runs native unit tests, frozen-oracle differential checks,
distribution/fresh-install checks and benchmark helper checks on relevant
changes. Full platform artifact qualification remains a release requirement.

On Linux machines with mixed performance/efficiency cores, scheduling can add
timing variation. For local calibration, choose a homogeneous set from
`lscpu --extended=CPU,CORE,MAXMHZ` and launch the evaluator with
`taskset --cpu-list <CPU-list>`. The fixture and workers inherit that affinity.
Reports record the allowed CPU IDs; paired comparisons reject different masks.
Platforms without the affinity API record `null`. This records the allowed set,
not continuous scheduling or CPU frequency. Keep the chosen set fixed across
comparisons and label control-only probes as calibration, not qualification.

## Historical observations

The checked-in 20260902 result predates the Requests Native rename; its
``requests-rust`` backend label is preserved as historical evidence.

The [September 13, 2026 local-date result](results/20260913-local-default.json)
contains 64 rows, 16 per surface, from the bounded default profile. Median
throughput was 504.905 requests/s for the frozen Python oracle, 23.859 for the
Rust-backed Python API, 3,172.514 for native Rust async, and 2,531.828 for native
Rust blocking. Python/Rust was slower in this run; this is not an overall
speedup claim.

The recorded source is `2a15969` with documentation/test changes in progress,
not a clean immutable release candidate. Runtime/build inputs were unchanged;
the extension and native driver were rebuilt in release mode offline. The
oracle's documentation commit differs from its frozen behavioral commit, but
its Python source, tests, and package manifests match the frozen baseline.
Twelve requests per case without warm-up do not predict production performance
or resolve the Windows TLS issue.
The public result normalizes its output destination to `{benchmark-output}`;
measurement data and build provenance are unchanged.

## Run

Use the rewrite virtual environment so the compiled Python extension and the
frozen oracle's dependencies are available:

```console
.venv/bin/python benchmarks/run.py --profile smoke
.venv/bin/python benchmarks/run.py --profile default
```

The frozen oracle defaults to the sibling `requests` checkout. Set
`REQUESTS_ORACLE_ROOT` to its absolute path if your checkouts are elsewhere.
The same setting is inherited by benchmark worker processes.

`smoke` uses two requests per case, 64 B and 32 KiB bodies, and concurrency
levels 1 and 2. `default` uses twelve requests per case, 128 B and 256 KiB
bodies, and concurrency levels 1 and 4. Both profiles cover every combination
of:

- frozen Python, Rust-backed Python, native Rust async, and native Rust
  blocking;
- one-shot and pooled clients;
- small and large bodies;
- buffered and streaming reads;
- serial and concurrent clients.

Override bounded inputs when a longer run is useful:

```console
.venv/bin/python benchmarks/run.py --profile default \
  --requests 100 --concurrency 8 --large-bytes 1048576 \
  --chunk-size 65536 --output benchmarks/results/local-long.json
```

Use `--surfaces python-oracle python-rust` to select surfaces. The standalone
native driver and the exact Rust-backed Python extension are built in release
mode and offline before every run. The extension build uses repository-local
temporary and uv-cache directories, then requires the loaded editable module
to match the release library byte-for-byte. The native driver is not a member
of the release workspace. `--case-timeout-seconds` sets the orchestrator's
deadline for each child workload; the default is 30 seconds.

The build locates `maturin` from the active isolated Python environment (with
`PATH` lookup as a fallback), uses `bin` on Unix and `Scripts` on Windows, and
selects the unique release `cdylib` for Linux, macOS, or Windows. Those platform
paths have focused unit coverage; the checked-in result and live provenance
smoke were collected on Linux, not on macOS or Windows.

## Fail-fast checks

The harness stops before writing a result if:

- the frozen oracle checkout is dirty or either Python surface imports from the
  wrong tree;
- the loopback fixture cannot serve two correct responses over one connection;
- an accepted fixture socket does not have `TCP_NODELAY` enabled;
- a worker sends the wrong number of requests, reads unequal bytes, or computes
  an unequal checksum;
- a streaming worker does not emit the configured application chunk sizes;
- Rust-backed Python returns a non-native response;
- the loaded Python extension is outside the intended virtual environment or
  differs from the just-built release artifact;
- the fixture and worker disagree about connection identities;
- a child exceeds its case deadline;
- timing or result-schema fields are missing or invalid.

The small deterministic helper checks run with:

```console
.venv/bin/python -m unittest benchmarks.test_run -v
```

## Result interpretation

Each JSON file records the exact child commands, Git state, toolchains, machine
metadata, Python ABI and dependency versions, extension paths and SHA-256,
configuration, raw latency samples, latency distribution, throughput, client
CPU, process peak RSS, allocation replay, transferred bytes, application chunk
count, checksum, and fixture-observed connection reuse.

Each case runs in a fresh process; `run.py` defaults to no warm-up, while the
paired evaluator supplies an explicit warm-up. One-shot elapsed timings include
client construction; per-request latencies cover the request/read. Pooled cases
keep one client per logical worker.
The loopback server runs in the orchestrator, so its CPU is not included in
client CPU. Python allocation numbers come from an otherwise identical serial
replay, so `tracemalloc` does not distort the timed sample; they cover
Python-traced allocations, not the Rust allocator. The serial replay avoids a
known instrumentation interaction in which concurrent fresh Rust-backed Python
sessions can fall back during `tracemalloc`; the measured concurrency remains
recorded separately. Native allocation totals count requested bytes from
successful `System` allocator calls during an untimed replay at the measured
concurrency. They are neither live nor peak bytes and are not directly
comparable to Python's traced peak. The native counter performs no allocation
of its own and is disabled for timed samples.

Peak RSS in historical reports is a process-lifetime high-water mark. Current
Linux workers reset it before the measured phase and record `rss_scope`;
already-resident imports and warmed client state remain part of memory in use.
Other platforms retain lifetime RSS; `ru_maxrss` is normalized from bytes on
macOS and KiB elsewhere.
Native CPU and RSS are read from `/proc` on Linux; unsupported platforms record
`null`. Tiny runs can report zero native CPU because Linux accounting is
tick-granular. Streaming counts are application-level chunks: both native
clients aggregate or split transport frames to honor the configured chunk size,
with only the last chunk allowed to be shorter.

Loopback results reduce network noise but do not predict internet, proxy, TLS,
DNS, or application performance. Compare historical rows only within the same
result file; use the paired evaluator for regression comparisons and retain its
raw reports.
