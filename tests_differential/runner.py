"""Run trusted differential cases without co-importing both implementations.

A case is ``{"source": "..."}``; source assigns ``result`` and appends to the
provided ``side_effects`` list.
"""

from __future__ import annotations

import contextlib
import json
import math
import os
import re
import subprocess
import sys
import types
import warnings
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ORACLE_ROOT = REPOSITORY_ROOT.parent / "requests"
_CASE_MODULE = "__differential_case__"
_CHILD_ARGUMENT = "--child"
_DEFAULT_TIMEOUT_SECONDS = 10.0
_TIMEOUT_ENVIRONMENT = "REQUESTS_DIFFERENTIAL_TIMEOUT"
_TARGET_ENVIRONMENT = "REQUESTS_DIFFERENTIAL_TARGET"
_DEPENDENCY_PATH_ENVIRONMENT = "REQUESTS_DIFFERENTIAL_DEPENDENCY_PATH"
_REWRITE_ROOT_ENVIRONMENT = "REQUESTS_DIFFERENTIAL_REWRITE_ROOT"
_MAX_DIAGNOSTIC_CHARS = 2048
_PRESERVED_ENVIRONMENT = (
    "COMSPEC",
    "DYLD_LIBRARY_PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LD_LIBRARY_PATH",
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "WINDIR",
)


@dataclass(frozen=True)
class CaseRun:
    observations: dict[str, Any]
    stderr: str


def run_oracle_case(case: dict[str, Any]) -> CaseRun:
    oracle_root = Path(
        os.environ.get("REQUESTS_ORACLE_ROOT", DEFAULT_ORACLE_ROOT)
    ).resolve()
    return _run_case(case, oracle_root / "src", "oracle")


def run_rewrite_case(case: dict[str, Any]) -> CaseRun:
    rewrite_root = Path(
        os.environ.get(_REWRITE_ROOT_ENVIRONMENT, REPOSITORY_ROOT / "src")
    ).resolve()
    return _run_case(case, rewrite_root, "rewrite")


def _run_case(case: dict[str, Any], package_root: Path, target: str) -> CaseRun:
    if not (package_root / "requests" / "__init__.py").is_file():
        raise FileNotFoundError(
            f"requests source package not found under {package_root}"
        )

    try:
        payload = json.dumps(case, allow_nan=False) + "\n"
    except (TypeError, ValueError) as error:
        raise TypeError("differential cases must be JSON-serializable") from error

    timeout = _case_timeout()
    try:
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), _CHILD_ARGUMENT],
            input=payload,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            cwd=REPOSITORY_ROOT,
            env=_child_environment(package_root, target),
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise _child_failure(
            package_root,
            f"timed out after {timeout:g}s",
            error.stdout,
            error.stderr,
        ) from error
    if completed.returncode:
        raise _child_failure(
            package_root,
            f"exited with {completed.returncode}",
            completed.stdout,
            completed.stderr,
        )

    if not completed.stdout.endswith("\n") or completed.stdout.count("\n") != 1:
        raise _child_failure(
            package_root,
            "did not emit exactly one LF-delimited JSON record",
            completed.stdout,
            completed.stderr,
        )
    try:
        observations = json.loads(completed.stdout[:-1])
    except json.JSONDecodeError as error:
        raise _child_failure(
            package_root,
            "emitted invalid JSON",
            completed.stdout,
            completed.stderr,
        ) from error
    if not isinstance(observations, dict):
        raise _child_failure(
            package_root,
            "record was not a JSON object",
            completed.stdout,
            completed.stderr,
        )
    return CaseRun(observations=observations, stderr=completed.stderr)


def _case_timeout() -> float:
    raw_timeout = os.environ.get(_TIMEOUT_ENVIRONMENT, str(_DEFAULT_TIMEOUT_SECONDS))
    try:
        timeout = float(raw_timeout)
    except ValueError as error:
        raise ValueError(f"{_TIMEOUT_ENVIRONMENT} must be a positive number") from error
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError(f"{_TIMEOUT_ENVIRONMENT} must be a positive number")
    return timeout


def _captured_text(output: str | bytes | None) -> str:
    if output is None:
        return ""
    if isinstance(output, bytes):
        return output.decode("utf-8", errors="replace")
    return output


