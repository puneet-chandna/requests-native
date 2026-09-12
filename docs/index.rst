.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Requests Native
===============

Requests compatibility version |version|. GitHub prerelease
`v1.0.0-beta <https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0-beta>`_.

Requests Native is an unofficial, independent Rust rewrite of
`PSF Requests <https://github.com/psf/requests>`_. It preserves the familiar
Python API and routes pristine built-in HTTP traffic through a native Rust
transport, with compatibility fallback for unsupported extension behavior.

.. warning::

   This project is beta software. It is not affiliated with the Python
   Software Foundation and is not published to PyPI or crates.io. Install it
   from this repository's release assets or source. An unresolved
   `Windows TLS issue <https://github.com/puneet-chandna/requests-native/issues/1>`_
   is accepted for the beta. Follow :ref:`install` and test your own workload
   before production use.

The Python distribution is ``requests-native`` version ``1.0.0b1``. Its
drop-in import remains ``requests`` and reports compatibility version
``2.34.2``. The Rust package version is ``1.0.0-beta.1``. The GitHub beta tag
identifies the rewrite milestone; these versions intentionally describe
different surfaces.

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
