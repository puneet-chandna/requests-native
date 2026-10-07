#!/usr/bin/env python3
"""Reproducible loopback benchmarks for all Requests rewrite surfaces."""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import datetime as dt
import hashlib
import http.client
import json
import math
import os
import pathlib
import platform
import re
import shlex
import shutil
import socket
import subprocess
import sys
import threading
import time
import tracemalloc
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

try:
    import tomllib
except ImportError:
    import tomli as tomllib

try:
    import resource
except ImportError:  # pragma: no cover - Windows records RSS as unavailable.
    resource = None  # type: ignore[assignment]


ROOT = pathlib.Path(
    os.environ.get(
        "REQUESTS_BENCHMARK_ROOT", pathlib.Path(__file__).resolve().parents[1]
    )
).resolve()
ORACLE_ROOT = pathlib.Path(
    os.environ.get("REQUESTS_ORACLE_ROOT", ROOT.parent / "requests")
).resolve()
NATIVE_BINARY = ROOT / "target" / "release" / "requests-benchmark-native"
NATIVE_MANIFEST = ROOT / "benchmarks" / "rust-native" / "Cargo.toml"
SCHEMA_VERSION = 3
SURFACES = ("python-oracle", "python-rust", "rust-async", "rust-blocking")
_PERSONAL_HOME = re.compile(
    r"(?:/(?:home|Users)/[^/\s]+|[A-Za-z]:[\\/]Users[\\/][^\\/\s]+)"
)


def sanitize_public_report(
    value: Any,
    *,
    repository: pathlib.Path = ROOT,
    oracle: pathlib.Path = ORACLE_ROOT,
) -> Any:
    replacements = (
        (str(repository.resolve()), "{repository}"),
        (str(oracle.resolve()), "{oracle}"),
        (str(pathlib.Path(__file__).resolve().parents[1]), "{evaluator}"),
    )

    def sanitize(item: Any) -> Any:
        if isinstance(item, str):
            for source, replacement in replacements:
                item = item.replace(source, replacement)
            if _PERSONAL_HOME.search(item):
                raise ValueError("benchmark report contains a personal home path")
            return item
        if isinstance(item, list):
            return [sanitize(member) for member in item]
        if isinstance(item, dict):
            return {key: sanitize(member) for key, member in item.items()}
        return item

    return sanitize(value)


def percentile(values: list[float], quantile: float) -> float:
    if not values:
        raise ValueError("percentile needs at least one value")
    if not 0.0 < quantile <= 1.0:
        raise ValueError("quantile must be in (0, 1]")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]


def split_work(requests: int, concurrency: int) -> list[int]:
    if requests < 1 or concurrency < 1:
        raise ValueError("requests and concurrency must be positive")
    workers = min(requests, concurrency)
    quotient, remainder = divmod(requests, workers)
    return [quotient + (index < remainder) for index in range(workers)]


def value_or_default(value: int | None, default: int) -> int:
    return default if value is None else value


def resolve_surface_requests(default: int, overrides: list[str]) -> dict[str, int]:
    if type(default) is not int or default < 1:
        raise ValueError("request counts must be positive integers")
    counts = dict.fromkeys(SURFACES, default)
    seen = set()
    for override in overrides:
        surface, separator, value = override.partition("=")
        if surface not in counts:
            raise ValueError(f"unknown surface: {surface}")
        if surface in seen:
            raise ValueError(f"duplicate surface: {surface}")
        if (
            not separator
            or not value.isascii()
            or not value.isdecimal()
            or int(value) < 1
        ):
            raise ValueError("surface requests require SURFACE=POSITIVE_INTEGER")
        counts[surface] = int(value)
        seen.add(surface)
    return counts


def configured_surface_requests(config: dict) -> dict[str, int]:
    counts = config.get("requests_per_surface")
    if counts is None and "requests_per_surface" not in config:
        return resolve_surface_requests(config["requests_per_case"], [])
    if (
        not isinstance(counts, dict)
        or set(counts) != set(SURFACES)
        or any(type(value) is not int or value < 1 for value in counts.values())
    ):
        raise ValueError(
            "requests_per_surface must contain a positive integer for every surface"
        )
    return counts


