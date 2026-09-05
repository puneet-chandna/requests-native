.. _install:

Installing Requests Native from source
=====================================

Requests Native is beta software and is not published to PyPI or crates.io.
There is no project-maintained ``pip install requests-native`` release yet.
The distribution is named ``requests-native`` while its drop-in import remains
``requests`` for compatibility.

Install Python 3.10 or later, Rust, and Maturin, then build from this
repository::

    $ git clone https://github.com/puneet-chandna/requests-native.git
    $ cd requests-native
    $ python -m venv .venv
    $ . .venv/bin/activate
    $ python -m pip install "maturin>=1.13,<2"
    $ python -m maturin develop

On Windows, activate the environment with
``.venv\Scripts\activate`` before running Maturin.

The built Python distribution reports version ``1.0.0b1`` through package
metadata, while ``requests.__version__`` reports compatibility version
``2.34.2``. The Cargo package version is ``1.0.0-beta.1``. The GitHub tag
``v1.0.0-beta`` names the rewrite milestone; these versions are separate by
design.

To install official PSF Requests from PyPI instead, follow the
`upstream installation guide <https://requests.readthedocs.io/en/latest/user/install/>`_.
