from __future__ import annotations

from textwrap import dedent

from tests_differential.runner import run_oracle_case, run_rewrite_case


def test_generated_url_header_cookie_redirect_and_body_boundaries_match_oracle() -> (
    None
):
    source = r"""
import random
from contextlib import nullcontext
from types import SimpleNamespace
import requests
from requests.cookies import RequestsCookieJar
from requests.models import PreparedRequest, Response
from requests.sessions import Session
from requests.structures import CaseInsensitiveDict

trial = getattr(requests, "_rust_public_trial", nullcontext)

def exercise(context_factory):
    rng = random.Random(1844674407370955161)
    urls = []
    headers = []
    cookies = []
    redirects = []
    bodies = []
    with context_factory():
        for index in range(64):
            label = "".join(chr(97 + rng.randrange(26)) for _ in range(9))
            request = PreparedRequest()
            request.prepare_url(
                f" HTTP://{label}.EXAMPLE/a path/{index}?first={rng.randrange(256)}#frag",
                [("next", str(rng.randrange(256)))],
            )
            urls.append(request.url)

            mapping = CaseInsensitiveDict()
            name = f"X-{rng.randrange(16):X}"
            mapping[name] = f"first-{index}"
            mapping["Y-Test"] = f"middle-{rng.randrange(256)}"
            mapping[name.lower()] = f"last-{rng.randrange(256)}"
            request.prepare_headers(mapping)
            headers.append(list(request.headers.items()))

            jar = RequestsCookieJar()
            shared = f"cookie-{rng.randrange(4)}"
            jar.set(shared, f"first-{index}", domain="a.example", path="/")
            jar.set(f"other-{index}", str(rng.randrange(256)), domain="a.example", path="/")
            jar.set(shared, f"last-{index}", domain="b.example", path="/")
            cookies.append([
                jar.get_dict(domain="a.example", path="/"),
                jar.get_dict(domain="b.example", path="/"),
            ])

            session = Session()
            old = f"http://host{index % 4}.example:{80 if index % 2 else 81}/a"
            new = f"https://host{index % 5}.example:{443 if index % 3 else 444}/b"
            response = Response()
            response.status_code = [301, 302, 303, 307, 308][index % 5]
            response.headers["location"] = f"/next/{index}"
            redirects.append([
                session.should_strip_auth(old, new),
                session.get_redirect_target(response),
            ])
            session.close()

            payload = bytes(rng.randrange(256) for _ in range(1 + index % 23))
            amount = 1 + rng.randrange(9)
            class Raw:
                def stream(self, chunk_size, decode_content=True):
                    for offset in range(0, len(payload), chunk_size):
                        yield payload[offset:offset + chunk_size]
            response = Response()
            response.raw = Raw()
            bodies.append([chunk.hex() for chunk in response.iter_content(amount)])
    return {
        "urls": urls,
        "headers": headers,
        "cookies": cookies,
        "redirects": redirects,
        "bodies": bodies,
    }

result = SimpleNamespace(default=exercise(nullcontext), trial=exercise(trial))
"""
    case = {"source": dedent(source)}
    assert run_rewrite_case(case) == run_oracle_case(case)
