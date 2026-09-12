.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Release process
===============

Requests Native releases are GitHub milestones for the independent rewrite.
They are not PSF Requests releases and are not published to PyPI or crates.io.
The Python distribution is ``requests-native`` version ``1.0.0b1`` and the
Cargo package is ``requests-native`` version ``1.0.0-beta.1``. The installed
Python import remains ``requests`` and ``requests.__version__`` remains
``2.34.2`` as the compatibility baseline.

Current beta
------------

The `v1.0.0-beta prerelease
<https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0-beta>`_
contains 23 wheels, one sdist, and an exact-commit validation manifest. Its
source commit is ``2146b22ed25951a5483cbb13d69dc551f99ff352``.
`Platform validation
<https://github.com/puneet-chandna/requests-native/actions/runs/34716443823>`_
completed with a narrowly accepted Windows failure under
`issue #1 <https://github.com/puneet-chandna/requests-native/issues/1>`_.
That exception is recorded in the manifest and is not a fix or a claim of
complete Requests parity.

`Final artifact validation
<https://github.com/puneet-chandna/requests-native/actions/runs/34719533263>`_
passed with workflow correction ``9d4c96f``. It reused the qualified archives
without rebuilding them or changing their source identity. Exact-source
artifact checks and strict Twine validation passed before release upload.

Qualification procedure
-----------------------

A release requires:

1. local compatibility, Rust, metadata, and benchmark checks pass;
2. source and installed-artifact checks qualify the exact source commit, with
   any explicitly approved beta exception disclosed and recorded;
3. the manually dispatched validation workflow produces one sdist, the full
   23-wheel matrix, and a validated artifact manifest; and
4. uploaded archives and checksums match that qualified release set.

``Validate beta artifacts`` builds the complete set when ``artifact_run_id``
is empty. To recover final assembly without repeating successful builds,
provide the existing qualified run ID. The workflow verifies its source and
24 successful build jobs, then validates the original archives. Later
documentation or validation-workflow corrections must not relabel those
archives as a newer source commit. Runtime or packaged-source changes require
fresh artifact qualification.

The workflow stores validation artifacts for review. It contains no PyPI or
TestPyPI deployment job, and version tags do not start registry publishing.
Publishing to any registry remains a separate, explicit maintainer decision.

Python package metadata intentionally omits aggregate ``License`` and
``License-Expression`` fields for now. Maturin currently supplies one project
metadata value to both the source distribution and binary wheels, while their
applicable license expressions differ. Canonical license files, notices,
attribution, runtime supplement, and the wheel SBOM remain packaged and
validated. Machine-readable aggregate SPDX metadata is deferred until the
backend can represent each artifact accurately.

Release wheels must be built with ``scripts/build_release_wheel.py`` and a
fixed ``SOURCE_DATE_EPOCH``. The helper rejects competing Rust flags, computes
portable path remappings for the complete Cargo build, adds the deterministic
locked-graph SBOM through Maturin's supported interface, and validates the
wheel before copying it to the requested output directory. Only release wheels
built by this helper carry the path-sanitization guarantee. Direct Maturin,
editable, and PEP 517 builds remain supported development and downstream build
paths, but do not carry the release path-sanitization guarantee.

Private reporting
-----------------

The maintainer should enable GitHub private vulnerability reporting on the
public repository and confirm that ``Report a vulnerability`` is available.
Security reports use that path when available; confidential conduct reports
use the same path with ``Conduct:`` at the start of the title. If unavailable,
use an already-agreed private channel with the maintainer, or ask for a private
contact method without sharing sensitive details. Never disclose these reports
in public issues. See the repository's security policy and code of conduct.
