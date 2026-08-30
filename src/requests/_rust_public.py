# pyright: reportConstantRedefinition=false
"""Private Task 17 public-facade admission and dispatch support."""

from __future__ import annotations

import itertools
import os
import sys
import threading
import types
import weakref
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any

_TRIAL_STATE = threading.local()
_REGISTRY_LOCK = threading.RLock()
_REGISTRY_PID = os.getpid()
_GENERATIONS = itertools.count(1)
_ACTIVITY_EPOCHS = itertools.count(1)


@dataclass
class _FacadeEntry:
    reference: weakref.ReferenceType[Any]
    generation: int
    pool_generation: int


@dataclass
class _OwnerTransitionToken:
    first: bool
    quiescent: bool = False


@dataclass(frozen=True)
class _OwnerRegistrationReservation:
    token: object
    epoch: int
    thread_id: int
    access_depth: int


_REGISTRY: dict[int, _FacadeEntry] = {}
_OWNER_TRANSITIONS: dict[int, dict[int, int]] = {}
_OWNER_ACCESSES: dict[int, dict[int, int]] = {}
_OWNER_REGISTRATIONS: dict[int, _OwnerRegistrationReservation] = {}
_OWNER_ACTIVITY_EPOCHS: dict[int, int] = {}


def _trial_enabled() -> bool:
    return bool(getattr(_TRIAL_STATE, "depth", 0))


@contextmanager
def suspend_dispatch():
    previous = getattr(_TRIAL_STATE, "suspended", 0)
    _TRIAL_STATE.suspended = previous + 1
    try:
        yield
    finally:
        _TRIAL_STATE.suspended = previous


@contextmanager
def owner_access(owner: Any):
    """Track an owner operation and deny native work during transitions."""

    _ensure_process()
    process_id = os.getpid()
    key = id(owner)
    thread_id = threading.get_ident()
    with _REGISTRY_LOCK:
        registration = _OWNER_REGISTRATIONS.get(key)
        if registration is not None:
            _OWNER_ACTIVITY_EPOCHS[key] = next(_ACTIVITY_EPOCHS)
        allowed = not _OWNER_TRANSITIONS.get(key) and registration is None
        accesses = _OWNER_ACCESSES.setdefault(key, {})
        accesses[thread_id] = accesses.get(thread_id, 0) + 1
    try:
        yield allowed
    finally:
        if os.getpid() != process_id:
            _ensure_process()
        else:
            with _REGISTRY_LOCK:
                accesses = _OWNER_ACCESSES[key]
                remaining = accesses[thread_id] - 1
                if remaining:
                    accesses[thread_id] = remaining
                else:
                    del accesses[thread_id]
                if not accesses:
                    del _OWNER_ACCESSES[key]


@contextmanager
def owner_transition(owner: Any):
    """Track one nonblocking owner transition without retaining its owner."""

    _ensure_process()
    process_id = os.getpid()
    key = id(owner)
    thread_id = threading.get_ident()
    with _REGISTRY_LOCK:
        if key in _OWNER_REGISTRATIONS:
            _OWNER_ACTIVITY_EPOCHS[key] = next(_ACTIVITY_EPOCHS)
        transitions = _OWNER_TRANSITIONS.setdefault(key, {})
        token = _OwnerTransitionToken(first=not transitions)
        transitions[thread_id] = transitions.get(thread_id, 0) + 1
    try:
        yield token
    finally:
        if os.getpid() != process_id:
            _ensure_process()
        else:
            with _REGISTRY_LOCK:
                transitions = _OWNER_TRANSITIONS.get(key)
                if transitions is None or thread_id not in transitions:
                    raise RuntimeError("owner transition state was lost")
                remaining = transitions[thread_id] - 1
                if remaining:
                    transitions[thread_id] = remaining
                else:
                    del transitions[thread_id]
                if not transitions:
                    del _OWNER_TRANSITIONS[key]
                    token.quiescent = not _OWNER_ACCESSES.get(key)


