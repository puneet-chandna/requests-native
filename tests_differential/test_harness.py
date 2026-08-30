from __future__ import annotations

import os
from pathlib import Path
from textwrap import dedent

import pytest
from tests_differential import runner

ROOT = Path(__file__).resolve().parents[1]


def test_rewrite_root_can_target_an_installed_package(
    monkeypatch, tmp_path: Path
) -> None:
    site_packages = tmp_path / "site-packages"
    package = site_packages / "requests"
    package.mkdir(parents=True)
    (package / "__init__.py").write_text('marker = "installed-artifact"\n')
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_REWRITE_ROOT", str(site_packages))

    run = runner.run_rewrite_case(
        {"source": "import requests\nresult = requests.marker\n"}
    )

    assert run.observations["result"]["repr"] == "'installed-artifact'"


def test_children_receive_an_unforgeable_differential_target(monkeypatch) -> None:
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_TARGET", "caller-forgery")
    case = {
        "source": ('import os\nresult = os.environ["REQUESTS_DIFFERENTIAL_TARGET"]\n')
    }

    oracle = runner.run_oracle_case(case)
    rewrite = runner.run_rewrite_case(case)

    assert oracle.observations["result"]["repr"] == "'oracle'"
    assert rewrite.observations["result"]["repr"] == "'rewrite'"


def test_runs_each_case_in_a_separate_process(oracle_root) -> None:
    case = {
        "source": dedent(
            """
            import os
            import sys
            import warnings

            class Sample:
                def __init__(self):
                    self.visible = {"items": [1, 2]}
                    self._hidden = "ignored"

                def __repr__(self):
                    return "<sample>"

            side_effects.append({
                "event": "start",
                "pid": os.getpid(),
                "cwd": os.getcwd(),
            })
            print("case diagnostic", file=sys.stderr)
            warnings.warn("first", UserWarning)
            warnings.warn("second", RuntimeWarning)
            result = Sample()
            side_effects.append({"event": "finish"})
            """
        )
    }

    oracle = runner.run_oracle_case(case)
    rewrite = runner.run_rewrite_case(case)

    for run in (oracle, rewrite):
        observations = run.observations
        assert observations["result"] == {
            "type": {"module": "__differential_case__", "name": "Sample"},
            "repr": "<sample>",
            "public_state": {"visible": {"items": [1, 2]}},
        }
        assert observations["warnings"] == [
            {
                "category": {"module": "builtins", "name": "UserWarning"},
                "message": "first",
            },
            {
                "category": {"module": "builtins", "name": "RuntimeWarning"},
                "message": "second",
            },
        ]
        assert observations["exception"] is None
        assert [effect["event"] for effect in observations["side_effects"]] == [
            "start",
            "finish",
        ]
        assert observations["side_effects"][0]["pid"] != os.getpid()
        assert observations["side_effects"][0]["cwd"] == str(ROOT)
        assert run.stderr == "case diagnostic\n"


def test_captures_exception_mro_args_warnings_and_prior_side_effects(
    oracle_root,
) -> None:
    case = {
        "source": dedent(
            """
            import warnings

            class ParentError(Exception):
                pass

            class ChildError(ParentError):
                pass

            side_effects.append("before warning")
            warnings.warn("careful", FutureWarning)
            side_effects.append("before error")
            raise ChildError("boom", 7)
            """
        )
    }

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        assert run.observations == {
            "result": None,
            "warnings": [
                {
                    "category": {
                        "module": "builtins",
                        "name": "FutureWarning",
                    },
                    "message": "careful",
                }
            ],
            "exception": {
                "mro": [
                    {"module": "__differential_case__", "name": "ChildError"},
                    {"module": "__differential_case__", "name": "ParentError"},
                    {"module": "builtins", "name": "Exception"},
                    {"module": "builtins", "name": "BaseException"},
                    {"module": "builtins", "name": "object"},
                ],
                "args": ["boom", 7],
                "cause": None,
                "context": None,
                "suppress_context": False,
                "traceback_present": True,
                "identity": [],
                "cause_identity": [],
                "context_identity": [],
                "cause_is_context": False,
            },
            "side_effects": ["before warning", "before error"],
        }
        assert run.stderr == ""


def test_observation_output_stays_off_protocol_stdout(oracle_root) -> None:
    case = {
        "source": dedent(
            """
            class LoudResult:
                def __repr__(self):
                    print("repr diagnostic")
                    return "<loud result>"

            result = LoudResult()
            """
        )
    }

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        assert run.observations["result"] == {
            "type": {
                "module": "__differential_case__",
                "name": "LoudResult",
            },
            "repr": "<loud result>",
            "public_state": {},
        }
        assert run.stderr == "repr diagnostic\n"


