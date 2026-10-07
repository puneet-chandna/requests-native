.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _install:

Installing Requests Native
==========================

The Rust core ``requests-native`` version ``1.0.0`` is published on crates.io.
PyPI publication is planned for the coming days; publishing setup is being
finalized. The Python distribution is not published to PyPI yet. Install the
improved ``v1.0.0`` Python source below; stable Python wheels are not included
in that release.

Install the stable Rust crate
-----------------------------

Use Rust 1.98.1 or newer and add this dependency to ``Cargo.toml``::

    [dependencies]
    requests-native = "1.0.0"

The Rust import is ``requests_native``. Async clients use Tokio; the blocking
client is available with default features. See the repository README for both
examples.

Install Python from the 1.0.0 source
------------------------------------

Install CPython 3.10 or later, Git, Rust through
`rustup <https://rustup.rs/>`_, and a native C/C++ build toolchain. Windows
requires MSVC Build Tools. Install the pinned compiler and create a fresh
environment::

    $ rustup toolchain install 1.98.1 --profile minimal
    $ python -m venv .venv

Activate the fresh environment on Linux or macOS::

    $ . .venv/bin/activate

On Windows, use ``.venv\Scripts\activate.bat`` in Command Prompt or
``.venv\Scripts\Activate.ps1`` in PowerShell. Then install the tagged source::

    $ python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"

Confirm the installed distribution and compatibility identity::

    $ python -c "import requests; from importlib.metadata import version; print(version('requests-native'), requests.__version__, requests.__file__)"

The distribution version is ``1.0.0`` and ``requests.__version__`` is
``2.34.2``. Import it as ``requests``. Python distribution and Requests
compatibility versions intentionally identify different surfaces.

.. warning::

   Do not install this distribution alongside upstream ``requests``. Both own
   the same import paths and can overwrite each other's files. A dependency
   requiring the distribution named ``requests`` can reinstall upstream
   Requests; ``requests-native`` does not satisfy that package requirement.

Current Windows checks passed without accepted TLS failures. Historical
`issue #1 <https://github.com/puneet-chandna/requests-native/issues/1>`_ remains
open without a reproduced cause; those checks do not prove it fixed. Test
integrations with your own workload and see the `readiness record
<https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md>`_ for qualification details.

Historical Python beta wheels
-----------------------------

The older `v1.0.0-beta release
<https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0-beta>`_
retains its original ``1.0.0b1`` wheels for Linux x86-64, Windows x86-64 and
macOS Apple silicon, plus its original source archive and validation manifest.
These are historical beta artifacts, not stable 1.0.0 wheels. Prefer the
improved source above. To try a beta wheel, use a separate fresh environment
and install the downloaded file with ``python -m pip install /path/to/downloaded.whl``.

To install official PSF Requests from PyPI instead, follow the
`upstream installation guide <https://requests.readthedocs.io/en/latest/user/install/>`_.
