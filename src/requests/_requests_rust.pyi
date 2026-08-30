from collections.abc import Callable
from types import NotImplementedType
from typing import Any
from weakref import ReferenceType

from .models import PreparedRequest, Response

def backend_name() -> str: ...
def _adapter_fork_reset_trial() -> None: ...
def _adapter_drop_trial(identity: int) -> int: ...
def _adapter_drop_reference_trial(
    identity: int, reference: ReferenceType[Any]
) -> int: ...
def _adapter_reference_trial(owner: object) -> ReferenceType[Any] | None: ...
def _adapter_register_trial(
    owner: object, callback: Callable[[ReferenceType[Any]], object]
) -> bool: ...
def _adapter_send_trial(
    adapter: object,
    request: PreparedRequest,
    stream: bool,
    timeout: object,
    verify: object,
    cert: object,
    proxies: object,
) -> Response | NotImplementedType: ...
def _adapter_facade_trial(
    adapter: object,
    operation: str,
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
) -> object: ...
def _session_facade_trial(
    session: object,
    operation: str,
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
) -> object: ...
