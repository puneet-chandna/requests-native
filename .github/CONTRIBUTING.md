<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Contributing to Requests Native

Requests Native is an unofficial Rust rewrite of Requests. Use this repository
for bugs or improvements in the rewrite, its Rust transport, Python bridge,
builds, documentation, and compatibility boundary. General Requests usage
questions and issues reproducible in unmodified PSF Requests belong in the
[upstream Requests project](https://github.com/psf/requests).

Before opening an issue, search existing reports and test the current `main`
branch. Bug reports must include:

- a minimal reproduction, expected result, actual result, and full traceback;
- OS and architecture, Python implementation/version, and Rust version;
- installation method and commit or release tag;
- whether the request used the default Rust path or a compatibility fallback;
- relevant proxy, TLS, streaming, retry, custom adapter, subclass, or
  monkeypatch details.

For code changes:

1. Add the smallest regression test that fails before the fix.
2. Preserve strict Requests behavior, including exceptions and side effects.
3. Run the focused test, then the relevant Python and Rust gates locally.
4. Update compatibility evidence only for behavior actually verified.
5. Explain the reason for the change and paste exact verification commands in
   the pull request.

The real JSON compatibility test downloads a pinned, SHA-256-verified
simplejson wheel into an isolated test directory. For offline testing, set
`REQUESTS_SIMPLEJSON_WHEEL=/path/to/the/pinned-simplejson.whl`; the test checks
the same digest before extraction. The fixture URL and digest are recorded in
`tests_differential/test_compatibility_values.py`. It does not install
simplejson into your development environment.

Never report a suspected vulnerability in an issue; follow
[SECURITY.md](SECURITY.md). AI-assisted changes must follow
[AI_POLICY.md](AI_POLICY.md). All participation follows the
[code of conduct](CODE_OF_CONDUCT.md).
