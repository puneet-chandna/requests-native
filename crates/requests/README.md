<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

[English](https://github.com/puneet-chandna/requests-native/blob/main/README.md) · [Español](https://github.com/puneet-chandna/requests-native/blob/main/README.es.md) · [简体中文](https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md) · [Français](https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md) · [हिन्दी](https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md) · [日本語](https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md)

Requests Native brings the familiar Python `requests` API to a shared Rust HTTP core.
The same core is available directly as native async and blocking clients for Rust.

[1.0.0 release](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0) ·
[crates.io](https://crates.io/crates/requests-native) ·
[Installation](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) · [Issues](https://github.com/puneet-chandna/requests-native/issues)

## Rust: stable 1.0.0

The Rust crate is published on crates.io. Use **Rust 1.98.1 or newer** and add:

```toml
[dependencies]
requests-native = "1.0.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The crate name is `requests-native`; the library import is `requests_native`.
Tokio is needed for the async example. The blocking client is included by default.

### Async

```rust
use requests_native::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send().await?;
    println!("{}", response.text().await?);
    Ok(())
}
```

### Blocking

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

Reuse a client to reuse its connection pool.

## Python: install the improved 1.0.0 source

PyPI publication is planned for the coming days; publishing setup is being
finalized. The Python distribution is **not published to PyPI** yet; use the
improved `v1.0.0` source now. This release does not include stable Python wheels.

You need **CPython 3.10+**, Git, the repository-pinned **Rust 1.98.1** toolchain,
and a native C/C++ compiler (MSVC Build Tools on Windows). Install Rust through
[rustup](https://rustup.rs/), then create a fresh environment:

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

Activate it on Linux/macOS:

```console
source .venv/bin/activate
```

Or in Windows PowerShell:

```powershell
.venv\Scripts\Activate.ps1
```

Install the tagged source:

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"
```

Use the familiar API:

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

The distribution is `requests-native` **1.0.0**; `import requests` and
`requests.__version__ == "2.34.2"` retain the Requests compatibility identity.

**Use a separate environment from upstream `requests`.** Both distributions
own the same import paths; dependencies on the package named `requests` can
reinstall upstream Requests. See [installation details](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst).

Current Windows checks passed without accepted TLS failures; the unreproduced historical [issue #1](https://github.com/puneet-chandna/requests-native/issues/1) remains open.

## Performance

The published 1.0.0 Rust core was measured in a local loopback comparison in five pairs, with 16 matched workloads per native client.

| Native client | Throughput vs original Requests | Throughput vs beta |
| --- | --- | --- |
| Async | 2.65x | 1.005x |
| Blocking | 2.23x | 1.055x |

Each value is the median of the 16 per-workload median ratios across five pairs.
Gains vary by workload; unstable oracle controls leave statistical uncertainty.
See the [methodology](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md) and [exact-source evidence](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md).

## Compatibility and development

Built-in `Session`/`HTTPAdapter` traffic uses the native transport. Custom
adapters, subclasses, monkeypatching and unsupported extension behavior can
fall back to Python before native I/O. Test integrations with your own workload.

See the [API compatibility inventory](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv),
[porting guide](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md) and
[contributing guide](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md).
Report vulnerabilities through the [security policy](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md).

## Attribution

Requests Native is an **unofficial, independent** rewrite of
[PSF Requests](https://github.com/psf/requests), maintained by
[Puneet Chandna](https://github.com/puneet-chandna). It is not affiliated with or
endorsed by the Python Software Foundation or upstream Requests maintainers.

The derivative retains Apache-2.0 licensing, notices, history and the original
contributor record. See [LICENSE](LICENSE), [NOTICE](NOTICE),
[AUTHORS.rst](AUTHORS.rst), [runtime notices](RUST_RUNTIME_NOTICES.html) and
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md).
