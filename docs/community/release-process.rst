Release process
===============

Requests Rust releases are GitHub milestones for the independent rewrite.
They are not PSF Requests releases and are not published to PyPI or crates.io.
The Python distribution remains named ``requests`` at compatibility version
``2.34.2`` during beta qualification.

The ``v1.0.0-beta`` milestone may be created as a GitHub prerelease only after:

1. local compatibility, Rust, metadata, and benchmark checks pass;
2. source CI is green for the exact commit;
3. the manually dispatched validation workflow produces one sdist, the full
   23-wheel matrix, and a validated artifact manifest; and
4. no source or workflow changes occur after that evidence is collected.

The workflow stores validation artifacts for review. It contains no PyPI or
TestPyPI deployment job. A future registry release requires a separate design,
version decision, ownership review, and explicit maintainer approval.