def _ensure_process() -> None:
    global _ACTIVITY_EPOCHS, _GENERATIONS, _OWNER_ACCESSES, _OWNER_ACTIVITY_EPOCHS
    global _OWNER_REGISTRATIONS, _OWNER_TRANSITIONS
    global _REGISTRY, _REGISTRY_LOCK, _REGISTRY_PID
    current_pid = os.getpid()
    if current_pid == _REGISTRY_PID:
        return
    _REGISTRY_PID = current_pid
    _REGISTRY_LOCK = threading.RLock()
    _REGISTRY = {}
    _OWNER_TRANSITIONS = {}
    _OWNER_ACCESSES = {}
    _OWNER_REGISTRATIONS = {}
    _OWNER_ACTIVITY_EPOCHS = {}
    _GENERATIONS = itertools.count(1)
    _ACTIVITY_EPOCHS = itertools.count(1)
    try:
        from . import _requests_rust
    except ImportError:
        return
    reset = getattr(_requests_rust, "_adapter_fork_reset_trial", None)
    if reset is not None:
        reset()


def _next_generation() -> int:
    return (os.getpid() << 32) | next(_GENERATIONS)


@contextmanager
def _transient_caller_module():
    """Make dynamically executed trial owners pickle-resolvable for the trial."""

    frame = sys._getframe(2)
    installed_name = None
    installed_module = None
    while frame is not None:
        namespace = frame.f_globals
        name = namespace.get("__name__")
        if isinstance(name, str) and name not in sys.modules:
            installed_name = name
            installed_module = types.ModuleType(name)
            installed_module.__dict__.update(namespace)
            sys.modules[name] = installed_module
            break
        frame = frame.f_back
    try:
        yield
    finally:
        if (
            installed_name is not None
            and sys.modules.get(installed_name) is installed_module
        ):
            del sys.modules[installed_name]


@contextmanager
def rust_public_trial():
    """Privately opt exact Requests facades into the Task 17 native path."""

    previous_depth = getattr(_TRIAL_STATE, "depth", 0)
    _TRIAL_STATE.depth = previous_depth + 1
    _ensure_process()
    try:
        if previous_depth:
            yield
        else:
            with _transient_caller_module():
                yield
    finally:
        _TRIAL_STATE.depth = previous_depth


def _is_exact_owner(owner: Any) -> bool:
    from .adapters import _HTTP_ADAPTER_FACADE_TYPE
    from .sessions import _SESSION_FACADE_TYPE

    return (
        type(owner) is _SESSION_FACADE_TYPE or type(owner) is _HTTP_ADAPTER_FACADE_TYPE
    )


def _drop_generation(key: int, generation: int) -> bool:
    _ensure_process()
    with _REGISTRY_LOCK:
        entry = _REGISTRY.get(key)
        if entry is None or entry.generation != generation:
            return False
        del _REGISTRY[key]
    return True


def close_reference(key: int, reference: weakref.ReferenceType[Any]) -> None:
    """Forget only the facade generation owned by this exact native weakref."""

    _ensure_process()
    with _REGISTRY_LOCK:
        entry = _REGISTRY.get(key)
        if entry is not None and entry.reference is reference:
            del _REGISTRY[key]


def _new_reference(
    owner: Any, key: int, generation: int
) -> weakref.ReferenceType[Any] | None:
    from .adapters import _HTTP_ADAPTER_FACADE_TYPE

    if type(owner) is not _HTTP_ADAPTER_FACADE_TYPE:

        def owner_callback(_reference: Any) -> None:
            _drop_generation(key, generation)

        return weakref.ref(owner, owner_callback)

    def callback(reference: weakref.ReferenceType[Any]) -> None:
        close_reference(key, reference)
        try:
            from . import _requests_rust
        except ImportError:
            return
        _requests_rust._adapter_drop_reference_trial(key, reference)

    before = {id(reference) for reference in weakref.getweakrefs(owner)}
    from . import _requests_rust

    try:
        admitted = _requests_rust._adapter_register_trial(owner, callback)
    except AttributeError:
        return None
    if not admitted:
        return None
    created = next(
        (
            reference
            for reference in weakref.getweakrefs(owner)
            if id(reference) not in before
        ),
        None,
    )
    if created is not None:
        return created
    try:
        existing = _requests_rust._adapter_reference_trial(owner)
    except AttributeError:
        return None
    if existing is not None and existing() is owner:
        return existing
    return None


