# Porting Requests from Python to Rust

This is the authoritative working guide for the Requests Rust port. Read it
before translating or reviewing any file.

The method is adapted from Bun's Zig-to-Rust migration: preserve behavior,
translate mechanically, document ownership before coding, keep uncertainty
visible, and let the existing tests define correctness.

## Status

This repository is in the preparation stage. Do not add Cargo manifests, Rust
source, extension-module code, or Python backend switches until the design and
the implementation plan are approved.

Frozen oracle:

| Item | Value |
| --- | --- |
| Package | Requests 2.34.2 |
| Commit | `69f84847045bef7a849cc994a26fe7ba8a169e95` |
| Commit date | 2026-07-20 |
| Python source | 19 modules, approximately 6,500 lines |
| Baseline suite | 635 collected cases |
| Valid local result | 619 passed, 15 skipped, 1 xpassed |
| Baseline Python | CPython 3.14.4 |
| Observed Rust toolchain | rustc/cargo 1.97.1 |

The valid baseline command is:

```bash
.venv/bin/python -m pytest -q
```

The tests open loopback sockets. A sandbox that prohibits local sockets does
not produce a valid behavioral baseline.

## Companion artifacts

- `docs/superpowers/specs/2026-07-24-requests-rust-port-design.md` states the
  approved architecture and scope.
- `API_COMPATIBILITY.tsv` inventories observable Python compatibility.
- `LIFETIMES.tsv` inventories state, ownership, cross-object references,
  threading, and cleanup.

Keep these files synchronized with confirmed discoveries. A changed
compatibility decision updates its test and ledger row in the same commit. A
changed field or ownership decision updates `LIFETIMES.tsv` before code.

Ledger states are explicit:

- API rows move from `NOT_PORTED` to `IN_PROGRESS` to `VERIFIED`.
- A genuinely inspection-only API row may use `INSPECTION_ONLY` only with a
  reviewed reason in its evidence column.
- Lifetime rows move from `PROPOSED` to `VERIFIED`.
- `REVIEW_REQUIRED` and `UNKNOWN` lifetime rows block implementation of their
  owning component.

Completion requires every applicable API and lifetime row to be `VERIFIED` or
an approved `INSPECTION_ONLY`; it is not enough for a row to lack an
`UNKNOWN` marker.

Ledger keys and evidence are also explicit:

- an API row is keyed by `(module, symbol, kind)`;
- a lifetime row is keyed by `(python_file, owner, field)`;
- a bare evidence description on a non-verified row states what evidence is
  still required;
- a `VERIFIED` API row must name `oracle:`, `test:`, and `review:` evidence,
  using exact source/test symbols and a review commit;
- a `VERIFIED` lifetime row must name `oracle:`, `rust:`, `test:`, and
  `review:` evidence;
- `INSPECTION_ONLY` must name `reason:` and `review:` evidence.

Do not change status first and promise to fill evidence later.

## Order of authority

When instructions conflict, use this order:

1. unchanged tests and behavior at the frozen oracle;
2. evidence-backed `API_COMPATIBILITY.tsv`;
3. frozen Python source;
4. documented Requests extension protocols;
5. this guide;
6. preferred Rust style.

Do not reinterpret the Python code into a cleaner API. Rust elegance comes
after parity.

## Hard rules

1. Translate behavior, not intent.
2. Preserve names, order of operations, defaults, mutation timing, and error
   timing unless the oracle proves they are unobservable.
3. Do not combine a port with a bug fix, cleanup, API redesign, or
   optimization.
4. Do not silently narrow accepted Python types.
5. Do not buffer a body or response merely to simplify Rust ownership.
6. Do not hold a borrowed Python reference across an `await`.
7. Do not hold the Python interpreter during network waits.
8. Do not let a panic cross FFI.
9. Do not add a dependency without a concrete compatibility, protocol,
   packaging, or platform requirement.
10. Do not make performance claims before parity and measurement.
11. Do not delete the Python oracle until all parity gates pass.
12. Do not merge a compile-only placeholder.

## What "mechanical" means

A mechanical translation keeps the original control flow recognizable:

- preserve function and stage order;
- preserve early returns and branching;
- preserve when state is copied versus shared;
- preserve whether input is consumed lazily;
- preserve exception boundaries;
- preserve callback order and replacement values;
- preserve dict insertion and adapter-prefix order;
- preserve redirect and authentication resend sequencing;
- use local helpers corresponding to the Python helpers;
- postpone deduplication and Rust-specific refactoring.

The translated code may initially be repetitive. Refactor only after
differential parity, and keep refactors behavior-neutral.

Mechanical does not mean knowingly incorrect. If a direct translation cannot
compile safely, stop, record the ownership question in `LIFETIMES.tsv`, and
resolve it explicitly. Do not replace it with a fake success path.

## Target architecture

The initial workspace has exactly two Rust crates:

```text
crates/
├── requests/          shared core and native async/blocking APIs
└── requests-python/   PyO3 boundary imported as _requests_rust
```

