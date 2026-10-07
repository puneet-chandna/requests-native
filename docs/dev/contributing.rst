.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _contributing:

Contributing to Requests Native
===============================

Requests Native is an unofficial Rust rewrite of Requests. Contributions should
target the rewrite, Rust transport, Python bridge, build and validation tools,
documentation, or a demonstrated compatibility difference. General Requests
usage questions and bugs reproducible in unmodified PSF Requests belong in the
`upstream project <https://github.com/psf/requests>`_.

Before contributing, read the repository's
`contribution guide
<https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md>`_,
`code of conduct
<https://github.com/puneet-chandna/requests-native/blob/main/.github/CODE_OF_CONDUCT.md>`_,
and `AI policy
<https://github.com/puneet-chandna/requests-native/blob/main/.github/AI_POLICY.md>`_.

Code changes must include the smallest regression test, preserve strict
Requests behavior, and report exact local verification commands. Include all
observable differences: return values, exceptions, side effects, wire bytes,
and compatibility fallback behavior. Do not move a compatibility ledger row
to verified without concrete runnable evidence.

Build the editable package with ``python -m maturin develop``. Useful focused
gates include::

    $ python -m pytest path/to/test.py
    $ cargo test --workspace --all-targets --offline
    $ cargo fmt --all -- --check
    $ python scripts/check_ledgers.py

Run source CI locally before pushing
------------------------------------

Use an existing development environment with the pinned runtime/test dependencies,
maturin 1.15.0, and a frozen oracle checkout. On Linux, the source job's main checks can run
without GitHub or Namespace. Keep temporary pytest projects outside this Git
checkout: otherwise nested pytest commands can discover its configuration and
produce different module and JUnit identities. Use a fresh artifact directory
so a repeated build preserves older artifacts::

    $ source .venv/bin/activate
    $ export REQUESTS_ORACLE_ROOT=/absolute/path/to/frozen/requests
    $ export TMPDIR=/tmp
    $ mkdir -p target
    $ export REQUESTS_DISTRIBUTION_DIR=$(mktemp -d "$PWD/target/local-ci-XXXXXX")
    $ export SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
    $ python -m maturin develop --release --offline
    $ python scripts/check_oracle.py
    $ python -m pytest -q tests
    $ python -m pytest -q tests_rust/test_backend_boundary.py tests_differential/test_property_boundaries.py
    $ cargo test -p requests-native --locked
    $ cargo test --manifest-path benchmarks/rust-native/Cargo.toml --locked --target-dir target
    $ python scripts/check_ledgers.py --completion
    $ python scripts/compare_windows_tls.py --self-check
    $ python -m unittest benchmarks.test_run benchmarks.test_evaluate
    $ python scripts/build_release_wheel.py --interpreter "$(command -v python)" --out "$REQUESTS_DISTRIBUTION_DIR" --manylinux 2_34 --offline
    $ python -m maturin sdist --out "$REQUESTS_DISTRIBUTION_DIR"
    $ python -m pytest -q tests_rust tests_differential

The wheel build's ``--offline`` requires a populated Cargo cache; omit it for
the first dependency preparation. Keep other extension builds out of this
environment while tests run. Performance evaluation is a separate gate;
see ``benchmarks/README.md`` and use a dedicated evaluation environment.

``act`` can validate workflow orchestration locally with an explicit mapping
from ``namespace-profile-puneet-chandna`` to a Linux container image. A dry run
checks the workflow path, not execution of its tests. Container execution also
depends on tools available in that image and does not qualify Windows or macOS.
Batch locally validated changes before any necessary remote qualification.

Documentation is reStructuredText under ``docs/`` and Markdown at the project
root and under ``.github/``. Keep changes focused and avoid unrelated generated
files.

The pinned Sphinx dependency requires the distribution named ``requests`` and
can install upstream Requests alongside this project's editable package. Check
``requests.__file__`` before generating API docs; it must point to this
checkout's ``src/requests``. Keep documentation tooling in a disposable
environment to avoid changing an application environment.

Sphinx 7.2.6 exposed the historical native ``raw`` mutation gap. The current
local fix passes an online ``dirhtml`` build with both inventories and zero
warnings on October 7, 2026. Use ``-E -W --keep-going`` when checking this
integration so cached inventories or tolerated warnings cannot conceal a
failure. See ``PORTING.md`` and the readiness checklist for qualification scope.

Suspected vulnerabilities must be reported privately under the
`security policy
<https://github.com/puneet-chandna/requests-native/security/policy>`_.
