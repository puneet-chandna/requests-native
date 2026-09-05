# Requests Native

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-beta-orange.svg)](https://github.com/puneet-chandna/requests-native/releases)

Requests Native is an unofficial, independent Rust rewrite of
[PSF Requests](https://github.com/psf/requests), maintained by
[Puneet Chandna](https://github.com/puneet-chandna). Its goal is strict
drop-in compatibility with the Requests Python API while moving pristine
built-in HTTP traffic through a native Rust transport.

> **Beta:** this repository is under compatibility qualification. It is not an
> official PSF Requests release, is not affiliated with the Python Software
> Foundation, and is not published to PyPI or crates.io. Do not replace a
> production Requests installation without testing your workload.

The Python distribution is `requests-native` at version `1.0.0b1`, while its
drop-in import remains `requests` and `requests.__version__` remains `2.34.2`
as the compatibility baseline. The Rust package is `requests-native` at
`1.0.0-beta.1`. The GitHub milestone remains `v1.0.0-beta`. These names and
versions describe different surfaces intentionally.

## Use from source

Requests Native is not yet published to PyPI or crates.io. To test it, clone
this repository and build it with Python 3.10+, Rust, and Maturin:

```console
git clone https://github.com/puneet-chandna/requests-native.git
cd requests-native
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin>=1.13,<2"
python -m maturin develop
```

The familiar API is preserved:

```python
import requests

response = requests.get("https://httpbin.org/get", timeout=10)
response.raise_for_status()
print(response.json())
```

Exact pristine `Session` and `HTTPAdapter` traffic uses the Rust path by
default. Unsupported extension, mutation, subclass, or custom-adapter behavior
falls back to the compatibility implementation before native I/O begins. See
[PORTING.md](PORTING.md) for the verified boundary and open qualification work.

## Current performance evidence

Performance is not a release gate and no target has been set. In the checked-in
Linux loopback run, median throughput across the 16 rows per surface was:

| Surface | Median requests/s |
| --- | ---: |
| Frozen Python Requests oracle | 672.374 |
| Rust-backed Python API | 23.931 |
| Native Rust async API | 3,052.193 |
| Native Rust blocking API | 2,616.302 |

The Rust-backed Python surface was slower than the oracle in this run; the
native Rust surfaces were faster. These are local loopback measurements, not
universal claims. See the [benchmark method](benchmarks/README.md) and
[raw result](benchmarks/results/20260902-local-default.json).

## Contributing and security

Read the [contribution guide](.github/CONTRIBUTING.md) before opening a pull
request. Report suspected vulnerabilities privately according to the
[security policy](.github/SECURITY.md), never in a public issue.

## Attribution and license

This derivative retains Requests' Apache License 2.0, notice, history, API
documentation, and original contributor record. See [LICENSE](LICENSE),
[NOTICE](NOTICE), [AUTHORS.rst](AUTHORS.rst), and [HISTORY.md](HISTORY.md).
Requests itself was created by Kenneth Reitz and is maintained upstream by the
PSF Requests project. Changes specific to this Rust rewrite are maintained by
Puneet Chandna.