def resolve_workload(arguments: argparse.Namespace) -> tuple[int, int, int, int]:
    profile = {
        "smoke": (2, 64, 32 * 1024, 2),
        "default": (12, 128, 256 * 1024, 4),
    }[arguments.profile]
    requests = value_or_default(arguments.requests, profile[0])
    small_size = value_or_default(arguments.small_bytes, profile[1])
    large_size = value_or_default(arguments.large_bytes, profile[2])
    maximum_concurrency = value_or_default(arguments.concurrency, profile[3])
    if (
        min(requests, small_size, large_size, maximum_concurrency, arguments.chunk_size)
        < 1
        or arguments.case_timeout_seconds <= 0
        or arguments.warmup < 0
    ):
        raise ValueError("all workload sizes and deadlines must be positive")
    return requests, small_size, large_size, maximum_concurrency


def find_release_library(
    release_directory: pathlib.Path, system: str = sys.platform
) -> pathlib.Path:
    patterns = {
        "linux": "*requests_rust.so",
        "darwin": "*requests_rust.dylib",
        "win32": "*requests_rust.dll",
    }
    try:
        pattern = patterns[system]
    except KeyError as error:
        raise RuntimeError(f"unsupported extension-build platform: {system}") from error
    candidates = sorted(
        path.resolve() for path in release_directory.glob(pattern) if path.is_file()
    )
    if len(candidates) != 1:
        raise RuntimeError(
            f"expected exactly one release extension matching {pattern}, "
            f"found {len(candidates)} in {release_directory}"
        )
    return candidates[0]


def resolve_maturin(
    environment: pathlib.Path,
    system: str = sys.platform,
    *,
    which=shutil.which,
) -> pathlib.Path:
    scripts = environment / ("Scripts" if system == "win32" else "bin")
    names = ("maturin.exe", "maturin") if system == "win32" else ("maturin",)
    for search_path in (str(scripts), None):
        for name in names:
            executable = which(name, path=search_path)
            if executable is not None:
                return pathlib.Path(executable).resolve()
    raise RuntimeError(f"maturin was not found for Python environment {environment}")


def body_checksum(body: bytes) -> int:
    return sum(body)


def enable_tcp_nodelay(connection) -> None:
    connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    if connection.getsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY) != 1:
        raise RuntimeError("loopback fixture could not enable TCP_NODELAY")


def normalize_peak_rss(value: int, system: str = sys.platform) -> int:
    return value if system == "darwin" else value * 1024


def reset_peak_rss() -> bool:
    if sys.platform != "linux":
        return False
    try:
        pathlib.Path("/proc/self/clear_refs").write_text("5\n")
    except OSError:
        return False
    return True