`crates/requests` uses Tokio for async execution and Hyper for HTTP. It owns
the default transport. It does not depend on Python or urllib3.

`crates/requests-python` converts Python values, preserves Python object
identity, invokes Python extension points, and maps errors. It does not contain
a second request pipeline.

`src/requests` remains the compatibility façade. At completion it contains
only the Python code required for import identity, dynamic protocols,
exception inheritance, pickling, and legacy aliases.

Public Requests classes remain ordinary Python heap types unless exhaustive
snapshots prove an internal PyO3 type indistinguishable. Rust handles stay
internal, and binding state that would add a new visible instance attribute
uses a weak identity side table.

The same async core drives:

- the native async Rust API;
- the native blocking Rust API;
- the synchronous Python API.

## Module map

Keep Rust module names aligned with Python names during translation.

| Python module | Rust/boundary destination | Translation rule |
| --- | --- | --- |
| `__init__.py` | Python façade | Preserve imports, warnings, logging, `__all__`, version exports |
| `__version__.py` | generated or Python constants | Preserve every constant and value |
| `_internal_utils.py` | `requests::internal_utils` | Mechanical string/header validation helpers |
| `_types.py` | Rust input traits + Python conversion code | Runtime protocols only; no public API expansion |
| `adapters.py` | `requests::adapters` + binding class | Built-in transport in Rust; custom subclasses cross to Python |
| `api.py` | core convenience functions + Python wrappers | Preserve signatures and one-shot session close |
| `auth.py` | `requests::auth` + callback bridge | Built-ins in Rust; arbitrary callables in Python |
| `certs.py` | Python façade/config input | Preserve `certifi.where` import identity |
| `compat.py` | Python façade | Preserve historical reexports and module choices |
| `cookies.py` | `requests::cookies` + cookie-jar bridge | Native default behavior; generic Python `CookieJar` support |
| `exceptions.py` | core error kind + Python classes | Preserve exact inheritance and pickle behavior |
| `help.py` | Python façade using Rust metadata | Preserve output schema and CLI behavior |
| `hooks.py` | `requests::hooks` + callback bridge | Preserve callback order and replacement semantics |
| `models.py` | `requests::models` + binding classes | Preserve mutable state, bodies, response laziness |
| `packages.py` | Python façade | Preserve module identity aliases |
| `sessions.py` | `requests::sessions` + binding class | Core pipeline, state merging, redirects, mounts |
| `status_codes.py` | `requests::status_codes` + binding object | Preserve all aliases and case variants |
| `structures.py` | `requests::structures` + binding classes | Preserve mapping order, casing, repr, equality |
| `utils.py` | `requests::utils` + selective Python façade | Preserve public helpers and platform behavior |

The first trial translates `structures.py`, `models.py`, and `adapters.py`.
This tests container semantics, ownership, streaming, transport, exceptions,
and Python callbacks before scaling the process.

## Core data flow

Preserve this sequence:

1. merge request settings with session settings;
2. construct `Request`;
3. prepare method;
4. prepare URL and parameters;
5. prepare headers;
6. prepare cookies;
7. prepare body and content length;
8. prepare authentication;
9. prepare hooks;
10. select the longest matching adapter prefix;
11. resolve environment, proxies, verification, and certificates;
12. send through the pool;
13. construct the response and extract cookies;
14. dispatch response hooks;
15. resolve redirects and authentication resends;
16. rewind only when the original body supports it;
17. return a stream or consume content;
18. release or close the connection exactly once.

Do not reorder preparation for convenience. Custom auth must see the prepared
body and may add hooks.

## Python-to-Rust type map

Use the narrowest type that preserves the Python contract.

| Python concept | Core Rust form | Boundary rule |
| --- | --- | --- |
| `None` | `Option<T>` | Distinguish omitted, explicit `None`, and empty when Python does |
| `bool` | `bool` | Never accept integer coercion unless Python does |
| unbounded `int` | checked integer or Python-owned value | Raise at the same boundary; never wrap |
| `float` timeout | validated duration representation | Preserve zero, `None`, tuple, and invalid-input behavior |
| `str` | `String`/`Cow<'_, str>` | Preserve encoding and error behavior; do not lossy-convert |
| `bytes`/`bytearray` | `Bytes`, `Vec<u8>`, or boundary object | Preserve mutability and identity only where observable |
| tuple/list | `Vec<T>` or fixed tuple | Preserve order and accepted iterables |
| dict/mapping | ordered typed map or Python mapping | Python path accepts arbitrary mapping protocols |
| `CaseInsensitiveDict` | insertion-ordered normalized map | Preserve last key casing and repr/equality |
| `LookupDict` | Python-compatible lookup object | Preserve dynamic attributes and `None` fallback |
| URL | validated string plus parsed view | Retain original formatting where Requests does |
| headers | ordered case-insensitive entries | Validate exact byte/text rules and duplicate behavior |
| cookie jar | native store or `CookieStore` bridge | External Python policies remain callable |
| hook/auth/adapter | trait object or callback handle | Exact built-ins use Rust fast path; dynamic objects use Python |
| file-like body | core `BodySource` plus origin binding bridge | Keep the strong Python owner on the origin thread; workers carry only typed Python-free actions/replies |
| iterator/generator body | streaming `BodySource` plus origin source | Never pre-consume; keep iterator state origin-owned and preserve exception timing |
| response body | stateful `BodyHandle` | Streaming, consumed, or closed states are explicit |
| Python exception | mapped error or pass-through handle | User exceptions keep identity and traceback |
| `datetime.timedelta` | core duration + Python conversion | Preserve elapsed definition and output type |