def _begin_adapter_registration(key: int) -> _OwnerRegistrationReservation | None:
    with _REGISTRY_LOCK:
        current = _OWNER_REGISTRATIONS.get(key)
        if current is not None:
            _OWNER_ACTIVITY_EPOCHS[key] = next(_ACTIVITY_EPOCHS)
            return None
        if _OWNER_TRANSITIONS.get(key):
            return None
        thread_id = threading.get_ident()
        accesses = _OWNER_ACCESSES.get(key, {})
        if any(owner_thread != thread_id for owner_thread in accesses):
            return None
        access_depth = accesses.get(thread_id, 0)
        epoch = next(_ACTIVITY_EPOCHS)
        reservation = _OwnerRegistrationReservation(
            object(), epoch, thread_id, access_depth
        )
        _OWNER_ACTIVITY_EPOCHS[key] = epoch
        _OWNER_REGISTRATIONS[key] = reservation
        return reservation


def _clear_adapter_registration(
    key: int, reservation: _OwnerRegistrationReservation
) -> None:
    with _REGISTRY_LOCK:
        if _OWNER_REGISTRATIONS.get(key) is reservation:
            del _OWNER_REGISTRATIONS[key]
            _OWNER_ACTIVITY_EPOCHS.pop(key, None)


def _drop_adapter_reference(key: int, reference: weakref.ReferenceType[Any]) -> None:
    try:
        from . import _requests_rust
    except ImportError:
        return
    _requests_rust._adapter_drop_reference_trial(key, reference)


def _register_adapter_reference(
    owner: Any,
    key: int,
    generation: int,
    pool_generation: int | None,
) -> _FacadeEntry | weakref.ReferenceType[Any] | None:
    reservation = _begin_adapter_registration(key)
    if reservation is None:
        return None
    try:
        reference = _new_reference(owner, key, generation)
    except BaseException:
        _clear_adapter_registration(key, reservation)
        raise
    if reference is None:
        _clear_adapter_registration(key, reservation)
        return None

    entry = (
        None
        if pool_generation is None
        else _FacadeEntry(reference, generation, pool_generation)
    )
    with _REGISTRY_LOCK:
        accesses = _OWNER_ACCESSES.get(key, {})
        baseline_accesses = accesses.get(
            reservation.thread_id, 0
        ) == reservation.access_depth and len(accesses) == int(
            bool(reservation.access_depth)
        )
        valid = (
            _OWNER_REGISTRATIONS.get(key) is reservation
            and _OWNER_ACTIVITY_EPOCHS.get(key) == reservation.epoch
            and not _OWNER_TRANSITIONS.get(key)
            and baseline_accesses
        )
        if valid:
            if entry is not None:
                _REGISTRY[key] = entry
            del _OWNER_REGISTRATIONS[key]
            _OWNER_ACTIVITY_EPOCHS.pop(key, None)
            return reference if entry is None else entry
        current = _REGISTRY.get(key)
        if current is not None and current.reference is reference:
            del _REGISTRY[key]

    _drop_adapter_reference(key, reference)
    _clear_adapter_registration(key, reservation)
    return None


def register_adapter(owner: Any) -> weakref.ReferenceType[Any] | None:
    """Register one exact adapter without holding registry state across proof."""

    _ensure_process()
    from .adapters import _HTTP_ADAPTER_FACADE_TYPE

    if type(owner) is not _HTTP_ADAPTER_FACADE_TYPE:
        return None
    key = id(owner)
    generation = _next_generation()
    result = _register_adapter_reference(owner, key, generation, None)
    return result if isinstance(result, weakref.ReferenceType) else None


