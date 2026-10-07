.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

Release process
===============

Requests Native releases belong to the independent rewrite, not PSF Requests.
The stable Rust core ``requests-native`` version ``1.0.0`` is published on
`crates.io <https://crates.io/crates/requests-native/1.0.0>`_. Python registry
publication is deferred; its complete stable artifact set remains unqualified
after a PyPy compatibility failure. The existing Python beta is distributed on
GitHub. Stable Python metadata alone does not qualify a release.
The installed Python import remains ``requests`` and ``requests.__version__`` remains
``2.34.2`` as the compatibility baseline.

Published Rust core
-------------------

Protected publication
`37667367591 <https://github.com/puneet-chandna/requests-native/actions/runs/37667367591>`_
completed on 2026-10-07 at 19:05 UTC. The immutable package source is
``c1087413e54b7817a05d4080c3aaeca8e5c27db0``; Rust 1.98.1 is the supported
compiler floor. The registry archive is 199,704 bytes and its SHA256 matches
the qualified package:
``43170f424e6ec6c63939367d7680dde58a28a2fbf81f434daecf825bc78b1cbf``.
Source tests, locked publish dry run, exact-source/legal checks, extracted
archive tests and complete core performance recomputation preceded upload.
The owner approved the protected deployment; the workflow then verified the
public registry checksum. Later main documentation updates do not change this
released source or its package bytes.

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

A Python distribution release requires:

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

Stable releases also require complete paired performance evidence for the
candidate, with a recorded baseline. Release workflows accept ``passed`` or
complete ``inconclusive`` evidence with a visible warning under the owner's
approved policy. Run
``.venv/bin/python benchmarks/evaluate.py --base <baseline> --candidate HEAD --gate``
locally, or dispatch ``Release performance qualification`` on Namespace CI.
Retain the raw reports, comparison and logs. The statistical CLI keeps its
strict exit behavior and leaves raw statuses, ratios and 95% intervals
unchanged. Confirmed native regressions block release even when unstable
controls make the overall status inconclusive. Correctness failures,
insufficient samples/CPU/RSS evidence, malformed or missing reports, and
source/evaluator mismatches also block release. Smoke mode only verifies
execution. Warning acceptance does not establish performance parity or
superiority. Runtime changes require fresh qualification. See the
`benchmark procedure <../../benchmarks/README.md>`_ and
`current readiness evidence <../dev/release-readiness.md>`_.

``Validate release artifacts`` builds the complete set when ``artifact_run_id``
is empty. To recover final assembly without repeating successful builds,
provide the existing qualified run ID. The workflow verifies its source and
24 successful build jobs, then validates the original archives. Later
documentation or validation-workflow corrections must not relabel those
archives as a newer source commit. Runtime or packaged-source changes require
fresh artifact qualification.

New stable wheel fan-out, sdist, assembled release-set and full performance
evidence artifacts are retained for 30 days. Pull-request wheels retain five
days and smoke performance evidence retains 14 days. These periods apply to
new uploads, not existing artifact expirations. Reuse still requires the
original sdist and wheel fan-out; preserving only the assembled release-set
does not satisfy the current reuse path. Download required evidence before
expiry if account setup will take longer than the retention period.

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

The separate ``Qualify and publish immutable stable core`` workflow qualifies
only the Rust core at ``c1087413e54b7817a05d4080c3aaeca8e5c27db0``. It does not
require Python wheels or an sdist and does not authorize Python publication.
Its source Tests run must identify that exact commit. The owner approved using
complete locally measured ``core`` evidence, with Python oracle controls plus
Rust async and blocking across all 48 cases and five complete pairs. The
unchanged ten reports, comparison and explicit local-origin metadata are
committed at ``benchmarks/release-evidence/core-1.0.0-c108.tar.gz``. The bounded
archive is 5,733,313 bytes, with SHA256
``fafe8c5c0b5075485617058481d2efd0b1792d0f66b8d62658f0edd023ee42bf``.
Its fixed evaluator is ``b51b43005239ed1be632b7f570daa2f991860e49``; the workflow
verifies that driver is an ancestor of the current ``main`` evidence commit.
It checks exact archive bytes and inventory, local origin and source identities,
then recomputes the unchanged core decision using that driver's evaluator.
This Namespace job validates local measurements; it does not claim a successful
Actions benchmark run. The earlier incomplete full-scope remote run remains
failed and is excluded from qualification.

The raw local result remains ``inconclusive``: 148 metrics passed, 47 were
inconclusive and none showed a confirmed regression. Samples and CPU/RSS
evidence are sufficient, but oracle controls are unstable. The approved warning
policy applies without a parity or superiority claim. ``full`` remains the
default for Python plus Rust qualification; core evidence makes no Python
qualification claim. The workflow packages the immutable core source, checks
canonical notices and metadata, tests the extracted archive, and retains its
digest and explicit local-evidence/Namespace-validation provenance before the
protected upload job. Package bytes must match SHA256
``43170f424e6ec6c63939367d7680dde58a28a2fbf81f434daecf825bc78b1cbf``;
repackaging and the public registry checksum are also checked. ``publish``
defaults to false. One explicit dispatch may validate and then pause for the
owner's ``crates-io`` approval after the actual package digest is reviewed.

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

The owner must complete PyPI account and publisher setup before Python publication.
For PyPI, enable two-factor authentication and configure a pending Trusted
Publisher for project ``requests-native``, owner ``puneet-chandna``, repository
``requests-native``, workflow ``publish.yml`` and environment ``pypi``.
`PyPI pending publishers <https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/>`_
support the first upload without storing a long-lived token. They do not reserve
the package name. The protected job rechecks the complete release set immediately
before upload and publishes only its 23 wheels and sdist; the manifest is retained
as qualification evidence.

`crates.io Trusted Publishing <https://crates.io/docs/trusted-publishing>`_
requires an existing crate. The owner has stored the bootstrap token as the
GitHub repository secret ``CRATES_IO_API_TOKEN``. The upload step maps it to
Cargo's ``CARGO_REGISTRY_TOKEN`` variable. The protected ``crates-io`` job still
requires reviewer approval and the exact ``main`` branch before any step runs;
the token is repository-scoped, not an environment secret. Do not paste it into
chat or commit it. The job runs a locked publish dry run and the extracted
archive's unit tests before exposing the token to the upload step. Only the core
crate publishes; the Python binding remains private. The first release has
completed. Revoke the bootstrap token and configure a Trusted Publisher for subsequent
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