def peak_rss_bytes() -> int | None:
    if sys.platform == "linux":
        for line in pathlib.Path("/proc/self/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1]) * 1024
        return None
    return (
        normalize_peak_rss(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
        if resource is not None
        else None
    )


def valid_latencies(values: Any, count: int) -> bool:
    return (
        isinstance(values, list)
        and len(values) == count
        and all(type(value) is int and value > 0 for value in values)
    )


def validate_allocations(worker: dict[str, Any], surface: str) -> None:
    python_peak = worker["python_peak_alloc_bytes"]
    native_total = worker["native_total_allocated_bytes"]
    if surface.startswith("python-"):
        if not isinstance(python_peak, int) or python_peak <= 0:
            raise ValueError("Python allocation replay did not return a positive peak")
        if native_total is not None:
            raise ValueError("Python allocation replay returned a native allocation")
    elif not isinstance(native_total, int) or native_total <= 0:
        raise ValueError("native allocation replay did not return a positive total")


def validate_result_row(row: dict[str, Any]) -> None:
    required = {
        "surface",
        "mode",
        "body",
        "read",
        "concurrency",
        "requests",
        "allocation_replay_concurrency",
        "latency_ns_samples",
        "latency_ms",
        "throughput_requests_per_second",
        "python_peak_alloc_bytes",
        "native_total_allocated_bytes",
        "body_bytes_total",
        "checksum_total",
        "application_chunks",
        "connections_opened",
        "requests_reusing_connections",
    }
    missing = required - row.keys()
    if missing:
        raise ValueError(f"result row missing keys: {sorted(missing)}")
    if type(row["requests"]) is not int or row["requests"] < 1:
        raise ValueError("result row requests must be a positive integer")
    if not valid_latencies(row["latency_ns_samples"], row["requests"]):
        raise ValueError("result row has the wrong latency sample count")
    validate_allocations(row, row["surface"])


def validate_worker_result(
    worker: dict[str, Any],
    *,
    requests: int,
    expected_bytes: int,
    expected_checksum: int,
    expected_application_chunks: int,
    require_native: bool,
) -> None:
    required = {
        "latencies_ns",
        "body_bytes",
        "checksum",
        "connection_ids",
        "elapsed_ns",
        "cpu_seconds",
        "rss_peak_bytes",
        "python_peak_alloc_bytes",
        "native_responses",
        "application_chunks",
        "native_total_allocated_bytes",
        "implementation",
    }
    missing = required - worker.keys()
    if missing:
        raise ValueError(f"worker result missing keys: {sorted(missing)}")
    latencies = worker["latencies_ns"]
    if not valid_latencies(latencies, requests):
        raise ValueError("worker returned an invalid latency sample count")
    if worker["body_bytes"] != requests * expected_bytes:
        raise ValueError("worker body bytes differ from the fixture contract")
    if worker["checksum"] != requests * expected_checksum:
        raise ValueError("worker checksum differs from the fixture contract")
    if worker["application_chunks"] != expected_application_chunks:
        raise ValueError("worker application chunks differ from the read contract")
    if require_native and worker["native_responses"] != requests:
        raise ValueError("Rust-backed Python did not route every response natively")
    if (
        type(worker["elapsed_ns"]) is not int
        or worker["elapsed_ns"] <= 0
        or (
            worker["cpu_seconds"] is not None
            and (not math.isfinite(worker["cpu_seconds"]) or worker["cpu_seconds"] < 0)
        )
    ):
        raise ValueError("worker returned invalid timing data")
    if not worker["connection_ids"]:
        raise ValueError("worker did not observe a fixture connection")


def validate_warmup(
    worker: dict[str, Any],
    count: int,
    body: bytes,
    chunk_size: int,
    read: str,
    surface: str,
) -> None:
    if (
        not valid_latencies(worker["warmup_latencies_ns"], count)
        or worker["warmup_body_bytes"] != count * len(body)
        or worker["warmup_checksum"] != count * body_checksum(body)
        or worker["warmup_application_chunks"]
        != (0 if read == "buffered" else count * math.ceil(len(body) / chunk_size))
        or worker["warmup_native_responses"]
        != (count if surface != "python-oracle" else 0)
        or (count > 0 and not worker["warmup_connection_ids"])
    ):
        raise ValueError("warm-up violates the fixture/native routing contract")


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, small_body: bytes, large_body: bytes):
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.bodies = {"/small": small_body, "/large": large_body}
        self._lock = threading.Lock()
        self._connection_ids: dict[int, int] = {}
        self.accepted = 0
        self.requests = 0

    def get_request(self):
        connection, address = super().get_request()
        enable_tcp_nodelay(connection)
        with self._lock:
            self.accepted += 1
            self._connection_ids[id(connection)] = self.accepted
        return connection, address

    def observe_request(self, connection: object) -> int:
        with self._lock:
            self.requests += 1
            return self._connection_ids[id(connection)]

    def snapshot(self) -> tuple[int, int]:
        with self._lock:
            return self.requests, self.accepted


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:  # noqa: N802
        body = self.server.bodies.get(self.path)  # type: ignore[attr-defined]
        if body is None:
            self.send_error(404)
            return
        connection_id = self.server.observe_request(self.connection)  # type: ignore[attr-defined]
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("X-Benchmark-Connection", str(connection_id))
        self.end_headers()
        try:
            for start in range(0, len(body), 16 * 1024):
                self.wfile.write(body[start : start + 16 * 1024])
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, format: str, *args: object) -> None:
        return