def _admit(owner: Any, *, rotate: bool = False) -> _FacadeEntry | None:
    _ensure_process()
    if getattr(_TRIAL_STATE, "suspended", False):
        return None
    if not _is_exact_owner(owner):
        return None
    key = id(owner)
    from .adapters import _HTTP_ADAPTER_FACADE_TYPE

    with _REGISTRY_LOCK:
        if key in _OWNER_REGISTRATIONS:
            _OWNER_ACTIVITY_EPOCHS[key] = next(_ACTIVITY_EPOCHS)
            return None
        current = _REGISTRY.get(key)
        if not rotate and current is not None and current.reference() is owner:
            return current
        generation = _next_generation()
        pool_generation = _next_generation()
    if type(owner) is _HTTP_ADAPTER_FACADE_TYPE:
        result = _register_adapter_reference(owner, key, generation, pool_generation)
        return result if isinstance(result, _FacadeEntry) else None
    reference = _new_reference(owner, key, generation)
    if reference is None:
        return None
    entry = _FacadeEntry(reference, generation, pool_generation)
    with _REGISTRY_LOCK:
        _REGISTRY[key] = entry
    return entry


def close_owner(owner: Any) -> None:
    _ensure_process()
    key = id(owner)
    with _REGISTRY_LOCK:
        entry = _REGISTRY.get(key)
        if entry is not None and entry.reference() is owner:
            del _REGISTRY[key]


def dispatch(
    group: str,
    subject: Any,
    operation: str,
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
) -> Any:
    if (
        not _trial_enabled()
        or getattr(_TRIAL_STATE, "suspended", False)
        or group in getattr(_TRIAL_STATE, "native_dispatch_groups", ())
    ):
        return NotImplemented
    if group in {"session", "adapter"}:
        _admit(subject)
    with owner_access(subject) as allowed:
        if not allowed:
            return NotImplemented
        from . import _requests_rust

        seam = getattr(_requests_rust, f"_{group}_facade_trial")
        previous_groups = getattr(_TRIAL_STATE, "native_dispatch_groups", ())
        _TRIAL_STATE.native_dispatch_groups = (*previous_groups, group)
        try:
            return seam(subject, operation, args, kwargs)
        finally:
            _TRIAL_STATE.native_dispatch_groups = previous_groups


def public_facade_snapshot(owner: Any) -> dict[str, Any]:
    _ensure_process()
    admitted = _admit(owner) if _trial_enabled() else None
    with owner_access(owner) as allowed:
        if not allowed:
            entry = None
        elif _trial_enabled():
            entry = admitted
        else:
            with _REGISTRY_LOCK:
                candidate = _REGISTRY.get(id(owner))
                entry = (
                    candidate
                    if candidate is not None and candidate.reference() is owner
                    else None
                )
    return {
        "owner_generation": None if entry is None else entry.generation,
        "pool_generation": None if entry is None else entry.pool_generation,
        "live": entry is not None and entry.reference() is owner,
    }


def public_facade_registry_trial(owner: Any, operation: str, *args: Any) -> Any:
    _ensure_process()
    entry = None if operation == "admitted" else _admit(owner)
    if operation == "rotate" and entry is not None:
        entry = _admit(owner, rotate=True)
    with owner_access(owner) as allowed:
        if not allowed:
            return False if operation == "admitted" else None
        if operation == "admitted":
            key = id(owner)
            with _REGISTRY_LOCK:
                entry = _REGISTRY.get(key)
                return entry is not None and entry.reference() is owner
        if entry is None:
            return None
        if operation == "key":
            return id(owner)
        if operation == "generation":
            return entry.generation
        if operation == "rotate":
            return entry.generation
        if operation == "drop":
            key, generation = args
            return _drop_generation(key, generation)
        raise ValueError(f"unknown public facade registry operation: {operation}")


def install_extension_hooks(extension: Any) -> None:
    extension._public_facade_snapshot = public_facade_snapshot
    extension._public_facade_registry_trial = public_facade_registry_trial
