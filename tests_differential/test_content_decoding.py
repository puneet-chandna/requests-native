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
ZLIB_DEFLATE_HEX = (
    "789c4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330"
    "b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00baaa24b9"
)
RAW_DEFLATE_HEX = (
    "4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a48630032313330b2b1"
    "b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00"
)
EMPTY_FRAMES = {
    "gzip": "1f8b08000000000002ff03000000000000000000",
    "deflate-zlib": "789c030000000001",
    "deflate-raw": "0300",
    "br": "3b",
    "zstd": "28b52ffd2000010000",
}
CONCATENATED_FRAMES = {
    "gzip": (
        "1f8b08000000000002ff4bcb2c2a2ed1cd4dcd4d4a2db20200dad159b70d000000"
        "1f8b08000000000002ff2b4e4dcecf4bd1cd4dcd4d4a2d520400a7f4f2390e000000"
    ),
    "deflate-zlib": (
        "789c4bcb2c2a2ed1cd4dcd4d4a2db2020024560508"
        "789c2b4e4dcecf4bd1cd4dcd4d4a2d52040029500543"
    ),
    "deflate-raw": ("4bcb2c2a2ed1cd4dcd4d4a2db202002b4e4dcecf4bd1cd4dcd4d4a2d520400"),
    "zstd": (
        "28b52ffd200d69000066697273742d6d656d6265723a"
        "28b52ffd200e7100007365636f6e642d6d656d62657221"
    ),
}
LARGE_BROTLI_HEX = (
    "1b0208e82f0e78d39c142d393ff0606ad8b8b50331b06d7b93d53a382fe4be531c9c"
    "57721e71102a1c23e4bd320dd1c4d1440a62824d1cbbf7efe80a20c284322ea4d2c63"
    "a1f62caa5b63ee6dae7be0f80108ca0184e9014cdb01c2f8892aca89a6e9896edb89"
    "e1f84519ca4595e9455ddb45d3f8cd3bcacdb7e9cd7fdbcdfef0f201841319c20299a"
    "61395e1025595135dd302ddb713d3f08a33849b3bc28abba69bb7e18a77959b7fd38a"
    "ffb79bf1f408409655c48a58d753e08a33849b3bc28abba69bb7e18a77959b7fd38a"
    "ffb79bf1fc4a2e691d5b3f7d1923606"
)
LARGE_ZSTANDARD_CHECKSUM_HEX = (
    "28b52ffd6403076d08003410000102030405060708090a0b0c0d0e0f10111213141516"
    "1718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30313233343536373839"
    "3a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c"
    "5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e"
    "7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0"
    "a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1"
    "c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2"
    "e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff656e6401"
    "0000fd0efc6b0a8ee43ec5"
)
CORRUPT_VECTORS = {
    "gzip": (
        "1f8b08000000000002ff4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949a"
        "c49a48630032313330b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48"
        "cac9c3c00f1b1f4157c000000"
    ),
    "deflate-zlib": (
        "789c4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac49a486300323133"
        "30b2b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00baaa2446"
    ),
    "deflate-raw": (
        "4bcc29c848d44d4a2d49d44d4fcccd4dd44d49cd2949ac495b8630032313330b2"
        "b1b3b072717370f2f1fbf80a090b088a898b884a494b48cac9c3c00"
    ),
    "br": (
        "1b7a00e80572714853f8ae5dd22c2c0d19e4aa541a32e9e7b8ca878604b5f77842830f484801"
    ),
    "zstd": LARGE_ZSTANDARD_CHECKSUM_HEX[:-2] + "c4",
}

CODEC_VECTORS = {
    "gzip": GZIP_HEX,
    "deflate-zlib": ZLIB_DEFLATE_HEX,
    "deflate-raw": RAW_DEFLATE_HEX,
    "br": BROTLI_HEX,
    "zstd": ZSTANDARD_HEX,
}
ENCODINGS = {
    "gzip": "gzip",
    "deflate-zlib": "deflate",
    "deflate-raw": "deflate",
    "br": "br",
    "zstd": "zstd",
}
LARGE = bytes(range(256)) * 8 + b"end"
DECODE_ERROR = {
    "type": ("requests.exceptions", "ContentDecodingError"),
    "arg_count": 1,
    "argument": ("urllib3.exceptions", "DecodeError"),
    "argument_is_context": True,
}