@contextlib.contextmanager
def fixture_server(small_body: bytes, large_body: bytes):
    server = FixtureServer(small_body, large_body)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def python_worker(arguments: argparse.Namespace) -> int:
    sys.path.insert(
        0, str((ORACLE_ROOT if arguments.surface == "python-oracle" else ROOT) / "src")
    )
    import requests  # noqa: PLC0415

    implementation = pathlib.Path(requests.__file__).resolve()
    expected_root = ORACLE_ROOT if arguments.surface == "python-oracle" else ROOT
    if not implementation.is_relative_to(expected_root):
        raise RuntimeError(
            f"{arguments.surface} imported {implementation}, outside {expected_root}"
        )

    workers = min(arguments.concurrency, arguments.requests)
    ready = threading.Barrier(workers + 1)
    start = threading.Barrier(workers + 1)

    def consume(session, url: str) -> tuple[int, int, int, int, int, int]:
        start = time.perf_counter_ns()
        with session.get(url, stream=arguments.read == "streaming") as response:
            response.raise_for_status()
            connection_id = int(response.headers["X-Benchmark-Connection"])
            is_native = type(response.raw).__module__ == "requests._requests_rust"
            if arguments.read == "buffered":
                body = response.content
                size = len(body)
                checksum = body_checksum(body)
                application_chunks = 0
            else:
                size = 0
                checksum = 0
                application_chunks = 0
                for chunk in response.iter_content(arguments.chunk_size):
                    size += len(chunk)
                    checksum += body_checksum(chunk)
                    application_chunks += 1
        latency = time.perf_counter_ns() - start
        return (
            latency,
            size,
            checksum,
            connection_id,
            application_chunks,
            int(is_native),
        )

    def run_client(count: int) -> list[tuple[int, int, int, int, int, int]]:
        pooled = None
        if arguments.mode == "pooled":
            pooled = requests.Session()
            pooled.trust_env = False
        output = []
        warmed = []
        try:
            for index in range(arguments.warmup + count):
                if index == arguments.warmup:
                    ready.wait()
                    start.wait()
                if pooled is None:
                    with requests.Session() as one_shot:
                        one_shot.trust_env = False
                        observation = consume(one_shot, arguments.url)
                else:
                    observation = consume(pooled, arguments.url)
                (warmed if index < arguments.warmup else output).append(observation)
        except BaseException:
            ready.abort()
            start.abort()
            raise
        finally:
            if pooled is not None:
                pooled.close()
        return output, warmed

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [
            executor.submit(run_client, count)
            for count in split_work(arguments.requests, arguments.concurrency)
        ]
        ready.wait()
        rss_reset = reset_peak_rss()
        if arguments.measure_allocations:
            tracemalloc.start()
        cpu_start = time.process_time()
        wall_start = time.perf_counter_ns()
        start.wait()
        nested = [future.result() for future in futures]
        elapsed_ns = time.perf_counter_ns() - wall_start
        cpu_seconds = time.process_time() - cpu_start
        rss_peak_bytes = peak_rss_bytes()
        allocation_peak = None
        if arguments.measure_allocations:
            _, allocation_peak = tracemalloc.get_traced_memory()
            tracemalloc.stop()

    observations = [item for measured, _ in nested for item in measured]
    warmups = [item for _, warmed in nested for item in warmed]
    result = {
        "latencies_ns": [item[0] for item in observations],
        "body_bytes": sum(item[1] for item in observations),
        "checksum": sum(item[2] for item in observations),
        "connection_ids": sorted({item[3] for item in observations}),
        "elapsed_ns": elapsed_ns,
        "cpu_seconds": cpu_seconds,
        "rss_peak_bytes": rss_peak_bytes,
        "rss_scope": "measured-phase" if rss_reset else "process-lifetime",
        "python_peak_alloc_bytes": allocation_peak,
        "native_responses": sum(item[5] for item in observations),
        "application_chunks": sum(item[4] for item in observations),
        "native_total_allocated_bytes": None,
        "implementation": str(implementation),
    }
    result.update(
        {
            "warmup_latencies_ns": [item[0] for item in warmups],
            "warmup_body_bytes": sum(item[1] for item in warmups),
            "warmup_checksum": sum(item[2] for item in warmups),
            "warmup_connection_ids": sorted({item[3] for item in warmups}),
            "warmup_application_chunks": sum(item[4] for item in warmups),
            "warmup_native_responses": sum(item[5] for item in warmups),
        }
    )
    print(json.dumps(result, sort_keys=True))
    return 0


def build_native(command_log: list[str]) -> None:
    command = [
        "cargo",
        "build",
        "--manifest-path",
        str(NATIVE_MANIFEST),
        "--target-dir",
        str(ROOT / "target"),
        "--release",
        "--locked",
        "--offline",
    ]
    command_log.append(shlex.join(command))
    subprocess.run(command, cwd=ROOT, check=True)
    if not NATIVE_BINARY.is_file():
        raise RuntimeError(f"native benchmark binary missing: {NATIVE_BINARY}")