def _child_failure(
    package_root: Path,
    reason: str,
    stdout: str | bytes | None,
    stderr: str | bytes | None,
) -> RuntimeError:
    return RuntimeError(
        f"differential child for {package_root} {reason}; "
        f"stdout={_bounded_diagnostic(stdout)!r}; "
        f"stderr={_bounded_diagnostic(stderr)!r}"
    )


def _bounded_diagnostic(output: str | bytes | None) -> str:
    text = _captured_text(output)
    if len(text) <= _MAX_DIAGNOSTIC_CHARS:
        return text
    return f"{text[:_MAX_DIAGNOSTIC_CHARS]}...<truncated>"


def _child_environment(package_root: Path, target: str) -> dict[str, str]:
    environment = {
        name: os.environ[name] for name in _PRESERVED_ENVIRONMENT if name in os.environ
    }
    python_path = str(package_root)
    if dependency_path := os.environ.get(_DEPENDENCY_PATH_ENVIRONMENT):
        python_path = os.pathsep.join((python_path, dependency_path))
    environment.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONHASHSEED": "0",
            "PYTHONIOENCODING": "utf-8",
            "PYTHONNOUSERSITE": "1",
            "PYTHONPATH": python_path,
            "PYTHONUTF8": "1",
            _TARGET_ENVIRONMENT: target,
        }
    )
    return environment


def _execute_case(case: object) -> dict[str, Any]:
    if not isinstance(case, dict) or set(case) != {"source"}:
        raise TypeError("a differential case must contain only a string 'source'")
    source = case["source"]
    if not isinstance(source, str):
        raise TypeError("differential case 'source' must be a string")

    namespace: dict[str, Any] = {
        "__name__": _CASE_MODULE,
        "result": None,
        "side_effects": [],
    }
    error: BaseException | None = None
    with warnings.catch_warnings(record=True) as caught_warnings:
        warnings.simplefilter("always")
        try:
            exec(compile(source, "<differential-case>", "exec"), namespace)
        except BaseException as caught:
            error = caught

    return {
        "result": None
        if error is not None
        else {
            "type": _type_record(type(namespace.get("result"))),
            "repr": _safe_repr(namespace.get("result")),
            "public_state": _public_state(namespace.get("result")),
        },
        "warnings": [
            {
                "category": _type_record(item.category),
                "message": str(item.message),
            }
            for item in caught_warnings
        ],
        "exception": None if error is None else _exception_record(error, namespace),
        "side_effects": _normalize(namespace.get("side_effects", [])),
    }


@contextlib.contextmanager
def _isolated_protocol_stdout() -> Iterator[int]:
    sys.stdout.flush()
    protocol_stdout = os.dup(1)
    try:
        os.dup2(2, 1)
        with contextlib.redirect_stdout(sys.stderr):
            yield protocol_stdout
    finally:
        sys.stderr.flush()
        os.close(protocol_stdout)


def _type_record(cls: type) -> dict[str, str]:
    return {"module": cls.__module__, "name": cls.__qualname__}


def _exception_record(
    error: BaseException, namespace: dict[str, Any]
) -> dict[str, Any]:
    cause = error.__cause__
    context = error.__context__
    identities = namespace.get("exception_identities")
    return {
        "mro": [_type_record(cls) for cls in type(error).__mro__],
        "args": _normalize(error.args),
        "cause": _linked_exception_record(cause),
        "context": _linked_exception_record(context),
        "suppress_context": error.__suppress_context__,
        "traceback_present": error.__traceback__ is not None,
        "identity": _matching_identity_names(error, identities),
        "cause_identity": _matching_identity_names(cause, identities),
        "context_identity": _matching_identity_names(context, identities),
        "cause_is_context": cause is not None and cause is context,
    }


def _linked_exception_record(error: BaseException | None) -> dict[str, Any] | None:
    if error is None:
        return None
    return {
        "type": _type_record(type(error)),
        "args": _normalize(error.args),
        "traceback_present": error.__traceback__ is not None,
    }


def _matching_identity_names(value: object, identities: object) -> list[str]:
    if type(identities) is not dict:
        return []
    return [
        name
        for name, candidate in dict.items(identities)
        if type(name) is str and value is candidate
    ]


def _public_state(value: object) -> dict[str, Any]:
    try:
        attributes = vars(value)
    except TypeError:
        return {}
    return {
        name: _normalize(attribute)
        for name, attribute in attributes.items()
        if not name.startswith("_")
    }