Do not use `HashMap` where insertion order is observable. Do not use lossy UTF
conversion. Do not collapse omitted and explicit `None` without oracle
evidence.

## Ownership classes

Every stateful field belongs to one class in `LIFETIMES.tsv`:

| Class | Meaning |
| --- | --- |
| `VALUE` | copied scalar or immutable owned value |
| `OWNED` | uniquely owned state |
| `SHARED_ARC` | thread-safe shared Rust state |
| `BORROWED` | scoped borrow that cannot outlive the call |
| `PYTHON` | strong Python-owned object held across calls |
| `CALLBACK` | callable Python or Rust extension point |
| `STREAM` | stateful producer/consumer with explicit close/drop |
| `HANDLE` | opaque resource or transport handle |
| `BACKREF` | reference to another request/response/transport object |
| `THREAD_LOCAL` | state isolated by calling thread |
| `UNKNOWN` | unresolved; implementation is blocked until reviewed |

Rules:

- An `UNKNOWN` row blocks translation of the owning field.
- A `BORROWED` value never crosses an async suspension point or FFI return.
- Python owners use strong handles, never raw pointers.
- Shared mutable Rust state uses explicit synchronization and a documented lock
  order.
- Request/response history must not create accidental reference cycles beyond
  Python's existing behavior.
- Close/drop behavior belongs in the ledger, not only in code.
- A reviewer must confirm any field moved from `PYTHON` to a typed Rust value
  remains mutable and observable as before.

## Dual representation rule

The native Rust API uses typed values. The Python API may supply arbitrary
objects. Do not force both through a single Python-shaped core type.

For Python wrappers:

1. keep Python-visible mutable attributes authoritative;
2. normalize a typed snapshot when entering a Rust stage;
3. retain strong owners for bodies and callbacks;
4. invoke the stage;
5. reflect observable mutations back to the wrapper;
6. after Python user code runs, normalize again before continuing.

Fast paths require exact built-in types or another proven immutability
condition. Subclasses and monkeypatched objects take the dynamic route.

Python resolves module globals when a method runs. For every translated method,
inventory the callable/module/constant globals it reads. The Rust fast path is
eligible only while those behavior-sensitive globals retain the frozen
identity or value. If, for example, a caller replaces `models.complexjson`,
`encode_multipart_formdata`, `check_header_validity`,
`sessions.dispatch_hook`, `preferred_clock`, or an adapter helper, the
replacement must affect behavior. Pass it through the caller-thread bridge
when exact, otherwise use the retained Python compatibility path.

### Built-in HTTPAdapter seam

The public Python adapter keeps the oracle's real visible state:

- `max_retries` is the actual urllib3 `Retry` object;
- `poolmanager` is an actual urllib3 `PoolManager`;
- `proxy_manager` contains the visible urllib3 proxy managers;
- `config`, pool settings, methods, reexports, and pickle state stay exact.

A binding-owned weak side table holds the Rust pool shadow. Do not add a
visible `_rust_*` field.

The unmodified built-in `send()` may use Rust only if:

1. `type(adapter)` is the exact built-in class;
2. relevant instance/class methods and module globals are pristine;
3. behavior-sensitive visible state matches the accepted shadow snapshot;
4. retry, timeout, proxy, TLS, and certificate values convert without
   narrowing behavior.

Otherwise dispatch through the Python compatibility implementation. Direct
calls to public adapter methods continue to use the visible urllib3 objects.
Pickling omits Rust shadow state and recreates it lazily. `close()` clears both
visible and Rust pool generations but does not make the adapter permanently
unusable.

The private adapter registry mutex protects only Rust-owned table state. Read
Python attributes, mappings, weak references, descriptors, and manager proofs
before taking that mutex. Keep destructive lifecycle epochs separate from
ordinary admission revisions and manager-proof revisions: concurrent sends
may merge native pool admissions without invalidating one another, while
`close()` still rejects an admission made after its destructive snapshot.
Direct/proxy manager refresh observes outside the mutex, then commits only
when the lifecycle and proof revision still match. Proxy refresh must
stable-copy the complete visible manager map, preserve and validate every
recorded immutable manager proof, admit only canonical new manager types, and
refresh mutable pool proofs without losing a concurrent manager. Move replaced
Python proof handles and evicted pools out of the table before dropping or
clearing them.

## Strings, URLs, and headers

- Preserve leading whitespace removal and scheme case behavior.
- Preserve IDNA conversion, invalid label errors, and Unicode host behavior.
- Preserve query parameter ordering, repeated values, byte values, fragments,
  and percent-escape rules.
