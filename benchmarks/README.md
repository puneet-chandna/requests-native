# Local benchmark harness

This directory compares the frozen Python Requests oracle, the rewrite's
Rust-backed Python API, and the native Rust async and blocking APIs against one
HTTP/1.1 loopback fixture. Measurements have no pass/fail target and do not
authorize compatibility changes.

## Run

Use the rewrite virtual environment so the compiled Python extension and the
frozen oracle's dependencies are available:

```console
.venv/bin/python benchmarks/run.py --profile smoke
.venv/bin/python benchmarks/run.py --profile default
```

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

Each case runs in a fresh process without a warm-up phase. One-shot timings
include client construction; pooled cases keep one client per logical worker.
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

Peak RSS is a whole-process high-water mark and therefore includes imports and
runtime setup. `ru_maxrss` is normalized from bytes on macOS and KiB elsewhere.
Native CPU and RSS are read from `/proc` on Linux; unsupported platforms record
`null`. Tiny runs can report zero native CPU because Linux accounting is
tick-granular. Streaming counts are application-level chunks: both native
clients aggregate or split transport frames to honor the configured chunk size,
with only the last chunk allowed to be shorter.

Loopback results reduce network noise but do not predict internet, proxy, TLS,
DNS, or application performance. Compare rows only within the same result file,
and retain the raw file when reporting any observation.
