from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case

_TRIAL_HELPERS = """
try:
    from requests import _requests_rust
except ImportError:
    _requests_rust = None


def cid_call(subject, operation, *arguments):
    if _requests_rust is not None:
        return _requests_rust._case_insensitive_dict_trial(
            subject, operation, arguments
        )
    if operation == "set":
        subject[arguments[0]] = arguments[1]
        return None
    if operation == "get":
        return subject[arguments[0]]
    if operation == "delete":
        del subject[arguments[0]]
        return None
    if operation == "core_snapshot":
        store = subject._store
        normalized_keys = list(store)
        return {
            "cased_keys": [store[key][0] for key in normalized_keys],
            "normalized_keys": normalized_keys,
            "length": len(normalized_keys),
        }
    if operation == "iter":
        return iter(subject)
    if operation == "len":
        return len(subject)
    if operation == "lower_items":
        return subject.lower_items()
    if operation == "eq":
        return subject.__eq__(arguments[0])
    if operation == "copy":
        return subject.copy()
    if operation == "repr":
        return repr(subject)
    raise AssertionError(operation)


def lookup_call(subject, operation, *arguments):
    if _requests_rust is not None:
        return _requests_rust._lookup_dict_trial(subject, operation, arguments)
    if operation == "getitem":
        return subject[arguments[0]]
    if operation == "get":
        return subject.get(*arguments)
    if operation == "getattr":
        return getattr(subject, arguments[0])
    if operation == "repr":
        return repr(subject)
    raise AssertionError(operation)
"""


def _assert_matches_oracle(source: str) -> None:
    case = {"source": dedent(_TRIAL_HELPERS + source)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_case_insensitive_order_casing_deletion_and_lazy_lower_items() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict()
cid_call(subject, "set", "Alpha", 1)
cid_call(subject, "set", "BETA", 2)
cid_call(subject, "set", "\\u0130tem", 4)
cid_call(subject, "set", "aLPHa", 3)
cid_call(subject, "set", "i\\u0307TEM", 5)

before = list(cid_call(subject, "iter"))
lower_items = cid_call(subject, "lower_items")
cid_call(subject, "set", "beta", 6)
lazy = list(lower_items)
lookup = cid_call(subject, "get", "ALPHA")
cid_call(subject, "delete", "BeTa")
core_snapshot = cid_call(subject, "core_snapshot")

result = {
    "before": before,
    "lazy": lazy,
    "lookup": lookup,
    "after": list(cid_call(subject, "iter")),
    "length": cid_call(subject, "len"),
    "core_snapshot": core_snapshot,
}
"""
    )


def test_case_insensitive_equality_mapping_side_effects_and_not_implemented() -> None:
    _assert_matches_oracle(
        """
from collections.abc import Mapping
from requests.structures import CaseInsensitiveDict


class ObservedMapping(Mapping):
    def __iter__(self):
        side_effects.append("iter")
        return iter(("x-token",))

    def __len__(self):
        side_effects.append("len")
        return 1

    def __getitem__(self, key):
        side_effects.append(["getitem", key])
        return "value"


subject = CaseInsensitiveDict({"X-Token": "value"})
mapping_equal = cid_call(subject, "eq", ObservedMapping())
direct = cid_call(subject, "eq", object())
operator_equal = subject == object()
result = {
    "mapping_equal": mapping_equal,
    "direct_not_implemented": direct is NotImplemented,
    "operator_equal": operator_equal,
}
"""
    )


def test_case_insensitive_copy_repr_and_arbitrary_value_identity() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict


class Marker:
    def __repr__(self):
        return "<marker>"


marker = Marker()
subject = CaseInsensitiveDict()
cid_call(subject, "set", "Token", marker)
fetched_is_original = cid_call(subject, "get", "tOKEN") is marker
copied = cid_call(subject, "copy")
copy_kept_identity = cid_call(copied, "get", "TOKEN") is marker
cid_call(copied, "set", "token", "replacement")

result = {
    "fetched_is_original": fetched_is_original,
    "copy_kept_identity": copy_kept_identity,
    "stores_are_independent": copied._store is not subject._store,
    "original_unchanged": cid_call(subject, "get", "TOKEN") is marker,
    "repr": cid_call(subject, "repr"),
}
"""
    )


def test_case_insensitive_lone_surrogate_keys_use_python_fallback() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

marker = object()
subject = CaseInsensitiveDict()
cid_call(subject, "set", "\\ud800-Token", marker)

result = {
    "keys": list(cid_call(subject, "iter")),
    "lower_items": [
        (key.encode("unicode-escape").decode("ascii"), value is marker)
        for key, value in cid_call(subject, "lower_items")
    ],
    "identity": cid_call(subject, "get", "\\ud800-tOKEN") is marker,
}
cid_call(subject, "delete", "\\ud800-TOKEN")
result["empty"] = cid_call(subject, "len") == 0
"""
    )


def test_case_insensitive_values_drop_on_the_origin_thread() -> None:
    _assert_matches_oracle(
        """