def test_subprocess_timeout_reports_target_and_captured_output(monkeypatch) -> None:
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_TIMEOUT", "0.5")
    case = {
        "source": dedent(
            """
            import time

            print("timeout diagnostic", flush=True)
            time.sleep(2)
            result = "too late"
            """
        )
    }

    with pytest.raises(RuntimeError, match="timed out") as raised:
        runner.run_rewrite_case(case)

    message = str(raised.value)
    assert str(ROOT / "src") in message
    assert "stdout=" in message
    assert "stderr=" in message
    assert "timeout diagnostic" in message


def test_unicode_line_separators_do_not_split_json_record(oracle_root) -> None:
    case = {
        "source": (
            'side_effects.append("left\\u2028middle\\u2029right")\n'
            'result = "unicode separators"\n'
        )
    }

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        assert run.observations["side_effects"] == ["left\u2028middle\u2029right"]


def test_opaque_repr_is_repeatable_across_processes() -> None:
    case = {"source": "result = object()\n"}

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == "<object object at 0x...>"


def test_composite_repr_is_repeatable_across_supported_graph() -> None:
    case = {
        "source": dedent(
            """
            class PublicState:
                def __init__(self):
                    self.child = object()

                def __repr__(self):
                    return f"<state child={self.child!r}>"

            cycle = []
            cycle.append(cycle)
            result = (
                object(),
                [object()],
                {"state": PublicState(), "cycle": cycle},
            )
            """
        )
    }

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)

    assert first.observations == second.observations
    assert first.observations["result"]["repr"].count("0x...") == 3


def test_repr_identity_traversal_ignores_overridden_infinite_iterator(
    monkeypatch,
) -> None:
    monkeypatch.setenv("REQUESTS_DIFFERENTIAL_TIMEOUT", "0.5")
    case = {
        "source": dedent(
            """
            class RepeatingList(list):
                def __iter__(self):
                    while True:
                        yield self

                def __repr__(self):
                    return "<repeating>"

            result = RepeatingList()
            """
        )
    }

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == "<repeating>"


def test_repr_discovery_bypasses_overridden_list_iteration() -> None:
    case = {
        "source": dedent(
            """
            class RaisingList(list):
                def __iter__(self):
                    raise RuntimeError("iteration must not run")

            result = RaisingList([object()])
            """
        )
    }

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == "[<object object at 0x...>]"


def test_repr_discovery_bypasses_overridden_class_lookup() -> None:
    case = {
        "source": dedent(
            """
            class GuardedState:
                def __init__(self):
                    self._child = object()

                def __getattribute__(self, name):
                    if name == "__class__":
                        raise RuntimeError("class lookup must not run")
                    return object.__getattribute__(self, name)

                def __repr__(self):
                    child = object.__getattribute__(self, "_child")
                    return f"<guarded child={child!r}>"

            result = GuardedState()
            """
        )
    }

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == (
        "<guarded child=<object object at 0x...>>"
    )


def test_private_instance_state_repr_is_repeatable() -> None:
    case = {
        "source": dedent(
            """
            class PrivateState:
                def __init__(self):
                    self._child = object()

                def __repr__(self):
                    return f"<private child={self._child!r}>"

            result = PrivateState()
            """
        )
    }

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == (
        "<private child=<object object at 0x...>>"
    )


def test_private_slot_state_repr_is_repeatable() -> None:
    case = {
        "source": dedent(
            """
            class PrivateSlotState:
                __slots__ = ("_child",)

                def __init__(self):
                    self._child = object()

                def __repr__(self):
                    return f"<private slot child={self._child!r}>"

            result = PrivateSlotState()
            """
        )
    }

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == (
        "<private slot child=<object object at 0x...>>"
    )


def test_non_string_dict_key_repr_is_repeatable() -> None:
    case = {"source": "result = {object(): object()}\n"}

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)

    assert first.observations == second.observations
    assert first.observations["result"]["repr"].count("0x...") == 2


def test_set_repr_is_repeatable_and_complete() -> None:
    case = {"source": "result = {object(), object()}\n"}

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)
    expected_item = "<object object at 0x...>"
    expected = f"{{{expected_item}, {expected_item}}}"

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == expected


def test_frozenset_repr_is_repeatable_and_complete() -> None:
    case = {"source": "result = frozenset((object(), object()))\n"}

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)
    expected_item = "<object object at 0x...>"
    expected = f"frozenset({{{expected_item}, {expected_item}}})"

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == expected


def test_large_semantic_tuple_repr_is_captured_unchanged() -> None:
    case = {"source": "result = tuple(range(300))\n"}

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == repr(tuple(range(300)))


def test_large_opaque_tuple_repr_is_repeatable_and_complete() -> None:
    case = {"source": "result = tuple(object() for _ in range(300))\n"}

    first = runner.run_rewrite_case(case)
    second = runner.run_rewrite_case(case)
    expected_item = "<object object at 0x...>"
    expected = f"({', '.join([expected_item] * 300)})"

    assert first.observations == second.observations
    assert first.observations["result"]["repr"] == expected


