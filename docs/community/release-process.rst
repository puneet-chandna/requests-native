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

Public repository gate
----------------------

Repository visibility is an owner-controlled step and is separate from the
GitHub beta prerelease. Before making the repository public, the owner must
enable GitHub private vulnerability reporting and confirm that the
``Report a vulnerability`` path is available. Security reports use that path;
confidential conduct reports use the same path with ``Conduct:`` at the start
of the title. Until then, invited collaborators use an already-agreed private
channel with the owner and never public Issues.
