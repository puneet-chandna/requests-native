.. _install:

Installing Requests Rust from source
=====================================

Requests Rust is beta software and is not published to PyPI or crates.io.
There is no ``pip install requests-rust`` package. The distribution and import
names intentionally remain ``requests`` for compatibility.

Install Python 3.10 or later, Rust, and Maturin, then build from this
repository::

    $ git clone https://github.com/puneet-chandna/requests-rust.git
    $ cd requests-rust
    $ python -m venv .venv
    $ . .venv/bin/activate
    $ python -m pip install "maturin>=1.13,<2"
    $ python -m maturin develop

On Windows, activate the environment with
``.venv\Scripts\activate`` before running Maturin.

The resulting package reports compatibility version ``2.34.2``. The GitHub
tag ``v1.0.0-beta`` names the Rust-rewrite milestone and does not change that
Python package version.

To install official PSF Requests from PyPI instead, follow the
`upstream installation guide <https://requests.readthedocs.io/en/latest/user/install/>`_.