- Do not send URL fragments.
- Preserve RFC 1808 redirect handling and non-ASCII redirect decoding.
- Preserve explicit `Host` behavior.
- Validate header names and values with the same accepted text/byte shapes.
- Preserve error classes and timing for newline, invalid type, and conflicting
  length cases.
- Preserve default header values and order where visible.
- Preserve `Content-Length` versus `Transfer-Encoding` decisions.

URL parsing libraries are helpers, not sources of behavior. Wrap or compensate
for any difference from the oracle.

## Bodies and multipart data

`BodySource` must model:

- known versus unknown length;
- seekable versus non-seekable;
- current and initial position;
- iterator exceptions;
- text versus byte chunks;
- JSON serialization failures;
- URL-encoded mapping/sequence ordering;
- multipart filename, content type, custom headers, and file tuples;
- close ownership;
- cancellation while producing a chunk.

Never call `len`, `tell`, `seek`, `read`, or iteration earlier than Python
does. The timing of user-code exceptions is observable.

Preserve the oracle's eager exception: Python `files=` multipart preparation
reads each file and constructs the complete multipart bytes body before send.
Do not turn it into streaming multipart. The no-new-buffering rule applies to
ordinary iterable and streamed file bodies that Requests already sends lazily.

Store any Python `tell()` result as an exact Python object. At prepared-request
redirect rewind, apply the oracle's `isinstance(value, int)` gate and pass an
accepted negative, Boolean, or arbitrarily large integer to `seek()` unchanged;
a non-integer raises `UnrewindableBodyError`. Digest auth is different: it
passes any non-`None` saved `tell()` result to `seek()`. Never narrow a Python
cursor to `u64`. Native Rust body offsets remain typed.

A redirect or digest resend may rewind only when `_body_position` is valid and
the body exposes a working seek operation. Preserve
`UnrewindableBodyError`.

## Sessions, mounts, and environment

- Preserve every `Session.__attrs__` field and pickle shape.
- Preserve mutable default headers, hooks, parameters, cookies, and adapters.
- Preserve one-shot API behavior: create, use, and close a temporary session.
- Preserve longest-prefix adapter matching and mount reordering.
- Preserve custom adapter object identity.
- Preserve `trust_env`, `.netrc`, `NO_PROXY`, case variants of proxy variables,
  scheme/host proxy keys, and proxy bypass platform behavior.
- Preserve stripping and rebuilding of authorization and proxy authorization.
- Preserve context-manager close behavior.
- Preserve close-then-reuse: `Session.close()` calls every mounted adapter's
  `close()` but does not mark the session closed.
- Repeated close is valid for the built-in adapter, the same session with
  built-in mounts may create fresh connections afterward, and a session close
  never stops the shared runtime driver or another session. A custom adapter's
  own `close()` behavior remains authoritative.
- Closing a session while a `stream=True` response is outstanding does not
  make that response unreadable. It may finish, but its lease is orphaned or
  closed on completion and must not re-enter the cleared pool generation.

The built-in adapter may use Rust only when Python overrides cannot be skipped.
Any subclassed or monkeypatched behavior must be observed.

## Redirects and resends

- Preserve redirect statuses, limit, history order, and `.next`.
- Preserve POST-to-GET rules for 301/302/303 and HEAD exceptions.
- Preserve 307/308 method and body behavior.
- Preserve fragment carry-forward and replacement.
- Preserve content header removal when the method/body changes.
- Consume or close the prior response before reusing its connection.
- Extract and merge cookies at the same stages.
- Re-evaluate proxies and authentication for the new URL.
- Preserve the generator mode of `resolve_redirects`.
- Preserve body rewind failure instead of silently sending an empty body.

Digest authentication is a response-hook-driven resend and follows the same
connection and history rules.

## Hooks and authentication

- `response` remains the only built-in hook event.
- Hooks run in registration order.
- A non-`None` hook return replaces the response passed to later hooks.
- Hook keyword arguments and user exceptions remain unchanged.
- Custom auth is any callable accepted by the oracle.
- Tuple auth keeps Basic Auth coercions, Latin-1 encoding, and deprecation
  warnings.
- Digest auth preserves per-thread state, nonce counting, challenge parsing,
  body position, one retry, redirect reset, and supported algorithms.
- Proxy auth is separate from origin auth.

Never invoke a Python hook or authenticator while holding a Rust lock needed by
the request pipeline.

Invoke Python auth, hooks, adapter methods, body/file/iterator protocols,
cookie policies, selected JSON/detector modules, clocks, and monkeypatched
globals on the OS thread that entered the synchronous Requests call. This
preserves `threading.local` and callback thread identity.

## Cookies

- Preserve `RequestsCookieJar` inheritance and mutable-mapping behavior.
- Preserve generic `http.cookiejar.CookieJar`, `LWPCookieJar`, and custom
  policy support.
- Preserve duplicate-name conflict behavior by domain/path.
- Preserve quoted value cleanup, `Morsel` conversion, expiry, merge, copying,
  and pickle locking behavior.
