.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _faq:

Frequently asked questions
==========================

Is this official Requests?
--------------------------

No. Requests Native is an unofficial, independent rewrite and is not affiliated
with PSF Requests or the Python Software Foundation. Upstream Requests remains
at `github.com/psf/requests <https://github.com/psf/requests>`_.

Can I install it from PyPI?
---------------------------

Not yet. PyPI publication is planned for the coming days; publishing setup is
being finalized. The Python distribution is not published to PyPI yet. The
Rust core ``requests-native`` version ``1.0.0`` is stable and
published on `crates.io <https://crates.io/crates/requests-native/1.0.0>`_. See
:ref:`install` for the Rust dependency and improved ``v1.0.0`` Python source.

Why does it report version 2.34.2?
----------------------------------

The import surface reports ``requests.__version__ == "2.34.2"`` for strict
compatibility. The distinct ``requests-native`` distribution uses version
``1.0.0`` metadata; the published Cargo package uses ``1.0.0``.
The existing ``v1.0.0-beta`` release retains its original ``1.0.0b1`` /
``1.0.0-beta.1`` archives and manifest.

Does every request use Rust?
----------------------------

Pristine built-in ``Session`` and ``HTTPAdapter`` traffic uses the native Rust
transport. Unsupported custom adapters, subclasses, monkeypatches, and dynamic
extension behavior fall back before native I/O begins. See ``PORTING.md`` in
the repository for the current evidence and boundary.

Are all Requests integrations supported?
----------------------------------------

Not every extension behavior is qualified. Current Windows checks passed
without accepted TLS failures. Historical
`issue #1 <https://github.com/puneet-chandna/requests-native/issues/1>`_ remains
open without a reproduced cause; those checks do not prove it fixed. Test
integrations with your own workload. The `readiness record
<https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md>`_ retains the complete qualification details.

Is it faster than Requests?
---------------------------

No general performance claim is made. The checked-in loopback result found the
Rust-backed Python surface slower than the frozen Python oracle while both
native Rust surfaces were faster. See the benchmark README and raw result;
repeat representative workloads before drawing conclusions.

Where should I ask a general Requests question?
------------------------------------------------

Use the `upstream Requests documentation <https://requests.readthedocs.io/>`_
or the `python-requests Stack Overflow tag
<https://stackoverflow.com/questions/tagged/python-requests>`_. This issue
tracker is for rewrite-specific defects and improvements.