import threading
from requests.structures import CaseInsensitiveDict

origin_thread = threading.get_ident()


class Tracked:
    def __init__(self, label):
        self.label = label

    def __del__(self):
        side_effects.append(
            ["drop", self.label, threading.get_ident() == origin_thread]
        )


subject = CaseInsensitiveDict()
cid_call(subject, "set", "Token", Tracked("first"))
cid_call(subject, "set", "tOKEN", Tracked("second"))
side_effects.append("after-overwrite")
cid_call(subject, "delete", "TOKEN")
side_effects.append("after-delete")
result = list(cid_call(subject, "iter"))
"""
    )


def test_lookup_dict_uses_dynamic_attributes_and_keeps_dict_base_empty() -> None:
    _assert_matches_oracle(
        """
from requests.structures import LookupDict


class Name:
    def __str__(self):
        side_effects.append("name-str")
        return "dynamic-name"


class Marker:
    pass


name = Name()
marker = Marker()
subject = LookupDict(name)
empty_at_start = dict(subject) == {}
subject.answer = marker
setattr(subject, "\\ud800-answer", marker)
dict.__setitem__(subject, "answer", "dict-value")

try:
    lookup_call(subject, "getattr", "missing")
except AttributeError as error:
    missing = [type(error).__name__, error.args]
else:
    missing = None

result = {
    "is_dict": isinstance(subject, dict),
    "empty_at_start": empty_at_start,
    "base_dict": dict(subject),
    "getitem_identity": lookup_call(subject, "getitem", "answer") is marker,
    "get_identity": lookup_call(subject, "get", "answer") is marker,
    "surrogate_identity": (
        lookup_call(subject, "getattr", "\\ud800-answer") is marker
    ),
    "missing_item": lookup_call(subject, "getitem", "missing"),
    "missing_default": lookup_call(subject, "get", "missing", "fallback"),
    "repr": lookup_call(subject, "repr"),
    "name_identity": subject.name is name,
    "missing_attribute": missing,
}
"""
    )


def test_public_subclasses_and_authoritative_store_mutations_fall_back_to_python() -> (
    None
):
    _assert_matches_oracle(
        """
from collections import OrderedDict
from requests.structures import CaseInsensitiveDict, LookupDict


class TrackingStore(OrderedDict):
    def __getitem__(self, key):
        side_effects.append(["store-get", key])
        return super().__getitem__(key)

    def __setitem__(self, key, value):
        side_effects.append(["store-set", key])
        return super().__setitem__(key, value)

    def __delitem__(self, key):
        side_effects.append(["store-del", key])
        return super().__delitem__(key)


class TrackingValue(tuple):
    def __getitem__(self, index):
        side_effects.append(["stored-tuple-get", index])
        return super().__getitem__(index)


class CustomCaseInsensitiveDict(CaseInsensitiveDict):
    def __setitem__(self, key, value):
        side_effects.append(["cid-set", key])
        super().__setitem__(key, value)

    def __iter__(self):
        side_effects.append("cid-iter")
        return super().__iter__()

    def lower_items(self):
        side_effects.append("cid-lower-items")
        return super().lower_items()

    def __repr__(self):
        side_effects.append("cid-repr")
        return "<custom-cid>"


class CustomLookupDict(LookupDict):
    def __getitem__(self, key):
        side_effects.append(["lookup-getitem", key])
        return "custom-item"

    def get(self, key, default=None):
        side_effects.append(["lookup-get", key, default])
        return "custom-get"

    def __getattr__(self, key):
        side_effects.append(["lookup-getattr", key])
        return "custom-attribute"

    def __repr__(self):
        side_effects.append("lookup-repr")
        return "<custom-lookup>"


subject = CustomCaseInsensitiveDict()
cid_call(subject, "set", "One", 1)
subclass_observations = [
    list(cid_call(subject, "iter")),
    list(cid_call(subject, "lower_items")),
    cid_call(subject, "repr"),
]

mutated = CaseInsensitiveDict()
mutated._store = TrackingStore()
cid_call(mutated, "set", "Token", 7)
mutation_observations = [
    cid_call(mutated, "get", "TOKEN"),
    list(cid_call(mutated, "iter")),
]
cid_call(mutated, "delete", "tOkEn")

mutated_value = CaseInsensitiveDict()
mutated_value._store["token"] = TrackingValue(("Token", 9))
mutated_value_observation = cid_call(mutated_value, "get", "TOKEN")

lookup = CustomLookupDict("custom")
lookup_observations = [
    lookup_call(lookup, "getitem", "key"),
    lookup_call(lookup, "get", "key", "default"),
    lookup_call(lookup, "getattr", "key"),
    lookup_call(lookup, "repr"),
]

