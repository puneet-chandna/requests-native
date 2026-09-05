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

No. Requests Native is not published to PyPI or crates.io. Build it from this
repository by following :ref:`install`. There is no ``requests-native`` package
on PyPI maintained by this project.

Why does it report version 2.34.2?
----------------------------------

The import surface reports ``requests.__version__ == "2.34.2"`` for strict
compatibility. The distinct ``requests-native`` distribution uses version
``1.0.0b1``, the Cargo package uses ``1.0.0-beta.1``, and the GitHub milestone
is ``v1.0.0-beta``.

Does every request use Rust?
----------------------------

Pristine built-in ``Session`` and ``HTTPAdapter`` traffic uses the native Rust
transport. Unsupported custom adapters, subclasses, monkeypatches, and dynamic
extension behavior fall back before native I/O begins. See ``PORTING.md`` in
the repository for the current evidence and boundary.

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
