from __future__ import annotations

import pytest
import urllib3
from urllib3.util.retry import Retry

from requests import _requests_rust


def snapshot(retry):
    return _requests_rust._retry_policy_snapshot_trial(retry)


def test_retry_snapshot_is_complete_and_nonmutating_for_supported_urllib3():
    retry_kwargs = {
        "total": None,
        "connect": False,
        "read": 3,
        "redirect": 2,
        "status": 4,
        "other": 1,
        "status_forcelist": {429, 503},
        "backoff_factor": 0.5,
        "respect_retry_after_header": False,
        "raise_on_status": False,
        "raise_on_redirect": True,
    }
    if urllib3.__version__.startswith("1.26."):
        retry_kwargs["method_whitelist"] = frozenset()
    else:
        retry_kwargs.update(
            allowed_methods=frozenset(),
            backoff_max=7.0,
            backoff_jitter=0.25,
        )
    retry = Retry(**retry_kwargs)
    before = retry.__dict__.copy()

    expected = {
        "eligible": True,
        "version": urllib3.__version__,
        "total": None,
        "connect": False,
        "read": 3,
        "status": 4,
        "redirect": 2,
        "other": 1,
        "allowed_methods": [],
        "status_forcelist": [429, 503],
        "backoff_factor": 0.5,
        "respect_retry_after_header": False,
        "raise_on_status": False,
        "raise_on_redirect": True,
        "history": [],
    }
    if urllib3.__version__.startswith("1.26."):
        expected.update(
            backoff_max=Retry.DEFAULT_BACKOFF_MAX,
            backoff_jitter=0.0,
            retry_after_max=None,
        )
    else:
        assert urllib3.__version__ == "2.7.0"
        expected.update(
            backoff_max=7.0,
            backoff_jitter=0.25,
            retry_after_max=21600,
        )

    assert snapshot(retry) == expected
    assert retry.__dict__ == before


def test_default_http_adapter_retry_shape_is_supported_without_replacement():
    from requests.adapters import HTTPAdapter

    adapter = HTTPAdapter()
    original = adapter.max_retries
    record = snapshot(original)

    assert adapter.max_retries is original
    assert record["eligible"] is True
    assert record["total"] == 0
    assert record["read"] is False


def test_retry_subclasses_and_mutated_unsupported_fields_fall_back():
    class CustomRetry(Retry):
        pass

    custom = CustomRetry(total=1)
    assert snapshot(custom) == {
        "eligible": False,
        "reason": "Retry must have the exact urllib3.util.retry.Retry type",
    }

    retry = Retry(total=1)
    marker = object()
    retry.total = marker
    assert snapshot(retry) == {
        "eligible": False,
        "reason": "total is not None, bool, or a nonnegative int",
    }
    assert retry.total is marker


def test_history_with_python_error_identity_falls_back_before_native_state():
    error = RuntimeError("kept")
    retry = Retry(total=2)
    retry.history = (
        urllib3.util.retry.RequestHistory(
            "GET",
            "http://example.test/",
            error,
            None,
            None,
        ),
    )

    assert snapshot(retry) == {
        "eligible": False,
        "reason": "history contains a Python error object",
    }
    assert retry.history[0].error is error


def test_status_and_redirect_history_without_error_is_snapshotted():
    retry = Retry(total=2)
    retry.history = (
        urllib3.util.retry.RequestHistory(
            "GET",
            "http://example.test/original",
            None,
            503,
            "/elsewhere",
        ),
    )

    record = snapshot(retry)
    assert record["eligible"] is True
    assert record["history"] == [
        {
            "method": "GET",
            "url": "http://example.test/original",
            "status": 503,
            "redirect_location": "/elsewhere",
        }
    ]


def test_effectful_collections_and_unsupported_history_rows_fall_back_inertly():
    yielded = []

    def statuses():
        yielded.append("consumed")
        yield 503

    retry = Retry(total=1)
    retry.status_forcelist = statuses()
    assert snapshot(retry)["eligible"] is False
    assert yielded == []

    retry = Retry(total=1)
    retry.history = ((None, None, None, None, None),)
    assert snapshot(retry)["eligible"] is False


