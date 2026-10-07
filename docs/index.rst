.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Requests Native
===============

Rust core 1.0.0 is stable on `crates.io <https://crates.io/crates/requests-native/1.0.0>`_.
The improved Python source is available from
`v1.0.0 <https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0>`_.
PyPI publication is planned for the coming days while publishing setup is
finalized. Follow :ref:`install` for the current source installation.

Requests Native is an unofficial, independent Rust rewrite of
`PSF Requests <https://github.com/psf/requests>`_. It preserves the familiar
Python API and routes pristine built-in HTTP traffic through a native Rust
transport, with compatibility fallback for unsupported extension behavior.

.. warning::

   The Python distribution is not published to PyPI yet and the 1.0.0 release
   does not include stable Python wheels. Use a fresh environment for the
   source install: upstream ``requests`` owns the same import paths. Test
   integrations with your own workload; see :ref:`faq` for compatibility details.

The Python distribution has ``requests-native`` version ``1.0.0`` metadata. Its
drop-in import remains ``requests`` and reports compatibility version
``2.34.2``. The Rust package version is ``1.0.0``. The existing GitHub beta tag
retains its original ``1.0.0b1`` / ``1.0.0-beta.1`` archives and manifest.
Distribution and compatibility versions intentionally describe different surfaces.

The User Guide
--------------

The inherited Requests user and API guides are retained as compatibility
documentation.

.. toctree::
   :maxdepth: 2

   user/install
   user/quickstart
   user/advanced
   user/authentication

The Project Guide
-----------------

.. toctree::
   :maxdepth: 2

   community/recommended
   community/faq
   community/out-there
   community/support
   community/vulnerabilities
   community/release-process
   community/updates

API Reference
-------------

.. toctree::
   :maxdepth: 2

   api

Contributing
------------

.. toctree::
   :maxdepth: 2

   dev/contributing
   dev/authors

Attribution
-----------

This derivative retains Requests' Apache License 2.0, notice, history, API
documentation, and original contributor record. Requests was created by
Kenneth Reitz and is maintained upstream by the PSF Requests project. The Rust
rewrite is maintained by Puneet Chandna.
