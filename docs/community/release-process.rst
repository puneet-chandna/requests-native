.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Release process
===============

Requests Native releases are GitHub milestones for the independent rewrite.
They are not PSF Requests releases and are not published to PyPI or crates.io.
The Python distribution is ``requests-native`` version ``1.0.0b1`` and the
Cargo package is ``requests-native`` version ``1.0.0-beta.1``. The installed
Python import remains ``requests`` and ``requests.__version__`` remains
``2.34.2`` as the compatibility baseline.

The ``v1.0.0-beta`` milestone may be created as a GitHub prerelease only after:

1. local compatibility, Rust, metadata, and benchmark checks pass;
2. source CI is green for the exact commit;
3. the manually dispatched validation workflow produces one sdist, the full
   23-wheel matrix, and a validated artifact manifest; and
4. no source or workflow changes occur after that evidence is collected.

The workflow stores validation artifacts for review. It contains no PyPI or
TestPyPI deployment job. Publishing to any registry remains a separate,
explicit maintainer decision.

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

Public repository gate
----------------------

Repository visibility is an owner-controlled step and is separate from the
GitHub beta prerelease. Before making the repository public, the owner must
enable GitHub private vulnerability reporting and confirm that the
``Report a vulnerability`` path is available. Security reports use that path;
confidential conduct reports use the same path with ``Conduct:`` at the start
of the title. Until then, invited collaborators use an already-agreed private
channel with the owner and never public Issues.
