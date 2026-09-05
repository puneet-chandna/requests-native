.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _contributing:

Contributing to Requests Native
=============================

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

Documentation is reStructuredText under ``docs/`` and Markdown at the project
root and under ``.github/``. Keep changes focused and avoid unrelated generated
files.

Suspected vulnerabilities must be reported privately under the
`security policy
<https://github.com/puneet-chandna/requests-native/security/policy>`_.
