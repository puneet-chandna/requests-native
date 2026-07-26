from __future__ import annotations

import ast
from textwrap import dedent

import pytest

from tests_differential.runner import run_oracle_case, run_rewrite_case


PAYLOAD_HEX = (
    "616c7068612d626574612d67616d6d612d64656c74617c"
    "616c7068612d626574612d67616d6d612d64656c74617c"
    "616c7068612d626574612d67616d6d612d64656c74617c"
    "616c7068612d626574612d67616d6d612d64656c74617c"
    "000102030405060708090a0b0c0d0e0f"
    "101112131415161718191a1b1c1d1e1f"
)
GZIP_HEX = (
    "1f8b08000000000002ff4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a486300"
    "32313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00fe41f4157c"
    "000000"
)
BROTLI_HEX = (
    "1b7b00e80572714853f8ae5dd22c2c0d19e4aa541a32e9e7b8ca878604b5f77842830f484801"
)
ZSTANDARD_HEX = (
    "28b52ffd207cfd01007403616c7068612d626574612d67616d6d612d64656c74617c00010203"
    "0405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f010045d1d904"
)


def _inventory_source(*, brotli: bool, zstandard: bool) -> str:
    return dedent(
        f"""
        import builtins
        import io

        enable_brotli = {brotli!r}
        enable_zstandard = {zstandard!r}
        original_import = builtins.__import__

        def inventory_import(name, globals=None, locals=None, fromlist=(), level=0):
            if level == 0:
                root = name.split(".", 1)[0]
                if not enable_brotli and root in {{"brotli", "brotlicffi"}}:
                    raise ImportError("Brotli blocked by inventory probe")
                if not enable_zstandard and (
                    name.startswith("compression.zstd")
                    or name.startswith("backports.zstd")
                    or (name == "compression" and "zstd" in fromlist)
                    or (name == "backports" and "zstd" in fromlist)
                ):
                    raise ImportError("Zstandard blocked by inventory probe")
            return original_import(name, globals, locals, fromlist, level)

        builtins.__import__ = inventory_import
        try:
            import requests
            from urllib3.response import HTTPResponse
        finally:
            builtins.__import__ = original_import

        def decode(encoding, wire_hex):
            wire = bytes.fromhex(wire_hex)
            response = HTTPResponse(
                body=io.BytesIO(wire),
                headers={{
                    "Content-Encoding": encoding,
                    "Content-Length": str(len(wire)),
                    "X-Wire-Fixture": "preserved",
                }},
                preload_content=False,
                decode_content=False,
            )
            body = response.read(decode_content=True)
            return {{
                "body": body.hex(),
                "content_encoding": response.headers["Content-Encoding"],
                "content_length": response.headers["Content-Length"],
                "wire_fixture": response.headers["X-Wire-Fixture"],
            }}

        session = requests.Session()
        prepared = session.prepare_request(
            requests.Request("GET", "http://example.test/inventory")
        )
        session.close()

        result = {{
            "accept_encoding": prepared.headers["Accept-Encoding"],
            "decoders": list(HTTPResponse.CONTENT_DECODERS),
            "x_gzip": decode("x-gzip", {GZIP_HEX!r}),
            "brotli": decode("br", {BROTLI_HEX!r}),
            "zstandard": decode("zstd", {ZSTANDARD_HEX!r}),
        }}
        """
    )


@pytest.mark.parametrize(
    ("brotli", "zstandard", "accept_encoding"),
    [
        (True, True, "gzip, deflate, br, zstd"),
        (False, True, "gzip, deflate, zstd"),
        (True, False, "gzip, deflate, br"),
        (False, False, "gzip, deflate"),
    ],
)
def test_fresh_process_optional_codec_inventory_matches_oracle(
    brotli: bool,
    zstandard: bool,
    accept_encoding: str,
) -> None:
    case = {"source": _inventory_source(brotli=brotli, zstandard=zstandard)}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""

    state = ast.literal_eval(oracle.observations["result"]["repr"])
    expected_decoders = ["gzip", "x-gzip", "deflate"]
    if brotli:
        expected_decoders.append("br")
    if zstandard:
        expected_decoders.append("zstd")

    assert state == {
        "accept_encoding": accept_encoding,
        "decoders": expected_decoders,
        "x_gzip": {
            "body": PAYLOAD_HEX,
            "content_encoding": "x-gzip",
            "content_length": str(len(bytes.fromhex(GZIP_HEX))),
            "wire_fixture": "preserved",
        },
        "brotli": {
            "body": PAYLOAD_HEX if brotli else BROTLI_HEX,
            "content_encoding": "br",
            "content_length": str(len(bytes.fromhex(BROTLI_HEX))),
            "wire_fixture": "preserved",
        },
        "zstandard": {
            "body": PAYLOAD_HEX if zstandard else ZSTANDARD_HEX,
            "content_encoding": "zstd",
            "content_length": str(len(bytes.fromhex(ZSTANDARD_HEX))),
            "wire_fixture": "preserved",
        },
    }
