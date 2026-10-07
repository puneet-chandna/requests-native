# Requests Native 1.0.0 readiness

Audit date: 2026-10-07. Development remains on main. The owner has approved pushing the reviewed
changes and running remote qualification; the candidate is not yet stable.
Validation now proceeds locally first to limit runner cost. Do not repeatedly
dispatch paid CI while local source, packaging and benchmark checks can resolve
the problem; batch validated changes before any necessary platform qualification.

## Existing beta

The [v1.0.0-beta release](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0-beta)
already exists as a published prerelease. It contains 23 wheels, one source
archive and the artifact manifest, built from
`2146b22ed25951a5483cbb13d69dc551f99ff352`. It does not qualify current main.
No duplicate release or replacement of those immutable artifacts is needed.

## GitHub security findings

GitHub returned seven open CodeQL findings against main
`86cf68fb5d633ee2013d22c97e694b836c4c396f`.

| Alert | Finding | Verified response |
| --- | --- | --- |
| [7](https://github.com/puneet-chandna/requests-native/security/code-scanning/7) | Exponential regex in release build configuration guard | Reproduced a bounded subprocess timeout; replaced regex scanning with TOML parsing. Quoted/escaped keys and Windows case-insensitive Rust flag environment names are rejected. Invalid TOML fails quickly. |
| [2](https://github.com/puneet-chandna/requests-native/security/code-scanning/2), [1](https://github.com/puneet-chandna/requests-native/security/code-scanning/1) | Implicit TLS defaults in test client/server contexts | The server context lacked an explicit minimum. Both locations now explicitly require TLS 1.2 or newer; client certificate and hostname validation remain enabled. Existing TLS/loopback checks pass. |
| [6](https://github.com/puneet-chandna/requests-native/security/code-scanning/6), [5](https://github.com/puneet-chandna/requests-native/security/code-scanning/5) | SHA-512/SHA-256 used with passwords | These functions implement HTTP Digest challenge-response, not stored password hashing. The challenge/response wire values preserve Requests compatibility; SHA-256 is standardized by RFC 7616, while the retained full SHA-512 variant is a Requests extension. Replacing them with a password-storage KDF breaks authentication. |
| [4](https://github.com/puneet-chandna/requests-native/security/code-scanning/4), [3](https://github.com/puneet-chandna/requests-native/security/code-scanning/3) | SHA-1/MD5 used with passwords | Same protocol flow, with real weaknesses in legacy algorithms. Compatibility retains server-selected legacy Digest algorithms; no stronger-security claim or automatic dismissal. Prefer HTTPS and modern server authentication policies. |

The [CodeQL rule](https://codeql.github.com/codeql-query-help/python/py-weak-sensitive-data-hashing/)
addresses sensitive-data/password-storage hashing. The implemented flow is
`HTTPDigestAuth.build_digest_header`, which computes an HTTP Digest response rather
than storing a password hash. [RFC 7616](https://datatracker.ietf.org/doc/html/rfc7616)
defines the challenge/response construction and registers MD5, SHA-256 and
SHA-512-256. Requests also retains SHA-1 and full SHA-512 compatibility variants;
full SHA-512 is not RFC SHA-512-256.
On 2026-10-07, a direct comparison with the frozen Requests oracle at
`c5a69855228cc61120883a069e3cbeb78bd6f151` confirmed that the entire
`src/requests/auth.py` file is byte-for-byte identical, including all four
hashing branches, the challenge/response construction and resend flow. Its
SHA256 is `fdc8bb34a8a5a088b169ca13277d107b0bc94ee63ed5e89dd4f5569d9b2bb04c`.
The owner explicitly defers inherited Digest findings 3–6 for stable 1.0.0
to preserve the faithful rewrite. No algorithm behavior change, suppression
or dismissal is requested; the findings remain open.
No alert has been dismissed. CodeQL run
[37538258198](https://github.com/puneet-chandna/requests-native/actions/runs/37538258198)
on `3ed9c7da974dad5f976711fd491e807c4b726a80` closed alerts 7 and 1.
At the 2026-10-07 05:06 UTC refresh, the latest CodeQL analysis still targets
`29ef0b6908ac414e5e78309fe78fcc20760cde4a`: five open alerts and no analysis
error. It does not qualify the final local candidate. Client alert 2 remains
open despite the TLS 1.2 minimum dominating its socket wrap call. The exact
CodeQL 2.27.1 ssl model assumes default contexts allow TLS 1.0/1.1 and has a
minimum-version restriction model; the precise dataflow failure was not
reproduced locally. Executing the actual context construction and assignment
from the flagged test confirms TLS 1.2 minimum, `CERT_REQUIRED` and hostname
checks. The diagnostic profiler does not modify that policy. The reported
insecure-version behavior is contradicted by source and runtime evidence.
This separate finding concerns the custom Windows diagnostic TLS context,
so it is not covered by the inherited Digest deferral.
Keep the alert open pending final analysis or an explicitly approved
false-positive disposition. The four HTTP Digest alerts remain open for the
compatibility reasons above. No suppression or authentication change is needed.

The initial Dependabot inventory was empty; the refreshed dependency graph
reported four findings in the Windows diagnostic dependency lock. The lock now
uses urllib3 2.8.0 and Werkzeug 3.1.9. The normal installation floor is
`urllib3>=2.8,<3`, and the performance environment also pins 2.8.0. The legacy
1.26 CI lane retains behavioral coverage outside the supported installation
range. These changes address the advisory patch boundaries for
[proxy TLS](https://github.com/advisories/GHSA-8988-9cw3-xx77),
[unbounded chunk-size buffering](https://github.com/advisories/GHSA-vxq7-64xx-v4gw),
[Deflate streaming](https://github.com/advisories/GHSA-gh4c-6fx4-qh6g), and
[Werkzeug Windows device names](https://github.com/advisories/GHSA-g6x2-hccm-hh4m).
Historical diagnostic runs retain their original immutable dependency evidence;
new historical comparisons use the updated shared dependency lock. GitHub
marked all four alerts fixed after graph refresh on 2026-10-06 at 22:19 UTC
(2026-10-07 at 03:49 IST).

Secret scanning and secret push protection
were disabled at the initial audit; both have now been enabled under the owner's
delegation. They are [free for this public repository](https://github.blog/changelog/2023-05-09-secret-scannings-push-protection-is-available-on-public-repositories-for-free/).
No paid feature was enabled. Generic/non-provider scanning and validity checks
remain disabled. The initial disabled endpoint was not a clean secret inventory. On 2026-10-07
at 05:06 UTC, the enabled scanner API returned zero alerts, and the authenticated
GitHub UI reported zero open and zero closed alerts with no progress banner.
The historical scan completion timestamp could not be verified: the
[scan-history API](https://docs.github.com/en/rest/secret-scanning/secret-scanning#get-secret-scanning-scan-history-for-a-repository)
requires GitHub Advanced Security and returned HTTP 404 for this free public
repository. That API limitation does not indicate a scanner failure. Record
zero current provider alerts; do not claim a proven completed backfill or
coverage for disabled generic patterns.
Private vulnerability reporting is enabled. Automatic dependency update PRs
stay paused because they can trigger extra Namespace CI work and runner cost;
alerts remain available for manual triage.

RustSec `cargo-audit 0.22.2` checked 169 locked dependencies against advisory
database `ef6173cbc5c50ec8166f9a5b28f07834144373ee`: zero known vulnerabilities.
A fresh network-backed audit on 2026-10-07 at 05:16 UTC again passed all 169
locked dependencies with zero vulnerabilities and the two warnings below.
The database commit is dated 2026-10-03, but a separate upstream `main` ref
read confirmed it remains the current RustSec advisory revision after refresh;
this result does not rely solely on the earlier four-day-old cache.
Two maintenance warnings remain.
[Unmaintained rustls-pemfile](https://rustsec.org/advisories/RUSTSEC-2025-0134.html)
is a thin wrapper over the already-used rustls-pki-types parser; replacing it
requires preserving malformed-PEM error text and missing-key behavior.
[Yanked yoke-derive 0.8.3](https://github.com/unicode-org/icu4x/issues/8506)
accidentally required Rust 1.87 without declaring it; the pinned Rust 1.98.1
is unaffected. Version 0.8.4 remedies that metadata issue. Neither finding
identifies a vulnerability or explains the intermittent Windows failure. Treat
these as separate maintenance changes with fresh compatibility qualification.

## Gates added locally

- Every relevant source change runs native Rust tests, the existing Python/
  differential suites, distribution/fresh-install checks, ledger structure,
  Windows harness self-check and benchmark helper checks.
- The [performance evaluator](../../benchmarks/README.md) runs locally and in
  manual CI on `namespace-profile-puneet-chandna`, alternating warmed base/
  candidate pairs and retaining raw evidence. Regressions and inconclusive
  evidence fail release performance qualification.
- Linux jobs use Namespace. Logical platform names in artifact matrices remain
  stable; Windows/macOS jobs use GitHub runners.
- Strict Windows qualification tests the selected current commit's ordinary
  sanitized release wheel against the frozen oracle with the full installed
  suite. It excludes the historical diagnostic issue exception. The full wheel
  matrix retains the Windows report bound to the wheel's SHA256 and requires its
  status to pass with exit zero and no accepted failures before qualification.

## Earlier local verification

- Native workspace: 317 tests passed; independently extracted core crate: 107 unit tests
  passed. The new archive boundary regression check also passed.
- Native Python group: 213 passed, three skipped, nine deselected; the revised
  source-archive/install group covered the exclusions with nine passed and one
  skipped. These scopes overlap on the canonical source check; do not sum them.
- Frozen-oracle differential group: 2,638 passed, six skipped, clean exit zero
  in 21 minutes 14 seconds. The prior combined run reported passing assertions
  but exited with signal 143; these split runs verify clean process exit.
- Patched urllib3 2.8 preparation: 81 tests passed, plus 10 backend checks and
  three skips; the Rust URL admission guard passed. New helper and regex proofs
  retain replacement rejection. Invalid schemeless and trailing-dot numeric
  hosts preserve the oracle behavior. Independent review found no remaining
  actionable issue. Oracle/layout checks passed 60 checks and two additional
  checks with an exact frozen checkout nested inside the rewrite directory.
- Benchmark helper suite: 25 checks passed; final local HEAD/WORKTREE smoke:
  64 cases per revision, measured-phase RSS, preserved Python dependencies,
  restored environment and exit zero. It is intentionally inconclusive and
  provides no release performance qualification.
- Final workflow/workspace/artifact checks: 73 passed, one skipped, four
  deselected. Twine strict, actionlint, Ruff, Rust formatting and Windows
  harness self-check passed. These checks are local Linux evidence.

## Current correctness qualification

At `757dd9bc3021b71fdbf0edb325c3b43525d62b30`,
[source CI 37545318501](https://github.com/puneet-chandna/requests-native/actions/runs/37545318501)
passed on Namespace: the full rewrite/differential suite passed 2,863 tests
with 10 skips and a clean process exit. Native regression checks, canonical
artifact checks, benchmark helper checks and the legacy urllib3/no-character-
detection jobs also passed.

[Artifact CI 37545638504](https://github.com/puneet-chandna/requests-native/actions/runs/37545638504)
passed at the same source commit: all 23 wheels, the sdist, their installed
suites and the exact-source release manifest. All seven Windows wheels passed
without retries, wider fixture timeouts or accepted TLS failures. The owner
accepts current supported Windows qualification for 1.0.0 while keeping
[historical issue 1](https://github.com/puneet-chandna/requests-native/issues/1)
open; the older runner, compiler and dependency changes do not prove its cause.

The three cross-cutting compatibility rows now record this evidence. Strict
ledger completion passes all 366 API rows and 106 lifetime rows. Source CI
requires completion mode for future changes. These records establish the tested
beta candidate's correctness and packaging; they do not cover every extension
behavior. A local raw-object compatibility fix is undergoing qualification;
those earlier runs do not qualify its changed native code. Performance remains
a separate gate.
The ledger-closure commit `63f04bc58cb5228383d087da5131d0ffdd0d24f4`
also passed [source CI 37548904822](https://github.com/puneet-chandna/requests-native/actions/runs/37548904822)
with 2,862 tests, 10 skips and a clean exit, plus lint and security workflows.

## Work still required before declaring 1.0.0

At `29ef0b6`, local loopback probes confirmed that the native raw object rejected
`response.raw.url = response.url` and replacement of `response.raw.read`.
The local fix supports dynamic attributes, live stream callbacks and context
management, preserves lazy argument handling and iterator-close behavior, and
collects Python ownership cycles. The expanded focused run passed 314 tests with
five skips and exit zero. Sphinx 7.2.6
loaded both online inventories with zero warnings under `-E -W --keep-going`;
profiling observed two native raw context entries and exits. The proxy helper
dependency guards and canonical proxy wire target are corrected. Follow-up
frozen-oracle and socket regressions reproduced lost explicit default ports,
mixed-case raw hosts and lexical port normalization before the fix; all 19
targeted cases now pass. Signed port spellings fall back before native effects.
Independent source review found no remaining actionable issue. The installed artifact
suite now includes the eight new regression families, covering callbacks,
ownership, URL metadata and wire behavior on each qualified platform.
Complete fresh source, packaged and platform checks on this candidate before
declaring this blocker closed. Earlier beta qualification does not cover these
changes.

The fresh source/package run at `259ccdfacafc066eeaf40a64b54757ba66f4a478`
passed 2,924 rewrite/differential tests with ten skips and exit zero. The upstream
suite passed 620 tests with 15 skips and one pre-existing expected-failure test
that passed. A fresh canonical wheel and sdist passed strict Twine checks.
The source-authority inventory now passes with all 17 audited authority lines
and unchanged forbidden-token rules. The preceding inventory failure was fixed
by reviewing and recording the new parser dependency guard.

Local Python 3.10 CI-equivalent no-detector and legacy urllib3 lanes each passed
620 upstream tests, with 15 skips and one expected failure. Their boundary/property
checks passed 11 with three skips and 13 with one skip, respectively. After restoring
supported urllib3 2.8, all 62 selected raw/proxy regressions passed. These are local
Linux results; final platform/artifact qualification still remains.

The local/Namespace evaluator now supports fixed per-surface request counts,
so fast native cases can receive longer samples without multiplying the slower
Python/Rust workload. All 31 benchmark helper tests pass, including strict
matching-map and integer sample-count checks. The calibration vector in
`benchmarks/README.md` is unqualified; all four surfaces, warmed paired runs,
full metrics and the unchanged 20% regression budget remain required.

The five-pair local comparison against `v1.0.0-beta` at `259ccdf` completed all
ten reports with the trial per-surface counts and restored the environment.
It reported 240 passing metrics, 20 inconclusive metrics and no definite regression.
Oracle controls remained unstable, so release qualification failed; the evidence
is retained under `target/evaluations/local-gate-259-u1ZSFA/`. The local HEAD/HEAD
smoke also completed all 64 cases per side and restoration, without qualifying
performance. The laptop has distinct performance/efficiency CPU sets; this is a
possible noise source, not a proved cause. Affinity telemetry now records the
allowed CPU set. The three-pair oracle-only probe at `680ce0f`, pinned to CPUs
4–11, completed with 62 passing metrics and three inconclusive p95 latency
metrics. Individual paired latency ratios reached 1.21–1.46 despite identical
source revisions. CPU pinning did not resolve control instability; this probe
does not qualify release performance. Its raw reports are retained under
`target/evaluations/local-control-affinity-fn3j0pzq/`. Diagnose the remaining
control variability before repeating the full evaluation.

1. Qualify core crate publication on the final source. The archive now
   carries canonical README/legal notices, the referenced unit-test modules
   and all nine TLS fixture files. An extracted archive passes its 107 unit
   tests; this boundary is now a regression check. The core
   has `publish = true` in the stable candidate metadata; this permits a dry-run
   publication check, not an upload. The Python binding crate remains private.
2. Complete performance qualification on the final candidate, calibrating the
   initial 20% budget and increasing samples if controls or CPU evidence are
   inconclusive. Smoke checks verify execution, not release performance. Initial
   Namespace run [37538303779](https://github.com/puneet-chandna/requests-native/actions/runs/37538303779)
   completed four pairs before its 60-minute deadline. The preserved partial
   comparison had no definite regression, but noisy oracle controls and the
   missing fifth pair made qualification inconclusive. The job limit is now
   90 minutes; measurement deadlines, samples and budgets are unchanged.
   Manual CI now exposes the existing local pair/request/warm-up counts and
   per-case deadline, with a deliberate 90- or 180-minute job limit. Defaults
   and the 20% budget are unchanged. Invalid deadlines and undersized release
   samples fail before builds; all 26 benchmark helper checks passed.
   Full five-pair run
   [37545362862](https://github.com/puneet-chandna/requests-native/actions/runs/37545362862)
   finished with 179 passing and 81 inconclusive metrics, and no definite
   regression. Oracle controls were unstable, so it failed qualification.
   Retain all ten reports and investigate sampling/noise locally before spending
   on another remote evaluation; do not weaken the budget to obtain a pass.
3. Stable candidate metadata now uses Rust `1.0.0`, Python distribution `1.0.0`
   and the stable development classifier, while retaining
   `requests.__version__ == 2.34.2` and the existing free-threading classifier.
   Ten selected local workspace/workflow/identity checks, Ruff and the Windows
   harness self-check passed. The inherited Rust floor is the pinned `1.98.1`.
   Review and commit this preparation with the remaining source fixes, then
   qualify that final source once; a performance pass before metadata preparation
   is not required. Build and qualify its exact source commit and complete
   release set before release. Existing beta artifacts retain their original
   identities and correctly fail the stable validator.
4. Configure registry accounts and publisher credentials/Trusted Publishers.
   Protected opt-in jobs now reuse the existing validated release manifest;
   `publish_pypi` and `publish_crates` default to false. Publication requires
   a manual dispatch on main, exact `1.0.0` source identity, successful source
   and full performance runs at that commit, and a recomputed 20% performance
   gate against beta `2146b22ed25951a5483cbb13d69dc551f99ff352`. The preflight
   rejects missing environments, missing required reviewers and deployment
   branch policies other than main. The free public-repository environments
   `pypi` and `crates-io` were created and read back on 2026-10-07 at 05:15 UTC.
   Each requires owner `puneet-chandna` (GitHub user ID `121252460`) as reviewer,
   permits the sole owner to approve their own dispatch, and has exactly one
   deployment rule: branch `main`. Both have zero environment secrets.
   Repository administrators remain trusted to manage these rules; no
   undocumented admin-bypass API field is required. No paid feature was enabled.
   PyPI revalidates all archives immediately
   before upload; crates.io first runs a locked publish dry run and the
   extracted core archive's unit tests. Public registry metadata returned
   HTTP 404 for `requests-native` on both PyPI and crates.io on 2026-10-07;
   no package currently exists under that name, but this does not reserve it.
   No workflow has been dispatched to publish and no registry upload has occurred.
   The owner has not created registry accounts yet. PyPI needs a verified email
   and two-factor authentication; crates.io can use the owner's GitHub login
   and requires a verified email before publishing. Account creation remains
   an owner action; credentials must not be pasted into this chat. Register PyPI's
   pending publisher for `requests-native`, GitHub owner `puneet-chandna`, repo
   `requests-native`, workflow `publish.yml`, environment `pypi`. crates.io needs
   the first-upload token stored directly as `CARGO_REGISTRY_TOKEN` in the
   protected `crates-io` environment; revoke it after bootstrap, then configure
   Trusted Publishing for later releases. See the [release procedure](../community/release-process.rst).

Core crate README/legal and TLS fixture copies are regular files for portable
Windows checkouts. When changing their canonical originals, update the matching
crate copies; the distribution check rejects divergence. Generated notice updates
must also be copied into `crates/requests/` before qualification.

The `requests-native` distribution preserves the `requests` import. It cannot
coexist with upstream Requests in one environment and does not satisfy pip
dependencies on a distribution named `requests`. This is an explicit identity
contract, not a packaging bug to hide during stable publication.