_DECODING_SOURCE = dedent(
    """
    import io

    import requests
    from requests.exceptions import ContentDecodingError
    from urllib3.response import HTTPResponse

    class CappedBody(io.BytesIO):
        def __init__(self, data, cap):
            super().__init__(data)
            self.cap = cap
            self.read_count = 0
            self.max_read = 0

        def read(self, amount=-1):
            amount = self.cap if amount is None or amount < 0 else min(amount, self.cap)
            data = super().read(amount)
            self.read_count += 1
            self.max_read = max(self.max_read, len(data))
            return data

    def make_response(encoding, wire, cap):
        body = CappedBody(wire, cap)
        raw = HTTPResponse(
            body=body,
            headers={
                "Content-Encoding": encoding,
                "Content-Length": str(len(wire)),
            },
            preload_content=False,
            decode_content=False,
        )
        response = requests.Response()
        response.status_code = 200
        response.url = "http://example.test/content-decoding"
        response.raw = raw
        response.headers = requests.structures.CaseInsensitiveDict(raw.headers)
        return response, body

    def stable_error(error):
        argument = error.args[0] if len(error.args) == 1 else None
        return {
            "type": (type(error).__module__, type(error).__qualname__),
            "arg_count": len(error.args),
            "argument": None
            if argument is None
            else (type(argument).__module__, type(argument).__qualname__),
            "argument_is_context": argument is error.__context__,
        }

    def iteration_state(encoding, wire_hex, chunk_size=7, cap=3):
        response, body = make_response(encoding, bytes.fromhex(wire_hex), cap)
        iterator = response.iter_content(chunk_size)
        lazy = {
            "content_is_false": response._content is False,
            "content_consumed": response._content_consumed,
            "raw_reads": body.read_count,
        }
        chunks = []
        error = None
        try:
            while True:
                chunks.append(next(iterator))
        except StopIteration:
            pass
        except ContentDecodingError as caught:
            error = stable_error(caught)
        return {
            "lazy": lazy,
            "chunks": [chunk.hex() for chunk in chunks],
            "sizes": [len(chunk) for chunk in chunks],
            "joined": b"".join(chunks).hex(),
            "error": error,
            "content_is_false": response._content is False,
            "content_consumed": response._content_consumed,
            "raw_reads": body.read_count,
            "max_raw_read": body.max_read,
        }

    def content_state(encoding, wire_hex, cap=3):
        response, body = make_response(encoding, bytes.fromhex(wire_hex), cap)
        value = None
        error = None
        try:
            value = response.content.hex()
        except ContentDecodingError as caught:
            error = stable_error(caught)
        return {
            "value": value,
            "error": error,
            "content_is_false": response._content is False,
            "content_consumed": response._content_consumed,
            "raw_reads": body.read_count,
            "max_raw_read": body.max_read,
        }
    """
)


def _snapshot(source: str) -> dict[str, object]:
    case = {"source": f"{_DECODING_SOURCE}\nresult = {source}\n"}
    oracle = run_oracle_case(case)
    rewrite = run_rewrite_case(case)

    assert oracle.observations["exception"] is None
    assert rewrite.observations == oracle.observations
    assert rewrite.stderr == oracle.stderr == ""
    return ast.literal_eval(oracle.observations["result"]["repr"])


def _chunks(payload: bytes, size: int = 7) -> list[str]:
    return [
        payload[offset : offset + size].hex() for offset in range(0, len(payload), size)
    ]


def _successful_iteration(
    payload: bytes,
    wire_hex: str,
    *,
    chunk_size: int = 7,
    cap: int = 3,
) -> dict[str, object]:
    wire_length = len(bytes.fromhex(wire_hex))
    chunks = _chunks(payload, chunk_size)
    return {
        "lazy": {
            "content_is_false": True,
            "content_consumed": False,
            "raw_reads": 0,
        },
        "chunks": chunks,
        "sizes": [len(bytes.fromhex(chunk)) for chunk in chunks],
        "joined": payload.hex(),
        "error": None,
        "content_is_false": True,
        "content_consumed": True,
        "raw_reads": (wire_length + cap - 1) // cap + 1,
        "max_raw_read": min(cap, wire_length),
    }


