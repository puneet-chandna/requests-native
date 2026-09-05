#!/usr/bin/env python3
"""Generate the deterministic release-wheel CycloneDX dependency graph."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from collections import deque
from pathlib import Path
from urllib.parse import quote

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10
    tomllib = None

ROOT = Path(__file__).resolve().parents[1]
ROOT_PACKAGE = "requests-native-python"


def cargo_metadata(
    *,
    root: Path = ROOT,
    environment: dict[str, str] | None = None,
    offline: bool = False,
) -> dict:
    command = ["cargo", "metadata", "--locked", "--format-version=1"]
    if offline:
        command.append("--offline")
    completed = subprocess.run(
        command,
        cwd=root,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


def _checksum_mapping(
    packages: list[dict],
) -> dict[tuple[str, str, str | None], str | None]:
    checksums: dict[tuple[str, str, str | None], str | None] = {}
    for package in packages:
        if not isinstance(package.get("name"), str) or not isinstance(
            package.get("version"), str
        ):
            raise ValueError("Cargo.lock package lacks a string name or version")
        if package.get("source") is not None and not isinstance(package["source"], str):
            raise ValueError("Cargo.lock package source must be a string")
        if package.get("checksum") is not None and not isinstance(
            package["checksum"], str
        ):
            raise ValueError("Cargo.lock package checksum must be a string")
        key = (package["name"], package["version"], package.get("source"))
        if key in checksums:
            raise ValueError(
                f"Cargo.lock contains a duplicate package identity: {key!r}"
            )
        checksums[key] = package.get("checksum")
    if not checksums:
        raise ValueError("Cargo.lock contains no package records")
    return checksums


def parse_generated_cargo_lock(
    content: str,
) -> dict[tuple[str, str, str | None], str | None]:
    packages = []
    package: dict[str, str] | None = None
    field_pattern = re.compile(r"^\s*(name|version|source|checksum)\s*=\s*(.+?)\s*$")

    for line_number, line in enumerate(content.splitlines(), 1):
        stripped = line.strip()
        if stripped == "[[package]]":
            if package is not None:
                packages.append(package)
            package = {}
            continue
        if stripped.startswith("[[package"):
            raise ValueError(
                f"malformed Cargo.lock package header at line {line_number}"
            )
        if package is None:
            continue
        match = field_pattern.fullmatch(line)
        if match is None:
            if re.match(r"^\s*(?:name|version|source|checksum)\b", line):
                raise ValueError(
                    f"malformed Cargo.lock package field at line {line_number}"
                )
            continue
        field, encoded = match.groups()
        if field in package:
            raise ValueError(
                f"duplicate Cargo.lock package field {field!r} at line {line_number}"
            )
        try:
            value = json.loads(encoded)
        except json.JSONDecodeError as error:
            raise ValueError(
                f"invalid Cargo.lock quoted value at line {line_number}"
            ) from error
        if not isinstance(value, str):
            raise ValueError(
                f"Cargo.lock package field {field!r} is not a quoted string"
            )
        package[field] = value

    if package is not None:
        packages.append(package)
    return _checksum_mapping(packages)


def cargo_lock_checksums(
    *, root: Path = ROOT
) -> dict[tuple[str, str, str | None], str | None]:
    content = (root / "Cargo.lock").read_text(encoding="utf-8")
    if tomllib is None:
        return parse_generated_cargo_lock(content)
    lock = tomllib.loads(content)
    packages = lock.get("package")
    if not isinstance(packages, list):
        raise ValueError("Cargo.lock contains no package records")
    return _checksum_mapping(packages)


def release_graph(
    metadata: dict,
) -> tuple[str, set[str], set[str], dict[str, set[str]]]:
    packages = {package["id"]: package for package in metadata["packages"]}
    roots = [
        package["id"]
        for package in metadata["packages"]
        if package["name"] == ROOT_PACKAGE and package["source"] is None
    ]
    if len(roots) != 1:
        raise ValueError(f"expected one {ROOT_PACKAGE!r} workspace package")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root_id = roots[0]

    def closure(
        kinds: set[str | None],
    ) -> tuple[set[str], dict[str, set[str]]]:
        queue = deque([root_id])
        visited: set[str] = set()
        edges: dict[str, set[str]] = {}
        while queue:
            package_id = queue.popleft()
            if package_id in visited:
                continue
            if package_id not in packages or package_id not in nodes:
                raise ValueError(
                    f"Cargo graph references unknown package: {package_id!r}"
                )
            visited.add(package_id)
            dependencies = {
                dependency["pkg"]
                for dependency in nodes[package_id]["deps"]
                if any(kind["kind"] in kinds for kind in dependency["dep_kinds"])
            }
            edges[package_id] = dependencies
            queue.extend(sorted(dependencies))
        return visited, edges

    required, _ = closure({None})
    included, edges = closure({None, "build"})
    return root_id, included, required, edges


def package_purl(package: dict) -> str:
    name = quote(package["name"], safe="")
    version = quote(package["version"], safe="")
    return f"pkg:cargo/{name}@{version}"


def package_ref(package: dict) -> str:
    if package.get("source") is not None:
        return package["id"]
    return package_purl(package)


def normalize_license_expression(expression: str | None) -> str:
    if not expression:
        raise ValueError("release Cargo component lacks a license expression")
    return re.sub(r"\s*/\s*", " OR ", expression)


def external_references(package: dict) -> list[dict[str, str]]:
    references = []
    for reference_type, field in (
        ("documentation", "documentation"),
        ("website", "homepage"),
        ("vcs", "repository"),
    ):
        if package.get(field):
            references.append({"type": reference_type, "url": package[field]})
    return references


def component(package: dict, *, scope: str, checksum: str | None) -> dict[str, object]:
    result: dict[str, object] = {
        "bom-ref": package_ref(package),
        "description": (package.get("description") or "").replace("\n", " "),
        "licenses": [
            {"expression": normalize_license_expression(package.get("license"))}
        ],
        "name": package["name"],
        "purl": package_purl(package),
        "scope": scope,
        "type": "library",
        "version": package["version"],
    }
    if package.get("authors"):
        result["author"] = ", ".join(package["authors"])
    if checksum is not None:
        result["hashes"] = [{"alg": "SHA-256", "content": checksum}]
    references = external_references(package)
    if references:
        result["externalReferences"] = references
    return result


def binding_targets(package: dict, root_component: dict[str, object]) -> list[dict]:
    manifest_root = Path(package["manifest_path"]).parent
    targets = [
        target
        for target in package["targets"]
        if "custom-build" not in target.get("kind", [])
    ]
    rendered = []
    for index, target in enumerate(sorted(targets, key=lambda item: item["name"])):
        try:
            source = Path(target["src_path"]).relative_to(manifest_root).as_posix()
        except ValueError as error:
            raise ValueError("Cargo target source escapes its package root") from error
        rendered.append(
            {
                "bom-ref": f"{root_component['bom-ref']} bin-target-{index}",
                "name": target["name"],
                "purl": f"{root_component['purl']}#{quote(source, safe='/')}",
                "type": "library",
                "version": package["version"],
            }
        )
    return rendered


def package_checksum(
    package: dict, checksums: dict[tuple[str, str, str | None], str | None]
) -> str | None:
    key = (package["name"], package["version"], package.get("source"))
    if key not in checksums:
        raise ValueError(f"Cargo.lock is missing resolved package: {key!r}")
    return checksums[key]


def render_sbom(
    metadata: dict,
    *,
    checksums: dict[tuple[str, str, str | None], str | None] | None = None,
) -> bytes:
    if checksums is None:
        checksums = cargo_lock_checksums()
    packages = {package["id"]: package for package in metadata["packages"]}
    root_id, visited, required, edges = release_graph(metadata)
    references = {
        package_id: package_ref(packages[package_id]) for package_id in visited
    }
    purls = {package_id: package_purl(packages[package_id]) for package_id in visited}
    if len(set(references.values())) != len(references):
        raise ValueError("Cargo packages do not have unique normalized references")
    if len(set(purls.values())) != len(purls):
        raise ValueError("Cargo packages do not have unique Cargo purls")

    rendered = {
        package_id: component(
            packages[package_id],
            scope="required" if package_id in required else "excluded",
            checksum=package_checksum(packages[package_id], checksums),
        )
        for package_id in visited
    }
    root_component = rendered[root_id]
    root_component["components"] = binding_targets(packages[root_id], root_component)

    dependencies = []
    for package_id in sorted(visited, key=lambda item: references[item]):
        dependency: dict[str, object] = {"ref": references[package_id]}
        depends_on = sorted(
            references[item] for item in edges[package_id] if item in visited
        )
        if depends_on:
            dependency["dependsOn"] = depends_on
        dependencies.append(dependency)

    document = {
        "bomFormat": "CycloneDX",
        "components": [
            rendered[package_id]
            for package_id in sorted(
                visited - {root_id}, key=lambda item: references[item]
            )
        ],
        "dependencies": dependencies,
        "metadata": {"component": root_component},
        "specVersion": "1.5",
        "version": 1,
    }
    validate_sbom(document)
    return (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()


def validate_sbom(document: dict) -> None:
    if document.get("bomFormat") != "CycloneDX":
        raise ValueError("release SBOM must use CycloneDX")
    if document.get("specVersion") != "1.5" or document.get("version") != 1:
        raise ValueError("release SBOM must use the reviewed CycloneDX 1.5 shape")
    if "serialNumber" in document or "timestamp" in document.get("metadata", {}):
        raise ValueError("release SBOM contains nondeterministic identity or time data")
    components = [
        document.get("metadata", {}).get("component"),
        *document.get("components", []),
    ]
    if not components[0] or any(not isinstance(item, dict) for item in components):
        raise ValueError("release SBOM component inventory is incomplete")
    references = [item.get("bom-ref") for item in components]
    if any(not isinstance(reference, str) or not reference for reference in references):
        raise ValueError("release SBOM references must be non-empty strings")
    if len(set(references)) != len(references):
        raise ValueError("release SBOM component references are not unique")
    if any(
        not isinstance(item.get("purl"), str)
        or not item["purl"].startswith("pkg:cargo/")
        for item in components
    ):
        raise ValueError("release SBOM components must use Cargo purls")
    if len({item["purl"] for item in components}) != len(components):
        raise ValueError("release SBOM component purls are not unique")
    for item in components:
        licenses = item.get("licenses")
        if (
            not isinstance(licenses, list)
            or len(licenses) != 1
            or set(licenses[0]) != {"expression"}
            or not isinstance(licenses[0]["expression"], str)
        ):
            raise ValueError("release SBOM component license expression is invalid")
        if item.get("scope") not in {"required", "excluded"}:
            raise ValueError("release SBOM component scope is invalid")
    dependencies = document.get("dependencies")
    if not isinstance(dependencies, list):
        raise ValueError("release SBOM dependency graph is missing")
    dependency_refs = [item.get("ref") for item in dependencies]
    if len(set(dependency_refs)) != len(dependency_refs) or set(dependency_refs) != set(
        references
    ):
        raise ValueError("release SBOM dependency nodes do not close over components")
    for item in dependencies:
        depends_on = item.get("dependsOn", [])
        if not isinstance(depends_on, list) or len(depends_on) != len(set(depends_on)):
            raise ValueError("release SBOM dependency edges are not unique")
        if not set(depends_on) <= set(references):
            raise ValueError("release SBOM dependency edge is not closed")
    encoded = json.dumps(document, sort_keys=True)
    if "path+file:" in encoded or "file://" in encoded:
        raise ValueError("release SBOM contains a file URI")
    strings = []
    queue = deque([document])
    while queue:
        value = queue.popleft()
        if isinstance(value, dict):
            queue.extend(value.values())
        elif isinstance(value, list):
            queue.extend(value)
        elif isinstance(value, str):
            strings.append(value)
    if any(value.startswith("/") for value in strings) or any(
        re.match(r"^(?:[A-Za-z]:[\\/]|\\\\|//\?/)", value) for value in strings
    ):
        raise ValueError("release SBOM contains an absolute filesystem path")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--offline", action="store_true")
    arguments = parser.parse_args()
    output = arguments.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(render_sbom(cargo_metadata(offline=arguments.offline)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