def test_repr_traversal_failure_preserves_captured_repr(monkeypatch) -> None:
    class StableRepr:
        def __repr__(self) -> str:
            return "<stable repr>"

    def fail_identity_discovery(value: object) -> set[int]:
        raise RuntimeError("identity discovery failed")

    monkeypatch.setattr(runner, "_reachable_identities", fail_identity_discovery)

    assert runner._safe_repr(StableRepr()) == "<stable repr>"


def test_safe_repr_normalizes_zero_padded_identity() -> None:
    class PaddedIdentity:
        def __repr__(self) -> str:
            return f"<padded at 0x{id(self):016X}>"

    assert runner._safe_repr(PaddedIdentity()) == "<padded at 0x...>"


def test_semantic_hex_in_custom_repr_is_preserved() -> None:
    case = {
        "source": dedent(
            """
            class SemanticResult:
                def __repr__(self):
                    return "<semantic at 0xBEEF>"

            result = SemanticResult()
            """
        )
    }

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == "<semantic at 0xBEEF>"


def test_lone_surrogate_is_escaped_across_protocol(oracle_root) -> None:
    case = {
        "source": dedent(
            """
            class LoneSurrogate:
                def __init__(self):
                    self.value = "\\ud800"

                def __repr__(self):
                    return self.value

            result = LoneSurrogate()
            side_effects.append(result.value)
            """
        )
    }
    surrogate = chr(0xD800)

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        assert run.observations["result"]["repr"] == surrogate
        assert run.observations["result"]["public_state"]["value"] == surrogate
        assert run.observations["side_effects"] == [surrogate]


def test_direct_fd_stdout_noise_is_captured(oracle_root) -> None:
    case = {
        "source": dedent(
            """
            import os

            class LoudResult:
                def __repr__(self):
                    os.write(1, b"repr fd diagnostic\\n")
                    return "<clean protocol>"

            os.write(1, b"exec fd diagnostic\\n")
            result = LoudResult()
            """
        )
    }

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        assert run.observations["result"]["repr"] == "<clean protocol>"
        assert run.stderr == "exec fd diagnostic\nrepr fd diagnostic\n"


@pytest.mark.skipif(os.name != "posix", reason="libc printf is POSIX-specific")
def test_buffered_native_stdout_cannot_corrupt_protocol() -> None:
    case = {
        "source": dedent(
            """
            import ctypes

            libc = ctypes.CDLL(None)
            libc.printf.argtypes = [ctypes.c_char_p]
            libc.printf.restype = ctypes.c_int
            libc.printf(b"buffered native diagnostic\\n")
            result = "clean protocol"
            """
        )
    }

    run = runner.run_rewrite_case(case)

    assert run.observations["result"]["repr"] == "'clean protocol'"
    assert run.stderr == "buffered native diagnostic\n"


def test_protocol_error_reports_target_and_bounded_streams() -> None:
    case = {"source": ('import os\nos.write(2, b"x" * 10000)\nos._exit(0)\n')}

    with pytest.raises(RuntimeError) as raised:
        runner.run_rewrite_case(case)

    message = str(raised.value)
    assert str(ROOT / "src") in message
    assert "stdout=''" in message
    assert "stderr=" in message
    assert "<truncated>" in message
    assert len(message) < 5000


def test_configured_source_roots_cannot_cross_contaminate(
    monkeypatch, oracle_root
) -> None:
    rewrite_root = ROOT.resolve()
    monkeypatch.setenv("PYTHONHOME", str(ROOT / "missing-python-home"))
    monkeypatch.setenv("PYTHONPATH", str(ROOT / "missing-python-path"))
    case = {
        "source": dedent(
            """
            import requests

            side_effects.append(requests.__file__)
            result = requests.__name__
            """
        )
    }

    oracle = runner.run_oracle_case(case)
    rewrite = runner.run_rewrite_case(case)
    oracle_module = Path(oracle.observations["side_effects"][0]).resolve()
    rewrite_module = Path(rewrite.observations["side_effects"][0]).resolve()

    assert oracle_module.is_relative_to(oracle_root)
    assert not oracle_module.is_relative_to(rewrite_root)
    assert rewrite_module.is_relative_to(rewrite_root)
    assert not rewrite_module.is_relative_to(oracle_root)


def test_exception_record_exposes_graph_traceback_and_same_process_identity(
    oracle_root,
) -> None:
    case = {
        "source": dedent(
            """
            cause = RuntimeError("cause")
            context = ValueError("context")
            outer = LookupError("outer")
            exception_identities = {
                "outer": outer,
                "cause": cause,
                "context": context,
            }
            try:
                raise context
            except ValueError:
                raise outer from cause
            """
        )
    }

    for run in (runner.run_oracle_case(case), runner.run_rewrite_case(case)):
        record = run.observations["exception"]
        assert record["cause"]["args"] == ["cause"]
        assert record["context"]["args"] == ["context"]
        assert record["suppress_context"] is True
        assert record["traceback_present"] is True
        assert record["identity"] == ["outer"]
        assert record["cause_identity"] == ["cause"]
        assert record["context_identity"] == ["context"]
        assert record["cause_is_context"] is False