- Preserve cookie extraction on normal responses, redirects, and digest
  resends.
- Preserve caller-visible jar identity where the oracle does.
- Treat the Python jar and its directly mutable inherited `_cookies` hierarchy
  as authoritative. Native cookie state is a stage-scoped snapshot, not a
  competing owner.
- Apply Rust-produced cookie changes on the origin thread and resnapshot after
  hooks, policies, or direct Python mutation.

The native cookie store may optimize a pristine exact built-in jar only after
parity evidence. It must use Python for an external/changed jar or policy it
cannot reproduce exactly, and it must never overwrite an unseen Python
mutation.

## Responses and streaming

Response body state is one of:

1. streaming and not consumed;
2. partially consumed;
3. fully consumed and cached;
4. closed;
5. failed.

Preserve:

- lazy `.content`;
- `_content` and `_content_consumed` meaning;
- iterator chunk sizes and type checks;
- `iter_lines` delimiter and pending-line behavior;
- Unicode incremental decoding;
- decompression and chunked-transfer errors;
- apparent encoding and explicit encoding;
- JSON BOM/UTF detection and alternate decoder behavior;
- `.ok`, redirect properties, `.links`, and status exceptions;
- built-in Rust `raw` file-like operations used by the compatibility suite;
- exact `response.raw is resp` identity when `HTTPAdapter.build_response` or a
  custom adapter supplies a Python raw object;
- response pickling consuming content and clearing `raw`;
- `close()` behavior and idempotence.

Do not return a connection to the pool while a caller can still read the
stream.

Only clean protocol EOF makes a connection reusable. Explicit close, drop,
cancellation, or protocol failure before EOF closes the Rust-owned connection
and releases the lease without returning it dirty. A decoder error follows
urllib3's observable ownership lifecycle: the first error leaves the raw
response open and retains its lease; a later read may observe EOF and release
it, while explicit close releases it immediately. The failed connection is
never returned dirty.
Custom Python `raw` objects receive `close()` and optional `release_conn()` on
explicit `Response.close()` exactly as today. Dropping the Response only
releases its strong reference; do not add new close callbacks during wrapper
drop.

## Async and blocking rules

The async pipeline is the implementation. Do not duplicate it for sync use.

- Native async methods return futures/streams and are cancellation-safe.
- Native async futures are polled on the caller's Tokio runtime; they do not
  route through or own the background driver.
- Native blocking methods submit to a shared runtime driver and wait without
  constructing a nested runtime.
- Python sync methods use that same driver.
- Each Python entry records its OS thread and interpreter.
- Python releases the interpreter while its origin-thread action pump waits.
- A Tokio worker needing Python sends a typed action and suspends; the origin
  thread reacquires the same interpreter, executes it, and replies.
- This applies to Python body operations, hooks, auth, adapters, cookie
  policies, JSON/detector modules, clocks, module globals, and conversion.
- The pump supports a callback making a nested Requests call through the same
  shared driver; it never constructs a nested Tokio runtime.
- The pump remains signal-aware while no Rust action is pending. A Python
  signal cancels the core future, closes any non-clean lease, and propagates
  the original `BaseException` such as `KeyboardInterrupt`.
- Do not hold a core lock across a Python action.
- No Python `Bound` borrow survives an `await`.
- No Python user protocol runs silently on a Tokio worker.
- Runtime shutdown, response drop, and interpreter finalization must not
  deadlock.
- The driver and Rust pool side tables are shared only within one process.
  Detect a PID change/at-fork child, invalidate inherited handles without
  joining vanished threads, and initialize a fresh child driver and pools.
- Before planning the fork implementation, characterize the frozen oracle for
  import-before-fork and idle-session-before-fork. Do not claim active
  multi-threaded fork behavior without evidence.

The background driver belongs only to the native blocking and Python façades.
It is not stored as a mandatory dependency of the native async `Client`.

Free-threaded CPython is a real target. Do not treat the GIL as the only
synchronization mechanism.

## Error translation

The core error kind must retain enough context to map to:

- `RequestException`;
- `InvalidJSONError` and `JSONDecodeError`;
- `HTTPError`;
- `ConnectionError`;
- `ProxyError`;
- `SSLError`;
- `Timeout`, `ConnectTimeout`, and `ReadTimeout`;
- `URLRequired`;
- `TooManyRedirects`;
- `MissingSchema`, `InvalidSchema`, `InvalidURL`, `InvalidHeader`, and
  `InvalidProxyURL`;
- `ChunkedEncodingError`;
- `ContentDecodingError`;
- `StreamConsumedError`;
- `RetryError`;
- `UnrewindableBodyError`.

Also preserve warning types:

- `RequestsWarning`;
- `FileModeWarning`;
- `RequestsDependencyWarning`;
- existing `DeprecationWarning` cases.

Rules:

- preserve multiple inheritance;
- preserve `.request` and `.response` attachment;
- preserve exception arguments, meaningful messages, and pickle reduction;
- distinguish connect timeout from read timeout;
- pass Python user exceptions through unchanged;
- convert Rust panics to an internal failure before FFI, while treating any
  reachable panic as a bug.

