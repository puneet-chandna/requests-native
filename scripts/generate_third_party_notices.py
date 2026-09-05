#!/usr/bin/env python3
"""Generate the auditable third-party license corpus embedded in NOTICE."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from collections import deque
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NOTICE = ROOT / "NOTICE"
RUNTIME_NOTICE = ROOT / "RUST_RUNTIME_NOTICES.html"
WORKSPACE_PACKAGE = "requests-native-python"
LICENSE_PREFIXES = ("COPYING", "COPYRIGHT", "LICENCE", "LICENSE", "NOTICE", "UNLICENSE")
RUNTIME_TOOLCHAIN = "1.98.0"
RUNTIME_COMMIT = "88d9e12ae178fab0fb5cc050a94da85685d449ea"
RUNTIME_NOTICE_SHA256 = (
    "68129500b616d5838629e68f55ff3aed5e096dacf60ce9eb41bbe599a563afa6"
)
ALLOC_STDLIB_LICENSE = {
    "package": ("alloc-stdlib", "0.2.4"),
    "source": ("alloc-no-stdlib", "2.0.4", "LICENSE"),
    "upstream_commit": "ae42d22078b98549e987d2f03d12df7b984fde47",
    "upstream_blob": "cd496ba72eb37e83a44958358d1f89a8a28cbc15",
    "sha256": "c0c56f26d9c051cac4d200c34c84e7ae9aaa853e01a982a1df08b09931e518ae",
}
REVIEWED_OBLIGATIONS = {
    "(MIT OR Apache-2.0) AND Unicode-3.0": "MIT AND Unicode-3.0",
    "0BSD OR MIT OR Apache-2.0": "MIT",
    "Apache-2.0 AND ISC": "Apache-2.0 AND ISC",
    "Apache-2.0 OR ISC OR MIT": "MIT",
    "Apache-2.0 OR MIT": "MIT",
    "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT": "MIT",
    "BSD-3-Clause": "BSD-3-Clause",
    "BSD-3-Clause AND MIT": "BSD-3-Clause AND MIT",
    "BSD-3-Clause/MIT": "BSD-3-Clause AND MIT",
    "ISC": "ISC",
    "MIT": "MIT",
    "MIT OR Apache-2.0": "MIT",
    "MIT OR Zlib OR Apache-2.0": "MIT",
    "MIT/Apache-2.0": "MIT",
    "Unicode-3.0": "Unicode-3.0",
    "Unlicense OR MIT": "MIT",
    "Zlib": "Zlib",
}
CURATED_FILES = {
    ("zstd-sys", "2.0.16+zstd.1.5.7"): (
        (
            "Bundled Zstandard 1.5.7 C library BSD-3-Clause license",
            "zstd/LICENSE",
            "7055266497633c9025b777c78eb7235af13922117480ed5c674677adc381c9d8",
            "full",
        ),
    ),
    ("brotli-decompressor", "5.0.3"): (
        (
            "Google MIT context-table notice from src/context.rs",
            "src/context.rs",
            "8fe7865962916fa7ce820b9fb407e6a2dd9b9c373191ad4a41c507b6be373e40",
            "header",
        ),
    ),
    ("brotli", "8.0.4"): (
        (
            "Google MIT encoder notice from src/enc/entropy_encode.rs",
            "src/enc/entropy_encode.rs",
            "1427f28f081792415f7ab352ece9d0820d7ab07e2318c0d4d190cd96131e32ab",
            "header",
        ),
    ),
    ("ring", "0.17.14"): (
        (
            "once_cell Apache-2.0 license",
            "src/polyfill/once_cell/LICENSE-APACHE",
            "a60eea817514531668d7e00765731449fe14d059d3249e0bc93b36de45f759f2",
            "full",
        ),
        (
            "once_cell MIT license",
            "src/polyfill/once_cell/LICENSE-MIT",
            "6ee2ed6c77710de911761acd5fc1ad1da00f476beb1a7ef27e78c2d1858deafc",
            "full",
        ),
        (
            "fiat-crypto Apache-2.0 license",
            "third_party/fiat/LICENSE",
            "9eacbcb81be660840c714a560a9d65ba07913db98dd4baf969f78dd499fdd60f",
            "full",
        ),
        (
            "Google ISC notice",
            "crypto/fipsmodule/aes/aes_nohw.c",
            "a78fb13eaf6704c6ab4109efc77dc14cabcefd7c8c78cd983e4daad97bbce42b",
            "header",
        ),
        (
            "Intel and OpenSSL Apache-2.0 notice",
            "crypto/fipsmodule/ec/asm/p256-x86_64-asm.pl",
            "454c278064dbae0f718bb2e415efee97e31a38958776623574632961b24e34ca",
            "header",
        ),
        (
            "Intel ISC notice",
            "crypto/fipsmodule/ec/ecp_nistz.c",
            "d5d2359807cea970cdbd059935322890a5535c39b8abf633d4324656392d527d",
            "header",
        ),
        (
            "Google ISC notice",
            "crypto/fipsmodule/ec/ecp_nistz.h",
            "ba6982c928bbbc3c71e9ef2a7bd0e1f4ca3436bf3d178e0c5727fbcd49dfea61",
            "header",
        ),
        (
            "Intel ISC notice",
            "crypto/fipsmodule/ec/ecp_nistz384.h",
            "7aeb84e1d64b1018d8e0c89b97ae1e603f31f9cd4786104ce5932f4d425bb1d7",
            "header",
        ),
        (
            "Intel ISC notice and authors",
            "crypto/fipsmodule/ec/ecp_nistz384.inl",
            "0229835ebc9bdfe9268c53cf3e5d346831a9b2f66377a320d92407f2293b5412",
            "header",
        ),
        (
            "Intel and OpenSSL Apache-2.0 notice",
            "crypto/fipsmodule/ec/p256-nistz-table.h",
            "eae4169efa9021ca93cf265be4de56bf1cb77e19c2957573e363c99a166953aa",
            "header",
        ),
        (
            "Intel and OpenSSL Apache-2.0 notice",
            "crypto/fipsmodule/ec/p256-nistz.c",
            "033e609fc9709c20f4b46c54760a1386a439b95585759bbd7ebaa3024a051133",
            "header",
        ),
        (
            "Intel and OpenSSL Apache-2.0 notice",
            "crypto/fipsmodule/ec/p256-nistz.h",
            "41d88cdd8f66e7ded5113290790e915c07c263c0e778078c7c76311d441b1513",
            "header",
        ),
        (
            "Intel and OpenSSL Apache-2.0 notice",
            "crypto/fipsmodule/ec/p256_shared.h",
            "1a69c500d344254a43afa37c2a6887f30d07d7d21ffe3a13988894171c98df48",
            "header",
        ),
        (
            "Google ISC notice",
            "crypto/poly1305/poly1305.c",
            "ef9031a370c97e16e7784eb70a982bf718734058ed960fd4c76f08385f03f2f7",
            "header",
        ),
        (
            "Google ISC notice",
            "crypto/poly1305/poly1305_arm.c",
            "2b2fc2e6c7b989d4d73626d4f15006b896b3eb16d08a131415b92b8a1c2c8b99",
            "header",
        ),
        (
            "Brian Smith and Google ISC notice",
            "src/aead/chacha/fallback.rs",
            "79c47ffa53a1450ef79cc311998998fb2d1785658ded656e85ef58a3621fc68a",
            "header",
        ),
        (
            "Brian Smith and Google ISC notice",
            "src/aead/chacha.rs",
            "bbb8f137622eda811404cd97fb6358c570c583ec21dc4b5406b2d56a23bd7aca",
            "header",
        ),
        (
            "Google and Brian Smith ISC notice",
            "src/aead/gcm/fallback.rs",
            "610754d15ee05925f4f606aba5c7bca88408e7fb2fce3ec9cfb27f7191be5870",
            "header",
        ),
        (
            "Brian Smith and Google ISC notice",
            "src/aead/poly1305/ffi_arm_neon.rs",
            "66b81d9ce04b938b1ca0fe53d6afb97e36afe99f0331db231f4566ed0824b7da",
            "header",
        ),
        (
            "Brian Smith and Google ISC notice",
            "src/aead/poly1305/ffi_fallback.rs",
            "85d27d274e05e48fd9136f0435b9b72957514f1dd724b3640176b8f4464fd155",
            "header",
        ),
        (
            "Trent Clarke ISC notice",
            "src/debug.rs",
            "9d0b5cdf0fbf2f18fa5a42c8916e30f93cf9881e5a1a0b8eb1b676e35dbafbe0",
            "header",
        ),
        (
            "Brian Smith and Simon Sapin ISC notice",
            "src/digest/sha1.rs",
            "9b5218444ee1268be006b853bc5994d8221c52243a3a2ffedbd37c60f6540290",
            "header",
        ),
        (
            "David Judd and Brian Smith ISC notice",
            "src/limb.rs",
            "a7542a645f31c0887f46b99a31b71da5c82e448b5671cfb19fe8264a955b08b8",
            "header",
        ),
    ),
}
FIAT_AUTHORS = """# This is the official list of fiat-crypto authors for copyright purposes.
# This file is distinct from the CONTRIBUTORS files.
# See the latter for an explanation.

