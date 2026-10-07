.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _install:

Installing Requests Native
==========================

The Rust core ``requests-native`` version ``1.0.0`` is published on crates.io.
Python publication is deferred and the Python distribution is not published to PyPI.
Its complete stable artifact set remains unqualified after a PyPy compatibility
failure. The Python distribution is named ``requests-native`` while its drop-in
import remains ``requests`` for compatibility.

Install the stable Rust crate
-----------------------------

Use Rust 1.98.1 or newer and add this dependency to ``Cargo.toml``::

    [dependencies]
    requests-native = "1.0.0"

The Rust import is ``requests_native``. Async clients use Tokio; the blocking
client is available with the default features. See the repository README for
examples. The published source is
``c1087413e54b7817a05d4080c3aaeca8e5c27db0``; later main documentation does not
change the released crate.

Install the beta wheel
-----------------------

Download the wheel matching your Python version, OS, and CPU architecture from
the `v1.0.0-beta release
<https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0-beta>`_.
The release covers Linux x86-64, Windows x86-64, and macOS Apple silicon.
Other targets require a source build and are not covered by this wheel matrix.

Create and activate a fresh virtual environment, then install the downloaded
wheel::

    $ python -m venv .venv
    $ . .venv/bin/activate
    $ python -m pip install /path/to/downloaded.whl

Replace the last path with the actual wheel filename. On Windows, activate
with ``.venv\Scripts\activate.bat`` in Command Prompt or
``.venv\Scripts\Activate.ps1`` in PowerShell.

.. warning::

   Do not install this distribution alongside upstream ``requests``. Both own
   the same import paths and can overwrite each other's files. A dependency
   that requires the distribution named ``requests`` can cause pip to install
   upstream Requests again; ``requests-native`` does not satisfy that package
   requirement.

The beta includes an unresolved `Windows TLS issue
<https://github.com/puneet-chandna/requests-native/issues/1>`_. Test your own
workload before considering production use.

Build Python from source
------------------------

Install Python 3.10 or later, Rust, and Maturin, then build from this
repository::

    $ git clone https://github.com/puneet-chandna/requests-native.git
    $ cd requests-native
    $ python -m venv .venv
    $ . .venv/bin/activate
    $ python -m pip install "maturin>=1.15,<2"
    $ python -m maturin develop

On Windows, use the activation command above before running Maturin.

The built Python distribution reports version ``1.0.0`` through package
metadata, while ``requests.__version__`` reports compatibility version
``2.34.2``. The Cargo package version is ``1.0.0``. The existing GitHub tag
``v1.0.0-beta`` retains its original ``1.0.0b1`` / ``1.0.0-beta.1`` archives
and manifest; it does not provide stable 1.0.0 artifacts.

To install official PSF Requests from PyPI instead, follow the
`upstream installation guide <https://requests.readthedocs.io/en/latest/user/install/>`_.
