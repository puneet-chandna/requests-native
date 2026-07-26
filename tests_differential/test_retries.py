from __future__ import annotations

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
