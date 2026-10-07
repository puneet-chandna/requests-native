.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Release process
===============

Requests Native releases are GitHub milestones for the independent rewrite.
They are not PSF Requests releases and are not published to PyPI or crates.io.
The stable candidate Python distribution and Cargo package are
``requests-native`` version ``1.0.0``. Metadata alone does not qualify a release.
The installed Python import remains ``requests`` and ``requests.__version__`` remains
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

For stable 1.0.0, qualification must use the strict installed suites without
the historical Windows exception. The owner accepts passing qualification on
current supported Windows runners while keeping issue #1 open; its historical
cause remains unproved.

Stable releases also require the full paired performance gate for the
candidate, with a recorded baseline and a ``passed`` decision. Run
``.venv/bin/python benchmarks/evaluate.py --base <baseline> --candidate HEAD --gate``
locally, or dispatch ``Release performance qualification`` on Namespace CI.
Retain the raw reports, comparison and logs. Smoke mode verifies execution;
regressions, inconclusive controls or insufficient evidence block performance
qualification. Runtime changes require fresh qualification. See the
`benchmark procedure <../../benchmarks/README.md>`_ and
`current readiness evidence <../dev/release-readiness.md>`_.

``Validate release artifacts`` builds the complete set when ``artifact_run_id``
is empty. To recover final assembly without repeating successful builds,
provide the existing qualified run ID. The workflow verifies its source and
24 successful build jobs, then validates the original archives. Later
documentation or validation-workflow corrections must not relabel those
archives as a newer source commit. Runtime or packaged-source changes require
fresh artifact qualification.

The workflow stores a ``release-set`` artifact for review. Both
``publish_pypi`` and ``publish_crates`` default to false; version tags do not
start registry publishing. Publication requires an explicit manual dispatch on
``main``, the exact ``1.0.0`` candidate source, and successful ``source_run_id``
and ``performance_run_id`` qualification runs at that same commit. The workflow
recomputes the full performance decision from the preserved raw paired reports
with the candidate's evaluator and the unchanged 20% budget. A same-source
control, smoke result, stale evaluator or missing evidence cannot qualify it.
The first stable upload requires the immutable beta baseline
``2146b22ed25951a5483cbb13d69dc551f99ff352``; choosing another baseline cannot
bypass this publication gate.

Before an upload job can start, the workflow verifies that its GitHub environment
exists, requires a human reviewer and permits only the ``main`` branch. Configure
``pypi`` and ``crates-io`` under repository Settings → Environments, with the
owner as required reviewer and a selected deployment branch named ``main``.
Allow the sole owner to approve their own dispatch. These free environments
were created and read back on 2026-10-07 at 05:15 UTC: owner ``puneet-chandna``
(user ID ``121252460``) is the required reviewer, self-approval is permitted,
and each has exactly one deployment rule for branch ``main``. Both currently
have no environment secrets. Repository administrators remain trusted to manage
these protection rules. Missing environments are a failure; the workflow does
not rely on automatically created environments.

The owner must create the registry accounts and verify their email addresses.
For PyPI, enable two-factor authentication and configure a pending Trusted
Publisher for project ``requests-native``, owner ``puneet-chandna``, repository
``requests-native``, workflow ``publish.yml`` and environment ``pypi``.
`PyPI pending publishers <https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/>`_
support the first upload without storing a long-lived token. They do not reserve
the package name. The protected job rechecks the complete release set immediately
before upload and publishes only its 23 wheels and sdist; the manifest is retained
as qualification evidence.

`crates.io Trusted Publishing <https://crates.io/docs/trusted-publishing>`_
requires an existing crate. For the first upload, create a short-lived API token
limited to publishing ``requests-native`` and store it directly as the GitHub
``crates-io`` environment secret ``CARGO_REGISTRY_TOKEN``. Do not paste it into
chat or commit it. The job runs a locked publish dry run and the extracted
archive's unit tests before exposing the token to the upload step. Only the core
crate publishes; the Python binding remains private. After the first release,
revoke the bootstrap token and configure a Trusted Publisher for subsequent
releases. Account and publisher configuration do not declare the candidate ready.

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
