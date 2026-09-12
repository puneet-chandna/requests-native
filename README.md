<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-beta-orange.svg)](https://github.com/puneet-chandna/requests-native/releases)

Requests Native is an unofficial, independent Rust rewrite of
[PSF Requests](https://github.com/psf/requests). It provides the familiar
`import requests` Python API and separate async and blocking Rust clients.
The compatibility target is Requests 2.34.2.

**Beta:** [known issue #1: intermittent Windows TLS failures](https://github.com/puneet-chandna/requests-native/issues/1)
is accepted for this beta, not fixed. HTTPS tests have failed with timeouts or
connection errors, including during redirects; the cause, failure rate, and
production impact are unknown. Platform qualification is incomplete. Test your
workload before replacing production Requests. The project is not published
to PyPI or crates.io.

## Python

```python
import requests

response = requests.get("https://httpbin.org/get", timeout=10)
response.raise_for_status()
print(response.json())
```

Use a session to reuse connections:

```python
with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

### Install from source

You need Python 3.10 or later, Rust through rustup, and a C build toolchain
(MSVC Build Tools on Windows). The repository pins Rust 1.98.0 in
[`rust-toolchain.toml`](rust-toolchain.toml). Pip installs the Maturin build
backend and Python runtime dependencies automatically.

```console
git clone https://github.com/puneet-chandna/requests-native.git
cd requests-native
python -m venv .venv
```

Activate the environment with `source .venv/bin/activate` on Linux or macOS,
or `.venv\Scripts\Activate.ps1` in Windows PowerShell. Then install:

```console
python -m pip install .
python -c "import requests; from importlib.metadata import version; print(version('requests-native'), requests.__version__, requests.__file__)"
```

The versions should be `1.0.0b1` and `2.34.2`. Use a fresh environment:
`requests-native` and upstream `requests` both provide the `requests` import,
so installing them together can overwrite package files. Other packages that
declare a dependency on the distribution named `requests` can still cause pip
to install upstream Requests. A different distribution name does not satisfy
that dependency.

## Rust

The native Rust API does not require Python. After cloning the repository,
add a path dependency to your application's `Cargo.toml`. This example assumes
your application directory is beside the `requests-native` checkout:

```toml
[dependencies]
requests-native = { path = "../requests-native/crates/requests" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The Cargo package is `requests-native`; the Rust library name is
`requests_native`. There is no registry package to install yet.

### Async

```rust
use requests_native::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send().await?;
    println!("{}", response.status());
    println!("{}", response.text().await?);
    Ok(())
}
```

### Blocking

Blocking support is enabled by default. This example does not need a Tokio
runtime in the application:

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.status());
    println!("{}", response.text()?);
    Ok(())
}
```

Keep a client around to reuse its connection pool. The Rust API is a separate
interface; Python conveniences such as `response.json()` are not Rust methods.

## Compatibility and current limits

Unmodified built-in Python `Session` and `HTTPAdapter` traffic uses the native
Rust transport by default. The PyO3 extension connects that transport to the
Python API. Python wrappers and compatibility fallbacks remain part of the
implementation: subclassing, monkeypatching, custom adapters, and unsupported
dynamic behavior can require Python authority. Those requests fall back before
native I/O begins.

This is not a pure Rust replacement of every Python code path. Strict
compatibility remains the goal. The Windows TLS issue is a disclosed beta
qualification exception, not proof of parity or a change to that goal; all
unrelated failures remain blockers. Published beta artifacts must carry an
exact-commit validation manifest recording any accepted Windows failures.
See [the architecture and porting guide](PORTING.md) and
[compatibility inventory](API_COMPATIBILITY.tsv) for the boundaries.

## Current performance evidence

The September 13, 2026 (local date) Linux loopback run measured the following
median throughput across 16 rows per surface:

| Surface | Median requests/s |
| --- | ---: |
| Frozen Python Requests oracle | 504.905 |
| Rust-backed Python API | 23.859 |
| Native Rust async API | 3,172.514 |
| Native Rust blocking API | 2,531.828 |

The Rust-backed Python API was slower than the frozen Python oracle in this
run. The native Rust APIs were faster. The run used commit `2a15969` with
non-runtime documentation/test changes in progress, not an immutable release
candidate. It does not establish performance for your workload.

The harness compares one-shot and pooled clients, buffered and streaming
reads, two body sizes, and serial and concurrent requests against the same
HTTP/1.1 loopback server. The default run uses only twelve requests per case,
without warm-up. Performance has no required target and does not override
compatibility. See the [method and limitations](benchmarks/README.md) and
[raw result](benchmarks/results/20260913-local-default.json). The
[September 2 result](benchmarks/results/20260902-local-default.json) remains
available as historical evidence.

## Names and versions

| Surface | Name | Version |
| --- | --- | --- |
| Python distribution | `requests-native` | `1.0.0b1` |
| Python import and compatibility baseline | `requests` | `requests.__version__ == "2.34.2"` |
| Rust package / library | `requests-native` / `requests_native` | `1.0.0-beta.1` |
| GitHub beta milestone | Requests Native | `v1.0.0-beta` |

The milestone name does not mean a release has been published. The Python
compatibility version stays separate from this project's release version.

## Contributing and security

Read the [contribution guide](.github/CONTRIBUTING.md) before opening a pull
request. Report suspected vulnerabilities privately according to the
[security policy](.github/SECURITY.md), never in a public issue.
The [code of conduct](.github/CODE_OF_CONDUCT.md) applies to project spaces.

## Attribution and license

This derivative retains Requests' Apache License 2.0, notice, history, API
documentation, and original contributor record. See [LICENSE](LICENSE),
[NOTICE](NOTICE), [AUTHORS.rst](AUTHORS.rst), and [HISTORY.md](HISTORY.md).
Requests itself was created by Kenneth Reitz and is maintained upstream by the
PSF Requests project. Changes specific to this Rust rewrite are maintained by
[Puneet Chandna](https://github.com/puneet-chandna).

Requests Native is not affiliated with, sponsored by, or endorsed by the
upstream Requests maintainers or the Python Software Foundation. Its
independent name and this disclaimer describe provenance, not trademark
clearance.