For an admitted native adapter call, the Python-facing error graph is part of
the compatibility contract, not just the outer Requests class. The core stays
Python-free but retains typed phase data such as the OS error number and
incomplete-body byte counts. The binding must then reconstruct the
version-appropriate urllib3 graph with the actual visible pool and origin-form
URL before running the live Requests handler:

- use CPython exception-handler matching for `except` source globals, including
  full outer-tuple validation and the exact invalid-target `TypeError`; do not
  invoke source metaclass `__instancecheck__`;
- reload live globals and attributes in Python bytecode order when lookup is
  observable, including every `MaxRetryError.reason` access;
- wrap only urllib3's current decoder error classes as
  `DecodeError(message, original)` and terminalize the response body before
  live `requests.models` target construction;
- retain stable class, argument, pool/URL, cause, and context shape in both
  supported urllib3 2.7 and 1.26 lanes while leaving OS/resolver prose as an
  explicit platform boundary;
- finish every fallback-selecting proof before entering a visible manager or
  cache operation. Once manager entry begins, later proof or mapping failure
  must propagate on the native path and must never replay the complete Python
  send.

Task 14 proves these rules only through the private opt-in adapter/response
trials. Public Session dispatch, default-backend selection, and the wider
platform matrix remain later integration boundaries.

The fifth Task 14 fix and the subsequent user-authorized recovery tighten the
private adapter proof boundary:

- registration owns the exact initially empty `proxy_manager` dict; a
  replacement exact dict or any subclass is incompatible before manager entry,
  and snapshots use the native exact-dict copy API rather than a dynamic
  `copy()` callback;
- direct and proxy mutable-pool proofs bind one `pools` object and one
  `_container`, derive visible identities/count from that mapping, and
  self-validate before committing, so a concurrent insertion yields a retry
  rather than a torn proof;
- an unrecorded HTTP proxy manager is admissible only when its exact type,
  normalized map-key destination, pool sizing/blocking, proxy headers, alias
  fields, default proxy configuration, and exact routing-map identities match
  the admitted send snapshot and independently frozen, still-pristine
  `pool_classes_by_scheme` and `key_fn_by_scheme` sources;
- SOCKS admission is separate: the exact map-key string must also be the
  manager's `proxy_url`, and the six `_socks_options` fields must match the
  parsed scheme, host, explicit-or-`None` port, Requests-decoded credentials,
  version, and remote-DNS mode; its exact routing maps must likewise match the
  independently frozen SOCKS pool-class and shared key-function sources;
- every manager observation and proof-revision retry is capped at eight
  attempts. Exhaustion before manager entry selects compatibility; exhaustion
  after manager entry raises the existing hard-commit error and never replays;
- no rejected-attempt realm grants later re-admission. A visible proxy manager
  without an already-recorded immutable proof—including repaired, replaced, or
  independently preused state left after a rejected committed attempt—selects
  compatibility. If removed, a later newly created manager may be considered
  only through the complete canonical checks above.

This evidence is for GIL-enabled CPython 3.14. It does not convert the
free-threaded Python row into a completed claim.

A synchronous `catch_unwind` probe proves only a direct same-thread FFI guard;
it does not prove the blocking worker boundary. Worker-panic evidence must
submit a panicking task to the actual shared `BlockingRuntimeDriver`, observe
the submission's `WorkerStopped` result, map the fixed Python `RuntimeError`,
and then prove a normal task and existing transport state reuse the same
driver generation. The probe contains the boundary for testing; production
code must still remove every reachable panic.

## Compatibility modules

`requests.compat` and `requests.packages` are compatibility APIs even though
they do not drive the Rust transport.

Preserve:

- chosen character-detection module;
- `json`/`simplejson` resolution, serialization/decoding behavior,
  monkeypatching, and `JSONDecodeError`;
- urllib parse/request reexports;
- legacy type aliases and tuples;
- urllib3, idna, and chardet/charset-normalizer module identities under
  `requests.packages`;
- `urllib3_version` and `is_urllib3_1`;
- import-time dependency warnings;
- urllib3 `DependencyWarning` filter;
- Requests `NullHandler`;
- default `FileModeWarning` filter.

Do not remove an installed Python dependency solely because Rust no longer uses
it for transport. Remove dependencies only after compatibility evidence shows
their namespace and behavior are no longer required.

## Native Rust API rules

The native API is idiomatic but shares core types:

- async `Client`;
- `blocking::Client`;
- request builders and convenience verbs;
- typed headers, timeouts, proxy/TLS settings, bodies, and errors;
- streaming request and response bodies.

Do not expose Python compatibility artifacts in the native API. Do not create a
third crate just to separate async and blocking methods.

Exact public signatures are fixed in the implementation plan and then covered
by Rust compile tests.

## Rust idiom map

Use Rust idioms only when they preserve timing and behavior:

| Python | Rust |
| --- | --- |
| early `raise` | early `Err` with mapped kind |
| `try/finally` | scoped guard or explicit cleanup path |
| context manager | guard plus explicit `close`/`Drop` semantics |
| generator | stream/state machine; preserve laziness |
| callback list | ordered vector; no parallel dispatch |
| duck typing | boundary protocol check or trait object |
| mutable public attr | wrapper-authoritative field with resnapshot |
| shallow copy | explicit field-by-field clone matching Python |
| thread-local | thread-local state, not task-local, when observable |
| `OrderedDict` | insertion-ordered representation |
| sentinel `object()` | dedicated enum state, not `None` |
| `False` content sentinel | explicit response-body enum state |

Avoid clever iterator chains during the mechanical phase when they obscure
control flow or error timing.

## Comments for unresolved and sensitive code

Use these prefixes consistently:

- `TODO(port):` unresolved source behavior. Include source file/line or an
  issue. A reachable TODO blocks completion.
- `PORT NOTE:` non-obvious compatibility reason.
- `PERF:` measured optimization opportunity, deferred until parity.
- `SAFETY:` proof for an approved unsafe block.

Do not use a comment as permission for an empty implementation.

## Forbidden placeholders

Never merge:

- `todo!()`, `unimplemented!()`, or reachable `panic!()`;
- empty function bodies that return success;
- unconditional `Ok`, `None`, empty bytes, or default objects standing in for
  untranslated behavior;
- ignored errors used only to make compilation pass;
- disabled tests without a tracked compatibility decision;
- Python fallbacks that silently keep urllib3 as the default backend;
- fake mocks in production paths;
- broad `allow` attributes hiding unfinished code.

A temporarily non-compiling internal phase is acceptable while its single
delivery-task implementer is actively resolving ownership. It must not be
presented as a completed delivery gate. A deceptively compiling stub is not.

## Translation workflow

### Preparation gate

Before Rust source:

- approve the design;
- review all three preparation artifacts;
- write and approve the implementation plan;
- freeze the oracle and baseline commands;
- confirm early build smoke coverage for the full matrix;
- assign non-overlapping file ownership.

### Three-file trial

Translate, review, and correct:

1. `structures.py`;
2. `models.py`;
3. `adapters.py`.

Review whether this guide answered every ownership, dynamic Python, streaming,
and error question. Amend the guide and ledgers before scaling.

### Consolidated delivery-task loop

For each dependency-coherent delivery task:

1. assign one implementer for the entire task, including every internal phase
   and the resulting fixes;
2. record owned fields and references in `LIFETIMES.tsv`;
3. confirm its API rows;
4. copy each control-flow outline without cleanup;
5. translate types and error paths with focused red/green tests;
6. add differential tests for observable behavior;
7. make logical internal commits containing only the task-owned sources,
   tests, and ledger rows, and run the narrowest meaningful checks while
   developing;
8. run the task's full affected regression gate once before review;
9. request two independent adversarial reviews of the complete task in
   parallel;
10. require both reviewers to return their complete Critical/Important finding
    sets in that first pass;
11. give the same implementer one combined fix batch;
12. run one parallel re-review, adding another round only for a genuine
    remaining Critical/Important blocker;
13. commit only the exact task-owned sources, tests, ledger rows, and review
    record, then hand off remaining risks explicitly.

Internal phases retain their focused acceptance checks, but they do not each
start a separate implementer/reviewer/fixer cycle.

### Review roles

The implementer reports:

- translated source ranges;
- compatibility rows covered;
- lifetime rows changed;
- tests run and exact results;
- unresolved `TODO(port)` items;
- files changed.

Reviewer A compares Python and Rust control flow. Reviewer B attacks dynamic
behavior, ownership, cancellation, resource cleanup, and platform assumptions.
Both review the whole delivery-task diff and supporting evidence concurrently;
the package sent to each includes the exact commands and exact output, or
durable unabridged output artifacts. Neither reviewer waits for the other or
assumes that compilation proves parity.

The implementer is also the fixer. They reproduce the combined confirmed
findings, apply root fixes with focused tests, and run the relevant
differential and regression gates once before the parallel re-review.

## Main-branch coordination and git

The current port is developed directly on `main`; do not create worktrees or
review branches. Across the entire repository, at most one agent may edit
source/documentation or perform a mutating Git operation at any time.
Reviewers and every other parallel agent are read-only until the orchestrator
hands them a complete commit and evidence package.

Rules:

- never use `git stash`;
- never use destructive reset or checkout to discard work;
- never stage with a broad pattern when unrelated files exist;
- never overwrite user changes;
- never run competing merges or rebases;
- never amend another task's commit without coordination;
- use exact task-owned file commits with descriptive messages; never include
  unrelated dirty files;
- run full checks from the shared rewrite repository at delivery gates.

If a task discovers a shared-file requirement, the orchestrator resolves
ownership before the sole mutating implementer edits it. Parallel work may
prepare read-only oracle comparisons, attack matrices, or reviews, but may not
edit files, stage, commit, merge, rebase, or otherwise race the shared
worktree, index, or `HEAD`.