# Names should be added to this file as one of
#     Organization's name
#     Individual's name <submission email address>
#     Individual's name <submission email address> <email2> <emailN>
# See CONTRIBUTORS for the meaning of multiple email addresses.

# Please keep the list sorted.

Andres Erbsen <andreser@mit.edu>
Google Inc.
Jade Philipoom <jadep@mit.edu> <jade.philipoom@gmail.com>
Massachusetts Institute of Technology
Zoe Paraskevopoulou <zoe.paraskevopoulou@gmail.com>"""
FIAT_AUTHORS_SHA256 = "ad74c036a6afaedc418fed65f171c877c2f74a4c70938a03930aa11c98a20508"
HEADER = """Requests
Copyright 2019 Kenneth Reitz

Requests Native modification notice: this NOTICE differs from Requests 2.34.2.

Requests Native is an independent, unofficial derivative of Requests, maintained
by Puneet Chandna. It preserves upstream Requests attribution and adds a native
Rust transport with Python compatibility fallbacks. The Python import remains
requests; distribution versions are independent of the Requests compatibility
version 2.34.2. See AUTHORS.rst for the preserved contributor record.

Requests Native is not an official Requests release and is not affiliated with,
sponsored by, or endorsed by the upstream Requests maintainers or the Python
Software Foundation. The independent name and this disclaimer are statements of
provenance, not trademark clearance.