result = {
    "subclass": subclass_observations,
    "mutated": mutation_observations,
    "mutated_empty": list(mutated) == [],
    "mutated_value": mutated_value_observation,
    "lookup": lookup_observations,
}
"""
    )


def test_case_insensitive_exact_ordered_dict_ignores_shadowed_items() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict({"First": 1, "Second": 2})


def forged_items():
    side_effects.append("shadow-items")
    return [("forged", ("Forged", 9))]


subject._store.items = forged_items
subject._store.move_to_end("first")
result = {
    "snapshot": cid_call(subject, "core_snapshot"),
    "keys": list(subject),
}
"""
    )


def test_case_insensitive_custom_key_does_not_delay_value_destruction() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict()


class Tracked:
    def __del__(self):
        side_effects.append("drop")


class ClearingKey(str):
    def lower(self):
        subject._store.clear()
        side_effects.append("after-clear")
        return "token"


subject["Token"] = Tracked()
try:
    cid_call(subject, "get", ClearingKey("TOKEN"))
except KeyError:
    outcome = "missing"
else:
    outcome = "found"

result = {"outcome": outcome}
"""
    )


def test_lookup_dict_exact_instance_honors_shadowed_get() -> None:
    _assert_matches_oracle(
        """
from requests.structures import LookupDict

subject = LookupDict("lookup")
subject.answer = 1


def shadowed_get(*arguments):
    side_effects.append(["shadow-get", list(arguments)])
    return 9


subject.get = shadowed_get
result = lookup_call(subject, "get", "answer", "fallback")
"""
    )


def test_case_insensitive_store_lookup_preserves_first_error_without_retry() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict({"Token": 1})
original_getattribute = CaseInsensitiveDict.__getattribute__
store_lookups = []


def stateful_getattribute(self, name):
    if name == "_store":
        store_lookups.append(name)
        if len(store_lookups) == 1:
            raise RuntimeError("first store lookup")
    return original_getattribute(self, name)


CaseInsensitiveDict.__getattribute__ = stateful_getattribute
try:
    try:
        cid_call(subject, "get", "TOKEN")
    except RuntimeError as error:
        outcome = [type(error).__name__, str(error)]
    else:
        outcome = "returned"
finally:
    CaseInsensitiveDict.__getattribute__ = original_getattribute

result = {"outcome": outcome, "store_lookups": len(store_lookups)}
"""
    )


def test_case_insensitive_raw_getattribute_descriptor_reads_store_once() -> None:
    _assert_matches_oracle(
        """
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict({"Token": 1})
original_getattribute = CaseInsensitiveDict.__getattribute__
store_lookups = []


class RawGetAttribute:
    def __get__(self, instance, owner):
        if instance is None:
            return object.__getattribute__

        def getattribute(name):
            if name == "_store":
                store_lookups.append(name)
            return object.__getattribute__(instance, name)

        return getattribute


CaseInsensitiveDict.__getattribute__ = RawGetAttribute()
try:
    value = cid_call(subject, "get", "TOKEN")
finally:
    CaseInsensitiveDict.__getattribute__ = original_getattribute

result = {"value": value, "store_lookups": store_lookups}
"""
    )


def test_case_insensitive_rebound_public_class_is_not_a_trusted_fast_path() -> None:
    _assert_matches_oracle(
        """
from collections import OrderedDict
import requests.structures as structures

original_class = structures.CaseInsensitiveDict


class Replacement:
    def __init__(self):
        self._store = OrderedDict({"token": ("Token", 9)})

    def __getitem__(self, key):
        side_effects.append(["replacement-getitem", key])
        return 1


structures.CaseInsensitiveDict = Replacement
try:
    subject = Replacement()
    result = cid_call(subject, "get", "TOKEN")
finally:
    structures.CaseInsensitiveDict = original_class
"""
    )


def test_core_snapshot_does_not_trust_rebound_ordered_dict_class() -> None:
    _assert_matches_oracle(
        """
import collections
from requests.structures import CaseInsensitiveDict

subject = CaseInsensitiveDict({"Token": 1})
original_class = collections.OrderedDict


class Replacement(dict):
    pass


collections.OrderedDict = Replacement
try:
    result = cid_call(subject, "core_snapshot")
finally:
    collections.OrderedDict = original_class
"""
    )


def test_lookup_dict_rebound_public_class_keeps_dynamic_get_semantics() -> None:
    _assert_matches_oracle(
        """
import requests.structures as structures

original_class = structures.LookupDict


class Replacement:
    def __init__(self):
        self.answer = 1

    def get(self, *arguments):
        side_effects.append(["replacement-get", list(arguments)])
        return 9


structures.LookupDict = Replacement
try:
    subject = Replacement()
    result = lookup_call(subject, "get", "answer", "fallback")
finally:
    structures.LookupDict = original_class
"""
    )