## Verification ladder

Run the smallest applicable rung first, then advance:

1. Rust unit tests for translated logic;
2. focused unchanged Python test file/case;
3. focused differential tests;
4. crate tests and lint/type checks;
5. full unchanged pytest suite;
6. API snapshot and pickle tests;
7. protocol integration tests with local servers;
8. property and fuzz tests;
9. Miri for ownership-sensitive components;
10. source and wheel builds;
11. installed-wheel tests across the full matrix;
12. resource, cancellation, and concurrency stress tests.

No reviewer may mark a row complete based only on reading code when runnable
evidence is possible.

### Differential oracle

Differential tests run the same input against:

- the frozen Python implementation;
- the Rust-backed implementation.

Compare values and side effects, not only success:

- type, repr, and attribute state;
- request bytes and header order;
- callback sequence and keyword arguments;
- exceptions, inheritance, arguments, and attached context;
- warnings;
- body reads/seeks and iteration count;
- redirect history and cookie state;
- connection close/reuse events.
- exact custom raw identity and explicit-close versus drop callback counts;
- callback OS-thread identity and nested Requests calls;
- repeated close, close-then-reuse, and isolation from other sessions;
- behavior after method, class, object, and module-global monkeypatches.
- signal interruption during a hung connect/read/write;
- import-before-fork and idle-session-before-fork behavior on supported POSIX
  systems;
- an active streamed response surviving `Session.close()` without rejoining
  the cleared pool generation.

Normalize only nondeterministic data such as elapsed time, random multipart
boundaries, digest cnonce, and ephemeral ports. Every normalization needs a
comment explaining why it is not part of the contract.

## CI and distribution

Preserve the current test matrix:

- CPython 3.10 through 3.15-dev;
- free-threaded CPython 3.14;
- PyPy 3.11;
- Ubuntu, macOS, and Windows;
- the existing PyPy/Windows exclusion;
- no-character-detector and urllib3 1.x compatibility jobs.

Add Rust checks without deleting Python compatibility jobs. Test both source
builds and installed wheels. A wheel test must run from outside the checkout so
it cannot import `src/requests` accidentally.

Do not claim free-threaded support based on a normal CPython wheel. Do not
claim PyPy support based on CPython ABI builds.

Preserve the frozen distribution surface:

- project metadata, Python requirement, classifiers, URLs, dependencies, and
  the `security`, `socks`, and `use_chardet_on_py3` extras;
- `requests/py.typed`, `LICENSE`, `NOTICE`, and expected artifact contents;
- an sdist plus the compiled wheel fan-out required by the platform/interpreter
  matrix;
- sdist installation with the documented Rust prerequisite;
- contributor editable installs;
- fresh-environment import and test execution from installed artifacts.

The implementation plan chooses the build backend only after an early matrix
smoke check. The compiled wheel fan-out may change; Python metadata and package
data may not.

## Benchmarks

Do not set latency, throughput, concurrency, or memory targets now.

After all compatibility gates pass:

- benchmark the frozen Python oracle and Rust backend on the same machine;
- compare one-shot and pooled requests;
- compare small and large bodies;
- compare buffered and streaming responses;
- compare concurrent clients;
- record CPU, allocations, memory, latency distribution, throughput, and
  connection reuse;
- publish raw commands and results.

Optimization happens from profiles and measurements. A benchmark regression
does not authorize changing compatibility silently.

## Completion gate

The backend switch is allowed only when:

- every required compatibility row has evidence;
- every ownership row is verified and none is `UNKNOWN` or
  `REVIEW_REQUIRED`;
- the unchanged pytest suite passes;
- differential and API snapshot suites pass;
- custom adapters, hooks, auth, cookie jars, bodies, and monkeypatch cases
  pass;
- async and blocking native APIs share the same pipeline;
- Python network waits release the interpreter;
- Python waits remain signal-interruptible and evidence-backed post-fork cases
  use a fresh child driver;
- supported source builds and wheels pass the matrix;
- frozen metadata/package data, sdist installs, editable installs, and publish
  artifact fan-out pass;
- no reachable placeholder remains;
- close, close-then-reuse, repeated close, cancellation, drop, streaming, and
  clean-EOF-only pool reuse are verified;
- closing a session with an active stream leaves that response readable but
  prevents its lease from rejoining the cleared pool generation;
- the Python default path no longer sends through urllib3;
- post-parity benchmarks can be run reproducibly.

## References

- [How Bun moved from Zig to Rust](https://bun.com/blog/bun-in-rust)
- [Bun's original `PORTING.md`](https://github.com/oven-sh/bun/blob/46d3bc29f270fa881dd5730ef1549e88407701a5/docs/PORTING.md)
- [The Bun port commit](https://github.com/oven-sh/bun/commit/46d3bc29f270fa881dd5730ef1549e88407701a5)
- [PyO3 free-threaded Python support](https://pyo3.rs/main/free-threading)
- [PyO3 multiple-Python-version guidance](https://pyo3.rs/main/building-and-distribution/multiple-python-versions.html)