def _normalize(value: object, seen: set[int] | None = None) -> Any:
    if value is None or isinstance(value, (bool, int, str)):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else _repr_record(value)

    if seen is None:
        seen = set()
    identity = id(value)
    if identity in seen:
        return _repr_record(value)
    seen.add(identity)
    try:
        if isinstance(value, (list, tuple)):
            return [_normalize(item, seen) for item in value]
        if isinstance(value, dict) and all(isinstance(key, str) for key in value):
            return {key: _normalize(item, seen) for key, item in value.items()}
        return _repr_record(value)
    finally:
        seen.remove(identity)


def _repr_record(value: object) -> dict[str, object]:
    return {"type": _type_record(type(value)), "repr": _safe_repr(value)}


def _safe_repr(value: object) -> str:
    try:
        representation = repr(value)
    except Exception as error:
        return f"<repr raised {type(error).__module__}.{type(error).__qualname__}>"

    try:
        identities = sorted(
            (re.escape(hex(identity)) for identity in _reachable_identities(value)),
            key=len,
            reverse=True,
        )
        return re.sub(
            rf"(?<![0-9a-fA-F])(?:{'|'.join(identities)})(?![0-9a-fA-F])",
            "0x...",
            representation,
        )
    except Exception:
        return representation


def _reachable_identities(value: object) -> set[int]:
    identities = {id(value)}
    pending = [value]
    while pending:
        current = pending.pop()
        for child in _repr_children(current):
            identity = id(child)
            if identity in identities:
                continue
            identities.add(identity)
            pending.append(child)
    return identities


def _repr_children(value: object) -> Iterator[object]:
    if _has_builtin_base(value, list):
        length = list.__len__(value)
        for index in range(length):
            yield list.__getitem__(value, index)
    elif _has_builtin_base(value, tuple):
        length = tuple.__len__(value)
        for index in range(length):
            yield tuple.__getitem__(value, index)
    elif _has_builtin_base(value, dict):
        for key, item in dict.items(value):
            yield key
            yield item
    elif _has_builtin_base(value, set):
        yield from set.__iter__(value)
    elif _has_builtin_base(value, frozenset):
        yield from frozenset.__iter__(value)

    yield from _instance_state_children(value)


def _has_builtin_base(value: object, candidate: type) -> bool:
    mro = type.__getattribute__(type(value), "__mro__")
    for index in range(tuple.__len__(mro)):
        if tuple.__getitem__(mro, index) is candidate:
            return True
    return False


def _instance_state_children(value: object) -> Iterator[object]:
    value_type = type(value)
    mro = type.__getattribute__(value_type, "__mro__")
    dictionary_seen = False
    for index in range(tuple.__len__(mro)):
        base = tuple.__getitem__(mro, index)
        namespace = type.__getattribute__(base, "__dict__")
        dictionary_descriptor = namespace.get("__dict__")
        if (
            not dictionary_seen
            and type(dictionary_descriptor) is types.GetSetDescriptorType
        ):
            dictionary_seen = True
            attributes = types.GetSetDescriptorType.__get__(
                dictionary_descriptor, value, value_type
            )
            if type(attributes) is dict:
                for name, item in dict.items(attributes):
                    yield name
                    yield item

        for descriptor in types.MappingProxyType.values(namespace):
            if type(descriptor) is not types.MemberDescriptorType:
                continue
            try:
                item = types.MemberDescriptorType.__get__(descriptor, value, value_type)
            except AttributeError:
                continue
            yield item


def _write_all(descriptor: int, content: bytes) -> None:
    remaining = memoryview(content)
    while remaining:
        written = os.write(descriptor, remaining)
        if written == 0:
            raise RuntimeError("protocol descriptor accepted zero bytes")
        remaining = remaining[written:]


def _child_main() -> None:
    records = [line for line in sys.stdin if line.strip()]
    if len(records) != 1:
        raise RuntimeError("differential child expects exactly one JSON input line")
    case = json.loads(records[0])
    with _isolated_protocol_stdout() as protocol_stdout:
        record = _execute_case(case)
        document = json.dumps(
            record,
            allow_nan=False,
            ensure_ascii=True,
            sort_keys=True,
            separators=(",", ":"),
        )
        _write_all(protocol_stdout, f"{document}\n".encode())


if __name__ == "__main__":
    if sys.argv[1:] != [_CHILD_ARGUMENT]:
        raise SystemExit(f"usage: {Path(__file__).name} {_CHILD_ARGUMENT}")
    _child_main()