def _failed_iteration(
    payload: bytes,
    *,
    raw_reads: int,
    chunk_size: int = 7,
    max_raw_read: int = 3,
) -> dict[str, object]:
    chunks = _chunks(payload, chunk_size)
    return {
        "lazy": {
            "content_is_false": True,
            "content_consumed": False,
            "raw_reads": 0,
        },
        "chunks": chunks,
        "sizes": [len(bytes.fromhex(chunk)) for chunk in chunks],
        "joined": payload.hex(),
        "error": DECODE_ERROR,
        "content_is_false": True,
        "content_consumed": False,
        "raw_reads": raw_reads,
        "max_raw_read": max_raw_read,
    }


def _failed_content(*, raw_reads: int, max_raw_read: int = 3) -> dict[str, object]:
    return {
        "value": None,
        "error": DECODE_ERROR,
        "content_is_false": True,
        "content_consumed": False,
        "raw_reads": raw_reads,
        "max_raw_read": max_raw_read,
    }


def _successful_content(
    payload: bytes,
    wire_hex: str,
    *,
    cap: int = 3,
) -> dict[str, object]:
    wire_length = len(bytes.fromhex(wire_hex))
    return {
        "value": payload.hex(),
        "error": None,
        "content_is_false": False,
        "content_consumed": True,
        "raw_reads": (wire_length + cap - 1) // cap + 1,
        "max_raw_read": min(cap, wire_length),
    }


def test_iter_content_success_and_empty_frames_match_exact_oracle_snapshots() -> None:
    calls = {
        name: f"iteration_state({ENCODINGS[name]!r}, {wire_hex!r})"
        for name, wire_hex in CODEC_VECTORS.items()
    }
    calls.update(
        {
            f"empty-wire-{name}": f"iteration_state({encoding!r}, '')"
            for name, encoding in ENCODINGS.items()
        }
    )
    calls.update(
        {
            f"empty-frame-{name}": (
                f"iteration_state({ENCODINGS[name]!r}, {wire_hex!r})"
            )
            for name, wire_hex in EMPTY_FRAMES.items()
        }
    )
    state = _snapshot(
        "{"
        + ", ".join(f"{name!r}: {expression}" for name, expression in calls.items())
        + "}"
    )

    expected = {
        name: _successful_iteration(bytes.fromhex(PAYLOAD_HEX), wire_hex)
        for name, wire_hex in CODEC_VECTORS.items()
    }
    expected.update(
        {f"empty-wire-{name}": _successful_iteration(b"", "") for name in ENCODINGS}
    )
    expected.update(
        {
            f"empty-frame-{name}": _successful_iteration(b"", wire_hex)
            for name, wire_hex in EMPTY_FRAMES.items()
        }
    )
    assert state == expected
    for name in CODEC_VECTORS:
        assert state[name]["raw_reads"] > 1
        assert state[name]["max_raw_read"] <= 3
        assert all(0 < size <= 7 for size in state[name]["sizes"])