The project source is distributed under Apache-2.0 except where otherwise
identified. Third-party components retain their respective license terms. These
notices identify provenance and applicable third-party material; they do not
replace or modify the accompanying licenses.

THIRD-PARTY AND RUNTIME NOTICE CORPUS

This deterministic corpus covers the transitive normal-dependency graph of the
native extension across supported Cargo target conditions. It is a conservative
cross-target inventory and does not assert that every listed package occurs in
every wheel. Cargo build-only and development-only dependency edges are excluded;
reviewed generated or vendored material is identified in its package section.
Raw Cargo declarations are retained and reviewed obligations record every AND and
the selected option for every OR. Python dependencies are not bundled in the
wheel; they remain separate requirements. Platform system libraries are not bundled; platform linkage
must still be verified for each final wheel.

Rust 1.98.0 (commit 88d9e12ae178fab0fb5cc050a94da85685d449ea) is the pinned
release toolchain. Its complete standard-library notice inventory is packaged as
RUST_RUNTIME_NOTICES.html with SHA-256
68129500b616d5838629e68f55ff3aed5e096dacf60ce9eb41bbe599a563afa6.

Aggregate Python License and License-Expression metadata are intentionally omitted
until the backend can represent source and binary artifacts separately. Canonical
LICENSE, NOTICE, AUTHORS.rst, this runtime supplement, and the wheel SBOM remain
packaged and validated.
"""


def cargo_metadata() -> dict:
    completed = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline", "--format-version=1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


def runtime_packages(metadata: dict) -> list[dict]:
    packages = {package["id"]: package for package in metadata["packages"]}
    roots = [
        package["id"]
        for package in metadata["packages"]
        if package["name"] == WORKSPACE_PACKAGE and package["source"] is None
    ]
    if len(roots) != 1:
        raise ValueError(f"expected one {WORKSPACE_PACKAGE!r} workspace package")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    queue = deque(roots)
    visited: set[str] = set()
    while queue:
        package_id = queue.popleft()
        if package_id in visited:
            continue
        visited.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                queue.append(dependency["pkg"])
    result = [
        packages[package_id] for package_id in visited if packages[package_id]["source"]
    ]
    return sorted(
        result,
        key=lambda package: (
            package["name"].casefold(),
            package["version"],
            package["source"],
        ),
    )


def license_files(
    package: dict, packages_by_key: dict[tuple[str, str], dict]
) -> list[tuple[str, Path]]:
    package_root = Path(package["manifest_path"]).parent
    paths = [
        path
        for path in package_root.iterdir()
        if path.is_file() and path.name.upper().startswith(LICENSE_PREFIXES)
    ]
    declared = package.get("license_file")
    if declared:
        paths.append(package_root / declared)
    unique = sorted(
        {path.resolve() for path in paths}, key=lambda path: path.name.casefold()
    )
    if (
        not unique
        and (package["name"], package["version"]) == ALLOC_STDLIB_LICENSE["package"]
    ):
        sibling_name, sibling_version, filename = ALLOC_STDLIB_LICENSE["source"]
        sibling = packages_by_key[sibling_name, sibling_version]
        path = Path(sibling["manifest_path"]).parent / filename
        verify_hash(path, ALLOC_STDLIB_LICENSE["sha256"])
        return [
            (
                f"{filename} (verified local byte source: {sibling_name} "
                f"{sibling_version}; authority: rust-alloc-no-stdlib commit "
                f"{ALLOC_STDLIB_LICENSE['upstream_commit']} root LICENSE; Git blob "
                f"{ALLOC_STDLIB_LICENSE['upstream_blob']}; SHA-256 "
                f"{ALLOC_STDLIB_LICENSE['sha256']})",
                path.resolve(),
            )
        ]
    if not unique:
        raise ValueError(
            f"{package['name']} {package['version']} has no packaged license/notice file"
        )
    return [(path.name, path) for path in unique]


def verify_hash(path: Path, expected: str) -> bytes:
    content = path.read_bytes()
    actual = hashlib.sha256(content).hexdigest()
    if actual != expected:
        raise ValueError(
            f"unreviewed notice bytes for {path}: expected {expected}, found {actual}"
        )
    return content


def normalized_text(content: bytes) -> str:
    return "\n".join(
        line.rstrip()
        for line in content.decode("utf-8").replace("\r\n", "\n").splitlines()
    ).rstrip()


def source_notice_block(content: bytes, relative_path: str) -> str:
    lines = content.decode("utf-8").replace("\r\n", "\n").splitlines()
    start = next(
        (index for index, line in enumerate(lines[:80]) if "Copyright" in line), None
    )
    if start is None:
        raise ValueError(f"curated source notice has no copyright: {relative_path}")
    first = lines[start].lstrip()
    if first.startswith("/*"):
        end = next(
            (
                index
                for index in range(start, min(len(lines), start + 80))
                if "*/" in lines[index]
            ),
            None,
        )
        if end is None:
            raise ValueError(f"unterminated curated block notice: {relative_path}")
        selected = lines[start : end + 1]
    elif first.startswith("//"):
        end = start
        while end < len(lines) and lines[end].lstrip().startswith("//"):
            end += 1
        selected = lines[start:end]
    elif first.startswith("#"):
        end = start
        while end < len(lines) and lines[end].lstrip().startswith("#"):
            end += 1
        selected = lines[start:end]
    else:
        raise ValueError(f"unsupported curated notice syntax: {relative_path}")
    return "\n".join(line.rstrip() for line in selected).rstrip()


def curated_notices(
    package: dict, packages_by_key: dict[tuple[str, str], dict]
) -> list[tuple[str, str]]:
    key = (package["name"], package["version"])
    package_root = Path(package["manifest_path"]).parent
    result = []
    for label, relative_path, expected_hash, mode in CURATED_FILES.get(key, ()):
        content = verify_hash(package_root / relative_path, expected_hash)
        if mode == "full":
            rendered = normalized_text(content)
        elif mode == "header":
            rendered = source_notice_block(content, relative_path)
        else:
            raise ValueError(f"unsupported curated notice mode: {mode}")
        result.append((f"{label} ({relative_path}; SHA-256 {expected_hash})", rendered))
    if key == ("brotli-decompressor", "5.0.3"):
        sibling = packages_by_key["brotli", "8.0.4"]
        mit_path = Path(sibling["manifest_path"]).parent / "LICENSE.MIT"
        mit_hash = "3d180008e36922a4e8daec11c34c7af264fed5962d07924aea928c38e8663c94"
        result.append(
            (
                "MIT license text for the Google context-table material "
                f"(verified byte source: brotli 8.0.4/LICENSE.MIT; SHA-256 {mit_hash})",
                normalized_text(verify_hash(mit_path, mit_hash)),
            )
        )
    if key == ("ring", "0.17.14"):
        fiat_authors = FIAT_AUTHORS.encode()
        if hashlib.sha256(fiat_authors).hexdigest() != FIAT_AUTHORS_SHA256:
            raise ValueError("unreviewed fiat-crypto AUTHORS bytes")
        result.append(
            (
                "fiat-crypto AUTHORS from ring release commit "
                "2723abbca9e83347d82b056d5b239c6604f786df "
                f"(SHA-256 {FIAT_AUTHORS_SHA256})",
                FIAT_AUTHORS.rstrip(),
            )
        )
    return result


def reviewed_obligations(package: dict, expression: str) -> str:
    key = (package["name"], package["version"])
    if key == ("zstd-sys", "2.0.16+zstd.1.5.7"):
        return "MIT AND BSD-3-Clause"
    try:
        return REVIEWED_OBLIGATIONS[expression]
    except KeyError as error:
        raise ValueError(
            f"unreviewed declared license expression: {expression}"
        ) from error


def runtime_notice_bytes() -> bytes:
    version = subprocess.run(
        ["rustc", "--version", "--verbose"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    if (
        f"release: {RUNTIME_TOOLCHAIN}" not in version
        or f"commit-hash: {RUNTIME_COMMIT}" not in version
    ):
        raise ValueError("unreviewed Rust release toolchain")
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return verify_hash(
        Path(sysroot) / "share/doc/rust/COPYRIGHT-library.html",
        RUNTIME_NOTICE_SHA256,
    )


REQUIRED_NOTICE_MARKERS = (
    "Meta Platforms, Inc. and affiliates",
    "Copyright 2013 Google Inc. All Rights Reserved.",
    "Copyright (c) 2014, Intel Corporation.",
    "Copyright 2016 David Judd.",
    "Copyright 2018 Trent Clarke.",
    "Copyright 2016 Simon Sapin.",
    "Copyright (c) 2019, Google Inc.",
    "Copyright 2015-2020 the fiat-crypto authors",
)


def validate_notice(notice: str) -> None:
    for marker in REQUIRED_NOTICE_MARKERS:
        if marker not in notice:
            raise ValueError(f"required notice marker is missing: {marker}")
    if "--- zstd/COPYING" in notice:
        raise ValueError("unselected Zstandard GPL alternative entered NOTICE")


def render_notice() -> bytes:
    sections = [HEADER.rstrip()]
    packages = runtime_packages(cargo_metadata())
    packages_by_key = {
        (package["name"], package["version"]): package for package in packages
    }
    for package in packages:
        expression = package.get("license") or "NOASSERTION"
        source = package["source"]
        sections.append(
            f"----- BEGIN {package['name']} {package['version']} -----\n"
            f"Declared license: {expression}\n"
            f"Reviewed obligations: {reviewed_obligations(package, expression)}\n"
            f"Cargo source: {source}"
        )
        if (package["name"], package["version"]) == (
            "zstd-sys",
            "2.0.16+zstd.1.5.7",
        ):
            sections.append(
                "Bundled Zstandard C library selection: BSD-3-Clause\n"
                "The wrapper's Rust/generated-bindings terms remain separate; "
                "the GPLv2 alternative in zstd/COPYING is not selected or reproduced."
            )
        for label, path in license_files(package, packages_by_key):
            content = normalized_text(path.read_bytes())
            sections.append(f"--- {label} ---\n{content}")
        for label, content in curated_notices(package, packages_by_key):
            sections.append(f"--- {label} ---\n{content}")
        sections.append(f"----- END {package['name']} {package['version']} -----")
    rendered = "\n\n".join(sections) + "\n"
    validate_notice(rendered)
    return rendered.encode()


def main() -> int:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    arguments = parser.parse_args()
    expected = render_notice()
    expected_runtime = runtime_notice_bytes()
    if arguments.write:
        NOTICE.write_bytes(expected)
        RUNTIME_NOTICE.write_bytes(expected_runtime)
        return 0
    actual = NOTICE.read_bytes()
    if actual != expected:
        print(
            f"NOTICE is stale: expected {len(expected)} bytes, found {len(actual)} bytes",
            file=sys.stderr,
        )
        return 1
    actual_runtime = RUNTIME_NOTICE.read_bytes()
    if actual_runtime != expected_runtime:
        print(
            "RUST_RUNTIME_NOTICES.html is stale: "
            f"expected {len(expected_runtime)} bytes, found {len(actual_runtime)} bytes",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
