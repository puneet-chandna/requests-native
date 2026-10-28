"""
requests._internal_utils
~~~~~~~~~~~~~~

Provides utility functions that are consumed internally by Requests
which depend on extremely few external helpers (such as compat)
"""

import re

from urllib.parse import ParseResult

from .compat import builtin_str, urlparse

_VALID_HEADER_NAME_RE_BYTE = re.compile(rb"^[^:\s][^:\r\n]*\Z")
_VALID_HEADER_NAME_RE_STR = re.compile(r"^[^:\s][^:\r\n]*\Z")
_VALID_HEADER_VALUE_RE_BYTE = re.compile(rb"^\S[^\r\n]*\Z|^\Z")
_VALID_HEADER_VALUE_RE_STR = re.compile(r"^\S[^\r\n]*\Z|^\Z")

_HEADER_VALIDATORS_STR = (_VALID_HEADER_NAME_RE_STR, _VALID_HEADER_VALUE_RE_STR)
_HEADER_VALIDATORS_BYTE = (_VALID_HEADER_NAME_RE_BYTE, _VALID_HEADER_VALUE_RE_BYTE)
HEADER_VALIDATORS = {
    bytes: _HEADER_VALIDATORS_BYTE,
    str: _HEADER_VALIDATORS_STR,
}


_LAST_PARSED_URL: tuple[str, ParseResult] | None = None


def urlparse_cached(url: str) -> ParseResult:
    """Return ``urlparse(url)``, reusing the parse of the same url string.

    One request consults its own url several times while it is prepared, sent
    and read back for cookies; those consults follow one another on the same
    string, so the previous parse answers them. Only that exact string hits the
    memo, and parse results are immutable.

    :rtype: ParseResult
    """
    global _LAST_PARSED_URL
    memo = _LAST_PARSED_URL
    if memo is not None and memo[0] == url:
        return memo[1]
    parsed = urlparse(url)
    _LAST_PARSED_URL = (url, parsed)
    return parsed


def to_native_string(string: str | bytes, encoding: str = "ascii") -> str:
    """Given a string object, regardless of type, returns a representation of
    that string in the native string type, encoding and decoding where
    necessary. This assumes ASCII unless told otherwise.
    """
    if isinstance(string, builtin_str):
        out = string
    else:
        out = string.decode(encoding)

    return out


def unicode_is_ascii(u_string: str) -> bool:
    """Determine if unicode string only contains ASCII characters.

    :param str u_string: unicode string to check. Must be unicode
        and not Python 2 `str`.
    :rtype: bool
    """
    assert isinstance(u_string, str)
    try:
        u_string.encode("ascii")
        return True
    except UnicodeEncodeError:
        return False