def build_python_extension(
    command_log: list[str],
) -> tuple[pathlib.Path, dict[str, Any]]:
    environment = pathlib.Path(sys.prefix).resolve()
    if environment == pathlib.Path(sys.base_prefix).resolve():
        raise RuntimeError("benchmark must run in an isolated Python environment")
    temporary = ROOT / "target" / "benchmark-tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    environment_overrides = {
        "PIP_NO_DEPS": "1",
        "TEMP": str(temporary),
        "TMP": str(temporary),
        "TMPDIR": str(temporary),
        "UV_CACHE_DIR": str(ROOT / "target" / "benchmark-uv-cache"),
    }
    command = [
        str(resolve_maturin(environment)),
        "develop",
        "--release",
        "--locked",
        "--offline",
    ]
    command_log.append(
        shlex.join(
            [
                *(f"{name}={value}" for name, value in environment_overrides.items()),
                *command,
            ]
        )
    )
    process_environment = os.environ.copy()
    process_environment.update(environment_overrides)
    process_environment["VIRTUAL_ENV"] = str(environment)
    subprocess.run(command, cwd=ROOT, check=True, env=process_environment)
    release_build = find_release_library(ROOT / "target" / "release")
    return release_build, {
        "command": command,
        "environment": environment_overrides,
    }


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def python_artifact_provenance(
    command_log: list[str],
    timeout_seconds: float,
    release_build: pathlib.Path,
    build_record: dict[str, Any],
) -> dict[str, Any]:
    script = """
import importlib.metadata as metadata
import json
import pathlib
import sys
import sysconfig
import os
sys.path.insert(0, str(pathlib.Path(os.environ.get("REQUESTS_BENCHMARK_ROOT", ".")) / "src"))
import requests
import requests._requests_rust as extension

names = ("urllib3", "certifi", "idna", "charset-normalizer")
print(json.dumps({
    "python_executable": sys.executable,
    "python_prefix": sys.prefix,
    "python_base_prefix": sys.base_prefix,
    "python_version": sys.version,
    "python_cache_tag": sys.implementation.cache_tag,
    "python_soabi": sysconfig.get_config_var("SOABI"),
    "extension_suffix": sysconfig.get_config_var("EXT_SUFFIX"),
    "requests_module": requests.__file__,
    "extension_module": extension.__file__,
    "extension_backend": extension.backend_name(),
    "distribution_name": "requests-native",
    "distribution_version": metadata.version("requests-native"),
    "compatibility_import": "requests",
    "compatibility_version": requests.__version__,
    "versions": {name: metadata.version(name) for name in names},
}, sort_keys=True))
"""
    command = [sys.executable, "-c", script]
    command_log.append(shlex.join(command))
    provenance = run_json_command(command, cwd=ROOT, timeout_seconds=timeout_seconds)
    active_environment = pathlib.Path(sys.prefix).resolve()
    if pathlib.Path(provenance["python_prefix"]).resolve() != active_environment:
        raise RuntimeError("provenance probe left the active Python environment")
    extension = pathlib.Path(provenance["extension_module"]).resolve()
    expected_directory = ROOT / "src" / "requests"
    expected_suffix = provenance["extension_suffix"]
    if (
        extension.parent != expected_directory
        or extension.name != f"_requests_rust{expected_suffix}"
    ):
        raise RuntimeError(
            f"loaded unexpected Rust-backed Python extension: {extension}"
        )
    loaded_digest = sha256(extension)
    build_digest = sha256(release_build)
    if loaded_digest != build_digest:
        raise RuntimeError(
            "loaded Python extension digest differs from the release build"
        )
    if provenance["extension_backend"] != "requests-native":
        raise RuntimeError("loaded Python extension reports the wrong backend")
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]
    expected_version = (
        workspace["version"]
        .replace("-beta.", "b")
        .replace("-alpha.", "a")
        .replace("-rc.", "rc")
    )
    if provenance["distribution_version"] != expected_version:
        raise RuntimeError("loaded Python distribution reports the wrong version")
    if provenance["compatibility_version"] != "2.34.2":
        raise RuntimeError(
            "loaded Requests API reports the wrong compatibility version"
        )
    provenance["build_command"] = build_record["command"]
    provenance["build_environment"] = build_record["environment"]
    provenance["extension_sha256"] = loaded_digest
    provenance["release_build"] = str(release_build)
    provenance["release_build_sha256"] = build_digest
    return provenance


def run_json_command(
    command: list[str], *, cwd: pathlib.Path | None, timeout_seconds: float
) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            check=False,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(
            f"subprocess timed out after {timeout_seconds:g}s: {shlex.join(command)}"
        ) from error
    if completed.returncode:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {shlex.join(command)}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"command did not return one JSON object: {shlex.join(command)}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        ) from error


