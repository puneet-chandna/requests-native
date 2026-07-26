from __future__ import annotations

import pytest


def proxy_case(source: str) -> dict[str, str]:
    return {
        "source": f"""
import os
import requests
from requests import models, utils

{source}

if os.environ["REQUESTS_DIFFERENTIAL_TARGET"] == "rewrite":
    resolved, selected = requests._requests_rust._select_proxy_trial(
        url, proxies, trust_env
    )
else:
    request = models.PreparedRequest()
    request.url = url
    resolved = utils.resolve_proxies(request, proxies, trust_env)
    selected = utils.select_proxy(url, resolved)

result = {{"resolved": dict(resolved), "selected": selected}}
"""
    }


@pytest.mark.parametrize(
    "source",
    [
        """
url = "http://example.test/path"
trust_env = False
proxies = {
    "all": "http://all.invalid",
    "all://example.test": "http://all-host.invalid",
    "http": "http://scheme.invalid",
    "http://example.test": "http://scheme-host.invalid",
}
""",
        """
url = "https://other.test/path"
trust_env = False
proxies = {
    "all": "http://all.invalid",
    "all://example.test": "http://all-host.invalid",
}
""",
        """
os.environ["HTTP_PROXY"] = "http://environment.invalid"
os.environ["NO_PROXY"] = "bypass.test"
url = "http://bypass.test/path"
trust_env = True
proxies = {}
""",
        """
os.environ["HTTPS_PROXY"] = "http://environment.invalid"
url = "https://selected.test/path"
trust_env = True
proxies = {"no_proxy": "other.test"}
""",
    ],
)
def test_proxy_resolution_and_precedence_stay_python_authoritative(
    run_differential_case, source
):
    oracle, rewrite = run_differential_case(proxy_case(source))
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""


def test_proxy_trial_honors_runtime_helper_monkeypatches(run_differential_case):
    case = proxy_case(
        """
url = "http://patched.test/path"
trust_env = True
proxies = {"http": "http://ignored.invalid"}
side_effects = []

def patched_resolve(request, supplied, trust_env):
    side_effects.append(["resolve", request.url, dict(supplied), trust_env])
    return {"http": "http://patched.invalid"}

def patched_select(selected_url, resolved):
    side_effects.append(["select", selected_url, dict(resolved)])
    return "http://selected-by-patch.invalid"

utils.resolve_proxies = patched_resolve
utils.select_proxy = patched_select
"""
    )
    oracle, rewrite = run_differential_case(case)
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""
