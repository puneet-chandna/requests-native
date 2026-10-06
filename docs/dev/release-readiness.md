# Requests Native 1.0.0 readiness

Audit date: 2026-10-07. Development remains on main. The owner has approved pushing the reviewed
changes and running remote qualification; the candidate is not yet stable.

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
| [2](https://github.com/puneet-chandna/requests-native/security/code-scanning/2), [1](https://github.com/puneet-chandna/requests-native/security/code-scanning/1) | Implicit TLS defaults in test client/server contexts | Local contexts reported `MINIMUM_SUPPORTED`; both flagged locations now explicitly require TLS 1.2 or newer. Existing TLS/loopback checks pass. |
| [6](https://github.com/puneet-chandna/requests-native/security/code-scanning/6), [5](https://github.com/puneet-chandna/requests-native/security/code-scanning/5) | SHA-512/SHA-256 used with passwords | These functions implement HTTP Digest challenge-response, not stored password hashing. The algorithm and wire values are required by the protocol and Requests compatibility. Replacing them with a password-storage KDF breaks authentication. |
| [4](https://github.com/puneet-chandna/requests-native/security/code-scanning/4), [3](https://github.com/puneet-chandna/requests-native/security/code-scanning/3) | SHA-1/MD5 used with passwords | Same protocol flow, with real weaknesses in legacy algorithms. Compatibility retains server-selected legacy Digest algorithms; no stronger-security claim or automatic dismissal. Prefer HTTPS and modern server authentication policies. |

The [CodeQL rule](https://codeql.github.com/codeql-query-help/python/py-weak-sensitive-data-hashing/)
addresses sensitive-data/password-storage hashing. The implemented flow is
`HTTPDigestAuth.build_digest_header`; [RFC 7616](https://datatracker.ietf.org/doc/html/rfc7616)
defines its challenge/response hashing and legacy-algorithm limitations.
No alert has been dismissed. CodeQL run
[37538258198](https://github.com/puneet-chandna/requests-native/actions/runs/37538258198)
on `3ed9c7da974dad5f976711fd491e807c4b726a80` closed alerts 7 and 1.
Client alert 2 remains open despite the TLS 1.2 minimum dominating its socket
wrap call. Runtime verification confirms TLS 1.2 minimum, required certificate
validation and hostname checks; no further runtime weakening or suppression is
justified. The four HTTP Digest alerts remain open for the compatibility reasons
above.

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
new historical comparisons use the updated shared dependency lock. GitHub must
refresh its graph after push before the alerts can be claimed closed.

Secret scanning and secret push protection
were disabled at the initial audit; both have now been enabled under the owner's
delegation. They are [free for this public repository](https://github.blog/changelog/2023-05-09-secret-scannings-push-protection-is-available-on-public-repositories-for-free/).
No paid feature was enabled. Generic/non-provider scanning and validity checks
remain disabled. The initial disabled endpoint was not a clean secret inventory;
the historical scan must complete before treating its inventory as assessed.
Private vulnerability reporting is enabled. Automatic dependency update PRs
stay paused because they can trigger extra Namespace CI work and runner cost;
alerts remain available for manual triage.

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
  matrix retains the existing signed-to-wheel Windows report and requires its
  status to pass with exit zero and no accepted failures before qualification.

## Local verification

- Native core: 317 tests passed; independently extracted crate: 107 unit tests
  passed. The new archive boundary regression check also passed.
- Native Python group: 213 passed, three skipped, nine deselected; the revised
  source-archive/install group covered the exclusions with nine passed and one
  skipped. These scopes overlap on the canonical source check; do not sum them.
- Frozen-oracle differential group: 2,638 passed, six skipped, clean exit zero
  in 21 minutes 14 seconds. The prior combined run reported passing assertions
  but exited with signal 143; these split runs verify clean process exit.
- Benchmark helper suite: 25 checks passed; final local HEAD/WORKTREE smoke:
  64 cases per revision, measured-phase RSS, preserved Python dependencies,
  restored environment and exit zero. It is intentionally inconclusive and
  provides no release performance qualification.
- Final workflow/workspace/artifact checks: 73 passed, one skipped, four
  deselected. Twine strict, actionlint, Ruff, Rust formatting and Windows
  harness self-check passed. These checks are local Linux evidence.

## Work still required before declaring 1.0.0

1. Resolve [Windows TLS issue 1](https://github.com/puneet-chandna/requests-native/issues/1)
   with a proven cause and fix. A passing isolated test or rerun is insufficient.
   The strict workflow is ready. The owner has now explicitly authorized its
   main push and dispatch. Strict installed release run
   [37538288557](https://github.com/puneet-chandna/requests-native/actions/runs/37538288557)
   passed at `3ed9c7da974dad5f976711fd491e807c4b726a80`; a pass establishes
   compatibility on that runner, not the cause of the intermittent failure.
   Repeat final qualification with the patched dependency environment.
2. Qualify the final exact source commit: full 23-wheel matrix, source archive,
   installed suites and artifact manifest, with no beta exception. Close the
   three `IN_PROGRESS` cross-cutting compatibility rows using this evidence.
   `python scripts/check_ledgers.py --completion` currently fails deliberately.
3. Enable core crate publication only after qualification. The archive now
   carries canonical README/legal notices, the referenced unit-test modules
   and all nine TLS fixture files. An extracted archive passes its 107 unit
   tests; this boundary is now a regression check. The core still deliberately
   has `publish = false` during beta qualification. The Python binding crate
   can remain private.
4. Complete performance qualification on the final candidate, calibrating the
   initial 20% budget and increasing samples if controls or CPU evidence are
   inconclusive. Smoke checks verify execution, not release performance.
5. Prepare stable version metadata and corresponding validators/docs only once
   the candidate qualifies: Rust `1.0.0`, Python distribution `1.0.0`, stable
   classifiers/history, while retaining `requests.__version__ == 2.34.2`.
6. Configure registry publishing credentials/Trusted Publishers, verify account
   ownership and add protected upload jobs. Public registry metadata returned
   HTTP 404 for `requests-native` on both PyPI and crates.io on 2026-10-07;
   no package currently exists under that name, but this does not reserve it.
   Current `publish.yml` validates artifacts and performs no PyPI/crates.io upload.

Core crate README/legal and TLS fixture copies are regular files for portable
Windows checkouts. When changing their canonical originals, update the matching
crate copies; the distribution check rejects divergence. Generated notice updates
must also be copied into `crates/requests/` before qualification.

The `requests-native` distribution preserves the `requests` import. It cannot
coexist with upstream Requests in one environment and does not satisfy pip
dependencies on a distribution named `requests`. This is an explicit identity
contract, not a packaging bug to hide during stable publication.