def tool_version(command: list[str]) -> str | None:
    try:
        return subprocess.run(
            command,
            cwd=ROOT,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        ).stdout.strip()
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def git_metadata() -> dict[str, Any]:
    commit = os.environ.get("REQUESTS_BENCHMARK_COMMIT") or tool_version(
        ["git", "rev-parse", "HEAD"]
    )
    status = (
        ""
        if os.environ.get("REQUESTS_BENCHMARK_COMMIT")
        else tool_version(["git", "status", "--short"])
    )
    oracle_commit = subprocess.run(
        ["git", "-C", str(ORACLE_ROOT), "rev-parse", "HEAD"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    oracle_status = subprocess.run(
        ["git", "-C", str(ORACLE_ROOT), "status", "--short"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout.strip()
    if oracle_status:
        raise RuntimeError(
            "frozen Python oracle is dirty; benchmark provenance is invalid"
        )
    frozen = tomllib.loads((ROOT / "ORACLE.lock").read_text())["frozen_source_commit"]
    subprocess.run(
        [
            "git",
            "-C",
            str(ORACLE_ROOT),
            "diff",
            "--exit-code",
            frozen,
            "--",
            "src/requests",
            "tests",
            "pyproject.toml",
            "setup.py",
        ],
        check=True,
        capture_output=True,
    )
    return {
        "rewrite_commit": commit,
        "rewrite_dirty": bool(status),
        "rewrite_status": status.splitlines() if status else [],
        "oracle_commit": oracle_commit,
        "oracle_frozen_source_commit": frozen,
        "oracle_dirty": False,
    }


def fixture_self_check(server: FixtureServer, expected_body: bytes) -> None:
    host, port = server.server_address
    connection = http.client.HTTPConnection(host, port, timeout=5)
    ids = []
    try:
        for _ in range(2):
            connection.request("GET", "/small")
            response = connection.getresponse()
            body = response.read()
            if response.status != 200 or body != expected_body:
                raise RuntimeError("loopback fixture returned an invalid response")
            ids.append(response.getheader("X-Benchmark-Connection"))
    finally:
        connection.close()
    if ids[0] != ids[1]:
        raise RuntimeError("loopback fixture did not preserve an HTTP/1.1 connection")


def worker_command(
    surface: str,
    *,
    url: str,
    requests: int,
    concurrency: int,
    mode: str,
    read: str,
    chunk_size: int,
    warmup: int = 0,
) -> list[str]:
    common = [
        "--surface",
        surface,
        "--url",
        url,
        "--requests",
        str(requests),
        "--concurrency",
        str(concurrency),
        "--mode",
        mode,
        "--read",
        read,
        "--chunk-size",
        str(chunk_size),
        "--warmup",
        str(warmup),
    ]
    if surface.startswith("python-"):
        return [
            sys.executable,
            str(pathlib.Path(__file__).resolve()),
            "--python-worker",
            *common,
        ]
    return [str(NATIVE_BINARY), *common]


def summarize(
    worker: dict[str, Any],
    *,
    surface: str,
    mode: str,
    body: str,
    read: str,
    concurrency: int,
    requests: int,
    accepted_connections: int,
    allocation_replay_concurrency: int,
) -> dict[str, Any]:
    milliseconds = [value / 1_000_000 for value in worker["latencies_ns"]]
    elapsed_seconds = worker["elapsed_ns"] / 1_000_000_000
    row = {
        "surface": surface,
        "mode": mode,
        "body": body,
        "read": read,
        "concurrency": concurrency,
        "requests": requests,
        "allocation_replay_concurrency": allocation_replay_concurrency,
        "elapsed_seconds": elapsed_seconds,
        "throughput_requests_per_second": requests / elapsed_seconds,
        "latency_ns_samples": worker["latencies_ns"],
        "latency_ms": {
            "min": min(milliseconds),
            "p50": percentile(milliseconds, 0.50),
            "p95": percentile(milliseconds, 0.95),
            "p99": percentile(milliseconds, 0.99),
            "max": max(milliseconds),
            "mean": sum(milliseconds) / len(milliseconds),
        },
        "cpu_seconds": worker["cpu_seconds"],
        "rss_peak_bytes": worker["rss_peak_bytes"],
        "rss_scope": worker["rss_scope"],
        "python_peak_alloc_bytes": worker["python_peak_alloc_bytes"],
        "native_total_allocated_bytes": worker["native_total_allocated_bytes"],
        "body_bytes_total": worker["body_bytes"],
        "checksum_total": worker["checksum"],
        "application_chunks": worker["application_chunks"],
        "connections_opened": accepted_connections,
        "connection_ids": worker["connection_ids"],
        "requests_reusing_connections": requests - accepted_connections,
        "implementation": worker["implementation"],
    }
    validate_result_row(row)
    return row


def orchestrate(arguments: argparse.Namespace) -> int:
    requests, small_size, large_size, maximum_concurrency = resolve_workload(arguments)
    surface_requests = resolve_surface_requests(requests, arguments.surface_requests)
    surfaces = arguments.surfaces or list(SURFACES)
    unknown = set(surfaces) - set(SURFACES)
    if unknown:
        raise ValueError(f"unknown surfaces: {sorted(unknown)}")
    concurrencies = sorted(
        {1, min(maximum_concurrency, *(surface_requests[s] for s in surfaces))}
    )

    small_body = bytes(index % 251 for index in range(small_size))
    large_body = bytes(index % 251 for index in range(large_size))
    bodies = {"small": small_body, "large": large_body}
    commands = [
        shlex.join(
            [sys.executable, str(pathlib.Path(__file__).resolve()), *sys.argv[1:]]
        )
    ]
    release_build, build_record = build_python_extension(commands)
    build_native(commands)
    python_provenance = python_artifact_provenance(
        commands,
        arguments.case_timeout_seconds,
        release_build,
        build_record,
    )
    metadata = {
        "schema_version": SCHEMA_VERSION,
        "generated_at_utc": dt.datetime.now(dt.UTC).isoformat(),
        "machine": {
            "platform": platform.platform(),
            "architecture": platform.machine(),
            "processor": platform.processor(),
            "logical_cpu_count": os.cpu_count(),
        },
        "toolchains": {
            "python": sys.version,
            "rustc": tool_version(["rustc", "--version"]),
            "cargo": tool_version(["cargo", "--version"]),
        },
        "git": git_metadata(),
        "python_artifact": python_provenance,
        "config": {
            "profile": arguments.profile,
            "requests_per_case": requests,
            "requests_per_surface": surface_requests,
            "small_bytes": small_size,
            "large_bytes": large_size,
            "stream_chunk_bytes": arguments.chunk_size,
            "concurrency_levels": concurrencies,
            "surfaces": surfaces,
            "warmup_per_worker": arguments.warmup,
        },
    }
    rows = []
    with fixture_server(small_body, large_body) as server:
        fixture_self_check(server, small_body)
        host, port = server.server_address
        for surface in surfaces:
            requests = surface_requests[surface]
            for mode in ("one-shot", "pooled"):
                for body_name, expected_body in bodies.items():
                    for read in ("buffered", "streaming"):
                        for concurrency in concurrencies:
                            url = f"http://{host}:{port}/{body_name}"
                            command = worker_command(
                                surface,
                                url=url,
                                requests=requests,
                                concurrency=concurrency,
                                mode=mode,
                                read=read,
                                chunk_size=arguments.chunk_size,
                                warmup=arguments.warmup,
                            )
                            before_requests, before_connections = server.snapshot()
                            commands.append(shlex.join(command))
                            case_name = (
                                f"{surface}/{mode}/{body_name}/{read}/"
                                f"concurrency-{concurrency}"
                            )
                            try:
                                worker = run_json_command(
                                    command,
                                    cwd=ROOT,
                                    timeout_seconds=arguments.case_timeout_seconds,
                                )
                            except RuntimeError as error:
                                raise RuntimeError(
                                    f"case failed: {case_name}: {error}"
                                ) from error
                            after_requests, after_connections = server.snapshot()
                            request_delta = after_requests - before_requests
                            accepted_delta = after_connections - before_connections
                            warmup_requests = arguments.warmup * min(
                                concurrency, requests
                            )
                            if request_delta != requests + warmup_requests:
                                raise RuntimeError(
                                    f"fixture observed {request_delta} requests, expected {requests}"
                                )
                            if accepted_delta != len(
                                set(worker["connection_ids"])
                                | set(worker["warmup_connection_ids"])
                            ):
                                raise RuntimeError(
                                    "fixture and worker disagree on accepted connections"
                                )
                            validate_warmup(
                                worker,
                                warmup_requests,
                                expected_body,
                                arguments.chunk_size,
                                read,
                                surface,
                            )
                            validate_worker_result(
                                worker,
                                requests=requests,
                                expected_bytes=len(expected_body),
                                expected_checksum=body_checksum(expected_body),
                                expected_application_chunks=(
                                    0
                                    if read == "buffered"
                                    else requests
                                    * math.ceil(
                                        len(expected_body) / arguments.chunk_size
                                    )
                                ),
                                require_native=surface == "python-rust",
                            )
                            if (
                                surface == "python-oracle"
                                and worker["native_responses"] != 0
                            ):
                                raise RuntimeError(
                                    "frozen Python oracle unexpectedly used a native response"
                                )
                            allocation_concurrency = (
                                1 if surface.startswith("python-") else concurrency
                            )
                            allocation_command = [
                                *worker_command(
                                    surface,
                                    url=url,
                                    requests=requests,
                                    concurrency=allocation_concurrency,
                                    mode=mode,
                                    read=read,
                                    chunk_size=arguments.chunk_size,
                                    warmup=arguments.warmup,
                                ),
                                "--measure-allocations",
                            ]
                            allocation_before, _ = server.snapshot()
                            commands.append(shlex.join(allocation_command))
                            try:
                                allocation_worker = run_json_command(
                                    allocation_command,
                                    cwd=ROOT,
                                    timeout_seconds=arguments.case_timeout_seconds,
                                )
                            except RuntimeError as error:
                                raise RuntimeError(
                                    f"allocation replay failed: {case_name}: {error}"
                                ) from error
                            allocation_after, _ = server.snapshot()
                            if (
                                allocation_after - allocation_before
                                != requests
                                + arguments.warmup
                                * min(allocation_concurrency, requests)
                            ):
                                raise RuntimeError(
                                    f"allocation replay sent the wrong request count: {case_name}"
                                )
                            try:
                                validate_warmup(
                                    allocation_worker,
                                    arguments.warmup
                                    * min(allocation_concurrency, requests),
                                    expected_body,
                                    arguments.chunk_size,
                                    read,
                                    surface,
                                )
                                validate_worker_result(
                                    allocation_worker,
                                    requests=requests,
                                    expected_bytes=len(expected_body),
                                    expected_checksum=body_checksum(expected_body),
                                    expected_application_chunks=(
                                        0
                                        if read == "buffered"
                                        else requests
                                        * math.ceil(
                                            len(expected_body) / arguments.chunk_size
                                        )
                                    ),
                                    require_native=surface == "python-rust",
                                )
                                validate_allocations(allocation_worker, surface)
                            except ValueError as error:
                                raise RuntimeError(
                                    f"allocation replay failed for {case_name}: {error}; "
                                    "native responses="
                                    f"{allocation_worker['native_responses']}"
                                ) from error
                            if surface.startswith("python-"):
                                worker["python_peak_alloc_bytes"] = allocation_worker[
                                    "python_peak_alloc_bytes"
                                ]
                            else:
                                worker["native_total_allocated_bytes"] = (
                                    allocation_worker["native_total_allocated_bytes"]
                                )
                            rows.append(
                                summarize(
                                    worker,
                                    surface=surface,
                                    mode=mode,
                                    body=body_name,
                                    read=read,
                                    concurrency=concurrency,
                                    requests=requests,
                                    accepted_connections=len(worker["connection_ids"]),
                                    allocation_replay_concurrency=allocation_concurrency,
                                )
                            )

    document = sanitize_public_report(
        {**metadata, "commands": commands, "results": rows}
    )
    output = arguments.output
    if output is None:
        stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        output = ROOT / "benchmarks" / "results" / f"{stamp}-{arguments.profile}.json"
    output = pathlib.Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    print(output)
    return 0


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    command.add_argument("--python-worker", action="store_true", help=argparse.SUPPRESS)
    command.add_argument(
        "--measure-allocations", action="store_true", help=argparse.SUPPRESS
    )
    command.add_argument("--profile", choices=("smoke", "default"), default="default")
    command.add_argument("--surfaces", nargs="+", choices=SURFACES)
    command.add_argument("--requests", type=int)
    command.add_argument(
        "--surface-requests",
        action="append",
        default=[],
        metavar="SURFACE=N",
        help="Override a surface's fixed request count (repeat for other surfaces)",
    )
    command.add_argument(
        "--warmup",
        type=int,
        default=0,
        help="Untimed requests per logical worker on the same clients",
    )
    command.add_argument("--concurrency", type=int)
    command.add_argument("--small-bytes", type=int)
    command.add_argument("--large-bytes", type=int)
    command.add_argument("--chunk-size", type=int, default=16 * 1024)
    command.add_argument("--case-timeout-seconds", type=float, default=30.0)
    command.add_argument("--output", type=pathlib.Path)
    command.add_argument("--surface", choices=SURFACES, help=argparse.SUPPRESS)
    command.add_argument("--url", help=argparse.SUPPRESS)
    command.add_argument(
        "--mode", choices=("one-shot", "pooled"), help=argparse.SUPPRESS
    )
    command.add_argument(
        "--read", choices=("buffered", "streaming"), help=argparse.SUPPRESS
    )
    return command


def main() -> int:
    arguments = parser().parse_args()
    if arguments.python_worker:
        required = (arguments.surface, arguments.url, arguments.mode, arguments.read)
        if (
            any(value is None for value in required)
            or arguments.requests is None
            or arguments.concurrency is None
        ):
            raise ValueError("internal Python worker arguments are incomplete")
        return python_worker(arguments)
    return orchestrate(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