def test_concatenated_codec_streams_match_exact_oracle_snapshots() -> None:
    frames = {
        **CONCATENATED_FRAMES,
        "br": LARGE_BROTLI_HEX * 2,
    }
    state = _snapshot(
        "{"
        + ", ".join(
            f"{name!r}: iteration_state({ENCODINGS[name]!r}, {wire_hex!r})"
            for name, wire_hex in frames.items()
        )
        + "}"
    )

    both_members = b"first-member:" + b"second-member!"
    expected = {
        "gzip": _successful_iteration(both_members, frames["gzip"]),
        "deflate-zlib": _successful_iteration(b"first-member:", frames["deflate-zlib"]),
        "deflate-raw": _successful_iteration(b"first-member:", frames["deflate-raw"]),
        "zstd": _successful_iteration(both_members, frames["zstd"]),
    }
    assert {name: state[name] for name in expected} == expected

    # urllib3 may select either brotli or brotlicffi. They fail at different
    # input boundaries for this concatenated stream, so exact differential
    # equality is enforced by _snapshot while these portable invariants pin
    # the shared public behavior.
    brotli = state["br"]
    joined = bytes.fromhex(brotli["joined"])
    assert LARGE.startswith(joined)
    assert len(LARGE) - 7 <= len(joined) <= len(LARGE)
    assert brotli["chunks"] == _chunks(joined)
    assert brotli["sizes"] == [len(bytes.fromhex(chunk)) for chunk in brotli["chunks"]]
    assert brotli["error"] == DECODE_ERROR
    assert brotli["lazy"] == {
        "content_is_false": True,
        "content_consumed": False,
        "raw_reads": 0,
    }
    assert brotli["content_is_false"] is True
    assert brotli["content_consumed"] is False
    assert 1 < brotli["raw_reads"] <= (len(bytes.fromhex(frames["br"])) + 2) // 3 + 1
    assert brotli["max_raw_read"] == 3


def test_corrupt_codec_iteration_and_content_match_exact_oracle_snapshots() -> None:
    state = _snapshot(
        "{"
        + ", ".join(
            (
                f"{name!r}: {{"
                f"'iteration': iteration_state({ENCODINGS[name]!r}, {wire_hex!r}), "
                f"'content': content_state({ENCODINGS[name]!r}, {wire_hex!r})"
                "}"
            )
            for name, wire_hex in CORRUPT_VECTORS.items()
        )
        + "}"
    )

    prefixes_and_reads = {
        "gzip": (bytes.fromhex(PAYLOAD_HEX)[:119], 25),
        "deflate-zlib": (bytes.fromhex(PAYLOAD_HEX)[:119], 22),
        "deflate-raw": (bytes.fromhex(PAYLOAD_HEX)[:21], 10),
        "br": (bytes.fromhex(PAYLOAD_HEX)[:105], 13),
        "zstd": (LARGE, 95),
    }
    expected = {
        name: {
            "iteration": _failed_iteration(prefix, raw_reads=raw_reads),
            "content": _failed_content(raw_reads=raw_reads),
        }
        for name, (prefix, raw_reads) in prefixes_and_reads.items()
    }
    assert state == expected


def test_selected_accepted_truncations_match_exact_oracle_snapshots() -> None:
    payload = bytes.fromhex(PAYLOAD_HEX)
    cases = {
        "gzip": (GZIP_HEX[:-16], payload),
        "deflate-zlib": (ZLIB_DEFLATE_HEX[:-4], payload),
        "deflate-raw": (RAW_DEFLATE_HEX[:-4], payload[:123]),
        "br": (BROTLI_HEX[:-2], payload[:116]),
        "zstd": (ZSTANDARD_HEX[:-2], b""),
    }
    state = _snapshot(
        "{"
        + ", ".join(
            (
                f"{name!r}: {{"
                f"'iteration': iteration_state({ENCODINGS[name]!r}, {wire_hex!r}), "
                f"'content': content_state({ENCODINGS[name]!r}, {wire_hex!r})"
                "}"
            )
            for name, (wire_hex, _) in cases.items()
        )
        + "}"
    )

    assert state == {
        name: {
            "iteration": _successful_iteration(expected, wire_hex),
            "content": _successful_content(expected, wire_hex),
        }
        for name, (wire_hex, expected) in cases.items()
    }


def test_checksum_zstd_truncation_keeps_chunk_sensitive_oracle_distinction() -> None:
    truncated = LARGE_ZSTANDARD_CHECKSUM_HEX[:-2]
    state = _snapshot(
        "{"
        f"'iteration': iteration_state('zstd', {truncated!r}, chunk_size=17, cap=64), "
        f"'content': content_state('zstd', {truncated!r}, cap=64)"
        "}"
    )

    assert state == {
        "iteration": _failed_iteration(
            LARGE[:2040],
            raw_reads=18,
            chunk_size=17,
            max_raw_read=17,
        ),
        "content": _successful_content(
            LARGE,
            truncated,
            cap=64,
        ),
    }


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
