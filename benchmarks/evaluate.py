#!/usr/bin/env python3
"""Run paired revision comparisons locally or on the release CI runner."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import importlib.metadata as metadata
import itertools
import json
import math
import os
import random
import shutil
import statistics
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

try:
    from packaging.requirements import Requirement
except ImportError:  # pip is already required by the isolated evaluation venv.
    from pip._vendor.packaging.requirements import Requirement

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from benchmarks import run

ROOT = Path(__file__).resolve().parents[1]
EVALUATOR_FILES = (
    "benchmarks/evaluate.py",
    "benchmarks/run.py",
    "benchmarks/rust-native/Cargo.toml",
    "benchmarks/rust-native/src/main.rs",
)
CASE_FIELDS = ("surface", "mode", "body", "read", "concurrency")


def case_key(row: dict) -> tuple:
    return tuple(row[field] for field in CASE_FIELDS)


def evaluator_digest() -> str:
    digest = hashlib.sha256()
    for name in EVALUATOR_FILES:
        digest.update(name.encode())
        digest.update((ROOT / name).read_bytes())
    return digest.hexdigest()


def check_runtime_dependencies(root: Path) -> None:
    project = run.tomllib.loads((root / "pyproject.toml").read_text())["project"]
    python = Requirement("python" + project["requires-python"])
    if not python.specifier.contains(run.platform.python_version()):
        raise ValueError("evaluation Python does not satisfy the selected revision")
    for value in project["dependencies"]:
        requirement = Requirement(value)
        if requirement.marker and not requirement.marker.evaluate():
            continue
        try:
            installed = metadata.version(requirement.name)
        except metadata.PackageNotFoundError as error:
            raise ValueError(
                f"preinstall compatible evaluation dependency: {requirement}"
            ) from error
        if requirement.url or not requirement.specifier.contains(installed):
            raise ValueError(
                f"preinstall a common compatible dependency: {requirement}"
            )


def confidence_interval(ratios: list[float]) -> tuple[float, float]:
    """Deterministic paired bootstrap of the median cost ratio."""
    rng = random.Random(0)
    medians = sorted(
        statistics.median(rng.choices(ratios, k=len(ratios))) for _ in range(2000)
    )
    return medians[49], medians[1949]


def compare_pairs(
    pairs: list[tuple[dict, dict]], *, budget: float = 0.20, gate: bool = False
) -> dict:
    if not pairs or not math.isfinite(budget) or not 0 < budget < 1:
        raise ValueError("pairs and a finite regression budget in (0, 1) are required")
    reference = pairs[0][0]
    config = reference["config"]
    expected = set(
        itertools.product(
            config["surfaces"],
            ("one-shot", "pooled"),
            ("small", "large"),
            ("buffered", "streaming"),
            config["concurrency_levels"],
        )
    )
    ratios: dict[tuple, list[float]] = {}
    insufficient = (
        len(pairs) < 5
        or config["requests_per_case"] < 100
        or config["warmup_per_worker"] < 4
    )
    insufficient |= set(config["surfaces"]) != set(run.SURFACES)
    insufficient |= 1 not in config["concurrency_levels"] or not any(
        level > 1 for level in config["concurrency_levels"]
    )
    commits = [pairs[0][side]["git"]["rewrite_commit"] for side in (0, 1)]
    for pair in pairs:
        indexed = []
        for side, document in enumerate(pair):
            if document["schema_version"] != 3 or (
                gate and document["git"]["rewrite_dirty"]
            ):
                raise ValueError(
                    "comparison requires schema 3 and immutable source snapshots"
                )
            if document["git"]["rewrite_commit"] != commits[side]:
                raise ValueError("source changed between paired runs")
            for key in ("machine", "toolchains", "config", "evaluator_sha256"):
                if document[key] != reference[key]:
                    raise ValueError(f"incomparable {key}")
            if not document["evaluator_sha256"]:
                raise ValueError("missing evaluator identity")
            if document["git"]["oracle_commit"] != reference["git"]["oracle_commit"]:
                raise ValueError("oracle changed between runs")
            for key in ("versions", "python_soabi"):
                if (
                    document["python_artifact"][key]
                    != reference["python_artifact"][key]
                ):
                    raise ValueError(f"incomparable Python {key}")
            rows = {case_key(row): row for row in document["results"]}
            if len(rows) != len(document["results"]) or set(rows) != expected:
                raise ValueError("missing, duplicate or unexpected benchmark cases")
            for row in rows.values():
                run.validate_result_row(row)
                insufficient |= row.get("rss_scope") != "measured-phase"
                if row["requests"] != config["requests_per_case"]:
                    raise ValueError("case request count differs from configuration")
                metrics = (
                    row["throughput_requests_per_second"],
                    row["latency_ms"]["p95"],
                    row["rss_peak_bytes"],
                    row["cpu_seconds"],
                )
                if any(
                    not isinstance(value, (int, float))
                    or not math.isfinite(value)
                    or value < 0
                    for value in metrics
                ):
                    raise ValueError(
                        "missing, nonfinite or negative performance metric"
                    )
                if min(metrics[:3]) <= 0:
                    raise ValueError("throughput, latency and RSS must be positive")
            indexed.append(rows)
        for key in expected:
            base, candidate = (rows[key] for rows in indexed)
            allocations = (
                "python_peak_alloc_bytes"
                if key[0].startswith("python")
                else "native_total_allocated_bytes"
            )
            for metric in (
                "throughput_requests_per_second",
                "latency_p95_ms",
                "rss_peak_bytes",
                allocations,
            ):
                first, second = (
                    row["latency_ms"]["p95"]
                    if metric == "latency_p95_ms"
                    else row[metric]
                    for row in (base, candidate)
                )
                ratio = (
                    first / second
                    if metric == "throughput_requests_per_second"
                    else second / first
                )
                if not math.isfinite(ratio) or ratio <= 0:
                    raise ValueError("invalid performance ratio")
                ratios.setdefault((*key, metric), []).append(ratio)
        for surface in config["surfaces"]:
            first, second = (
                sum(
                    row["cpu_seconds"] for key, row in rows.items() if key[0] == surface
                )
                for rows in indexed
            )
            # Linux native accounting is tick-granular: aggregate cases, require 50 ms.
            if min(first, second) < 0.05:
                insufficient = True
            else:
                ratios.setdefault(
                    (surface, "all", "all", "all", 0, "cpu_seconds_total"), []
                ).append(second / first)
    metrics = []
    control_unstable = False
    regression = False
    uncertain = False
    for key, values in sorted(ratios.items()):
        low, high = confidence_interval(values)
        median = statistics.median(values)
        status = (
            "regression"
            if low > 1 + budget
            else "inconclusive"
            if high > 1 + budget
            else "passed"
        )
        if key[0] == "python-oracle":
            control_unstable |= low < 1 / (1 + budget) or high > 1 + budget
        else:
            regression |= status == "regression"
            uncertain |= status == "inconclusive"
        metrics.append(
            {
                "case": list(key[:-1]),
                "metric": key[-1],
                "median_cost_ratio": median,
                "bootstrap_95_interval": [low, high],
                "paired_cost_ratios": values,
                "status": status,
            }
        )
    if control_unstable or insufficient:
        status = "inconclusive"
    elif regression:
        status = "regression"
    elif uncertain or (gate and insufficient):
        status = "inconclusive"
    else:
        status = "passed"
    return {
        "status": status,
        "release_gate": gate,
        "pairs": len(pairs),
        "budget_percent": budget * 100,
        "sources": commits,
        "oracle_control_unstable": control_unstable,
        "insufficient_release_evidence": insufficient,
        "metrics": metrics,
    }


def snapshot(revision: str, destination: Path) -> str:
    if revision == "WORKTREE":
        destination.mkdir()
        names = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT)
        names += subprocess.check_output(
            [
                "git",
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                "src",
                "crates",
            ],
            cwd=ROOT,
        )
        digest = hashlib.sha256()
        for name in sorted(set(names.split(b"\0")) - {b""}):
            relative = Path(os.fsdecode(name))
            source, target = ROOT / relative, destination / relative
            if not source.exists():
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            if source.is_symlink():
                target.symlink_to(os.readlink(source))
                content = os.readlink(source).encode()
            else:
                shutil.copy2(source, target)
                content = source.read_bytes()
            digest.update(
                name + b"\0" + source.lstat().st_mode.to_bytes(4, "big") + content
            )
        commit = "WORKTREE:" + digest.hexdigest()
    else:
        commit = subprocess.check_output(
            [
                "git",
                "rev-parse",
                "--verify",
                "--end-of-options",
                f"{revision}^{{commit}}",
            ],
            cwd=ROOT,
            text=True,
        ).strip()
        archive = destination.with_suffix(".tar")
        with archive.open("wb") as output:
            subprocess.run(
                ["git", "archive", commit], cwd=ROOT, stdout=output, check=True
            )
        destination.mkdir()
        with tarfile.open(archive) as source:
            source.extractall(destination, filter="data")
        archive.unlink()
    for name in EVALUATOR_FILES[2:]:
        shutil.copyfile(ROOT / name, destination / name)
    # Seed the independent driver from this revision's release lock, rather than
    # letting a stale historical benchmark lock choose its transport dependencies.
    shutil.copyfile(
        destination / "Cargo.lock", destination / "benchmarks/rust-native/Cargo.lock"
    )
    return commit


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base", required=True, help="Git tag or commit used as baseline"
    )
    parser.add_argument(
        "--candidate",
        default="HEAD",
        help="Git revision or WORKTREE for local uncommitted changes (default HEAD)",
    )
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="One short pair; never release qualification",
    )
    parser.add_argument(
        "--gate",
        action="store_true",
        help="Fail on regressions or inconclusive release evidence",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help="Require all Rust dependencies already cached",
    )
    parser.add_argument("--pairs", type=int, default=5)
    parser.add_argument("--requests", type=int, default=100)
    parser.add_argument("--warmup", type=int, default=4)
    parser.add_argument("--max-regression-percent", type=float, default=20.0)
    parser.add_argument("--case-timeout-seconds", type=float, default=60)
    parser.add_argument(
        "--output",
        type=Path,
        default=ROOT
        / "target/evaluations"
        / dt.datetime.now().strftime("%Y%m%d-%H%M%S"),
    )
    args = parser.parse_args()
    if args.smoke and args.gate:
        parser.error("--smoke cannot qualify a release")
    if args.gate and "WORKTREE" in (args.base, args.candidate):
        parser.error("release qualification requires committed revisions")
    if (
        args.pairs < 1
        or args.requests < 1
        or args.warmup < 0
        or not 0 < args.max_regression_percent < 100
    ):
        parser.error("invalid evaluation sizes or regression budget")
    if sys.prefix == sys.base_prefix:
        parser.error(
            "run with an isolated Python environment containing maturin and Requests dependencies"
        )
    if not (run.ORACLE_ROOT / "src/requests/__init__.py").is_file():
        parser.error("set REQUESTS_ORACLE_ROOT to the frozen psf/requests checkout")
    output = args.output.resolve()
    if output.exists() and any(output.iterdir()):
        parser.error("output directory must be empty (preserve previous evidence)")
    output.mkdir(parents=True, exist_ok=True)
    scratch = ROOT / "target"
    scratch.mkdir(exist_ok=True)
    identity = evaluator_digest()
    pairs = []
    result = {"status": "error"}
    switched_install = False
    try:
        with tempfile.TemporaryDirectory(
            prefix="evaluation-", dir=scratch
        ) as temporary:
            roots = [Path(temporary) / name for name in ("base", "candidate")]
            commits = [
                snapshot(revision, root)
                for revision, root in zip((args.base, args.candidate), roots)
            ]
            build_env = os.environ.copy()
            build_env["RUSTUP_TOOLCHAIN"] = run.tomllib.loads(
                (roots[1] / "rust-toolchain.toml").read_text()
            )["toolchain"]["channel"]
            for root in roots:
                check_runtime_dependencies(root)
                for manifest in (
                    root / "Cargo.toml",
                    root / "benchmarks/rust-native/Cargo.toml",
                ):
                    command = [
                        "cargo",
                        "metadata",
                        "--format-version",
                        "1",
                        "--manifest-path",
                        str(manifest),
                    ]
                    if manifest == root / "Cargo.toml":
                        command.append("--locked")
                    if args.offline:
                        command.append("--offline")
                    with (output / "prepare.log").open("a") as log:
                        subprocess.run(
                            command,
                            cwd=root,
                            env=build_env,
                            check=True,
                            stdout=log,
                            stderr=log,
                        )
            for index in range(1 if args.smoke else args.pairs):
                reports = [None, None]
                for side in (0, 1) if index % 2 == 0 else (1, 0):
                    label = "base" if side == 0 else "candidate"
                    path = output / f"pair-{index + 1:02}-{label}.json"
                    print(f"Pair {index + 1}: {label} {commits[side][:12]}", flush=True)
                    env = build_env.copy()
                    env.update(
                        REQUESTS_BENCHMARK_ROOT=str(roots[side]),
                        REQUESTS_BENCHMARK_COMMIT=commits[side],
                        REQUESTS_ORACLE_ROOT=str(run.ORACLE_ROOT),
                    )
                    command = [
                        sys.executable,
                        str(ROOT / "benchmarks/run.py"),
                        "--profile",
                        "smoke" if args.smoke else "default",
                        "--warmup",
                        str(args.warmup),
                        "--case-timeout-seconds",
                        str(args.case_timeout_seconds),
                        "--output",
                        str(path),
                    ]
                    if not args.smoke:
                        command.extend(("--requests", str(args.requests)))
                    with path.with_suffix(".log").open("w") as log:
                        switched_install = True
                        subprocess.run(
                            command,
                            cwd=roots[side],
                            env=env,
                            stdout=log,
                            stderr=log,
                            check=True,
                        )
                    if evaluator_digest() != identity:
                        raise ValueError(
                            "evaluator changed during measurement; rerun with unchanged evaluator files"
                        )
                    reports[side] = json.loads(path.read_text())
                    reports[side]["git"]["rewrite_dirty"] = commits[side].startswith(
                        "WORKTREE:"
                    )
                    reports[side]["evaluator_sha256"] = identity
                    reports[side]["transport_lock_sha256"] = run.sha256(
                        roots[side] / "Cargo.lock"
                    )
                    reports[side]["driver_lock_sha256"] = run.sha256(
                        roots[side] / "benchmarks/rust-native/Cargo.lock"
                    )
                    path.write_text(
                        json.dumps(reports[side], indent=2, sort_keys=True) + "\n"
                    )
                pairs.append(tuple(reports))
            result = compare_pairs(
                pairs, budget=args.max_regression_percent / 100, gate=args.gate
            )
            result["smoke_only"] = args.smoke
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        result = {
            "status": "error",
            "error": type(error).__name__,
            "detail": "See local prepare/pair logs; measurement did not qualify.",
        }
        print(f"Evaluation failed: {error}", file=sys.stderr)
    finally:
        # maturin develop switches this environment's editable install. Restore
        # the caller's checkout when measurement ends.
        if switched_install:
            try:
                env = os.environ.copy()
                env["VIRTUAL_ENV"] = sys.prefix
                env["PIP_NO_DEPS"] = "1"
                command = [
                    str(run.resolve_maturin(Path(sys.prefix))),
                    "develop",
                    "--release",
                    "--locked",
                    "--offline",
                ]
                with (output / "restore.log").open("w") as log:
                    subprocess.run(
                        command, cwd=ROOT, env=env, check=True, stdout=log, stderr=log
                    )
            except (OSError, subprocess.SubprocessError) as error:
                result = {
                    "status": "error",
                    "detail": f"Editable environment restore failed: {type(error).__name__}; rerun maturin develop in your checkout.",
                }
        (output / "comparison.json").write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n"
        )
    print(f"{result['status']}: {output / 'comparison.json'}")
    return (
        0
        if result["status"] == "passed"
        or (not args.gate and result["status"] == "inconclusive")
        else 1
    )


if __name__ == "__main__":
    raise SystemExit(main())