def test_retry_instance_method_and_constant_shadows_are_ineligible():
    retry = Retry(total=1)
    retry.get_retry_after = lambda response: 0
    assert snapshot(retry)["eligible"] is False

    retry = Retry(total=1)
    retry.RETRY_AFTER_STATUS_CODES = frozenset({418})
    assert snapshot(retry)["eligible"] is False


def test_allowed_method_case_is_not_normalized():
    retry = Retry(total=1, allowed_methods={"get"}, status_forcelist={503})
    record = snapshot(retry)
    assert record["eligible"] is True
    assert record["allowed_methods"] == ["get"]


def test_urllib3_126_post_init_method_whitelist_takes_precedence():
    if not urllib3.__version__.startswith("1.26."):
        pytest.skip("method_whitelist was removed in urllib3 2.x")
    retry = Retry(total=1, allowed_methods={"GET"})
    retry.method_whitelist = {"post"}
    assert snapshot(retry)["allowed_methods"] == ["post"]


def test_retry_class_and_module_mutations_fall_back_then_restore(monkeypatch):
    retry = Retry(total=1)
    with monkeypatch.context() as patched:
        patched.setattr(Retry, "get_retry_after", lambda self, response: 0)
        assert snapshot(retry)["eligible"] is False
    assert snapshot(retry)["eligible"] is True

    with monkeypatch.context() as patched:
        patched.setattr(urllib3.util.retry, "Retry", object())
        assert snapshot(retry)["eligible"] is False
    assert snapshot(retry)["eligible"] is True


def test_retry_getattribute_version_and_module_dependencies_are_frozen(monkeypatch):
    retry = Retry(total=1)
    mutations = (
        (
            Retry,
            "__getattribute__",
            lambda self, name: object.__getattribute__(self, name),
        ),
        (urllib3, "__version__", "".join([urllib3.__version__, "-mutated"])),
        (urllib3.util.retry, "time", object()),
        (urllib3.util.retry, "RequestHistory", object()),
    )
    for owner, name, value in mutations:
        with monkeypatch.context() as patched:
            patched.setattr(owner, name, value)
            assert snapshot(retry)["eligible"] is False
        assert snapshot(retry)["eligible"] is True


def test_complete_retry_behavior_dependencies_fall_back_then_restore(monkeypatch):
    retry = Retry(total=1)
    class_names = [
        "is_exhausted",
        "_is_connection_error",
        "_is_read_error",
        "sleep_for_retry",
        "_sleep_backoff",
    ]
    for name in class_names:
        if not hasattr(Retry, name):
            continue
        with monkeypatch.context() as patched:
            patched.setattr(Retry, name, lambda *args, **kwargs: False)
            assert snapshot(retry)["eligible"] is False
        assert snapshot(retry)["eligible"] is True

    for name in ("takewhile", "random"):
        if not hasattr(urllib3.util.retry, name):
            continue
        with monkeypatch.context() as patched:
            patched.setattr(urllib3.util.retry, name, object())
            assert snapshot(retry)["eligible"] is False
        assert snapshot(retry)["eligible"] is True

    for name in class_names:
        if not hasattr(Retry, name):
            continue
        setattr(retry, name, lambda *args, **kwargs: False)
        assert snapshot(retry)["eligible"] is False
        delattr(retry, name)
        assert snapshot(retry)["eligible"] is True


def test_retry_function_behavior_mutations_fall_back_then_restore():
    retry = Retry(total=1)
    function = Retry.is_retry
    original_code = function.__code__
    original_defaults = function.__defaults__
    try:
        function.__code__ = (lambda self, *args, **kwargs: False).__code__
        assert snapshot(retry)["eligible"] is False
    finally:
        function.__code__ = original_code
    assert snapshot(retry)["eligible"] is True

    try:
        function.__defaults__ = (not original_defaults[0],)
        assert snapshot(retry)["eligible"] is False
    finally:
        function.__defaults__ = original_defaults
    assert snapshot(retry)["eligible"] is True
