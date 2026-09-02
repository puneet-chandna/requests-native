#!/usr/bin/env python3
"""Reproducible loopback benchmarks for all Requests rewrite surfaces."""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import datetime as dt
import http.client
import json
import math
import os
import pathlib
import platform
import shlex
import subprocess
import sys
import threading
import time
import tracemalloc
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

try:
    import resource
except ImportError:  # pragma: no cover - Windows records RSS as unavailable.
    resource = None  # type: ignore[assignment]


ROOT = pathlib.Path(__file__).resolve().parents[1]
ORACLE_ROOT = ROOT.parent / "requests"
NATIVE_BINARY = ROOT / "target" / "release" / "requests-benchmark-native"
NATIVE_MANIFEST = ROOT / "benchmarks" / "rust-native" / "Cargo.toml"
SCHEMA_VERSION = 1
SURFACES = ("python-oracle", "python-rust", "rust-async", "rust-blocking")


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


def body_checksum(body: bytes) -> int:
    return sum(body)


def validate_worker_result(
    worker: dict[str, Any],
    *,
    requests: int,
    expected_bytes: int,
    expected_checksum: int,
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
        "implementation",
    }
    missing = required - worker.keys()
    if missing:
        raise ValueError(f"worker result missing keys: {sorted(missing)}")
    latencies = worker["latencies_ns"]
    if len(latencies) != requests or any(value <= 0 for value in latencies):
        raise ValueError("worker returned an invalid latency sample count")
    if worker["body_bytes"] != requests * expected_bytes:
        raise ValueError("worker body bytes differ from the fixture contract")
    if worker["checksum"] != requests * expected_checksum:
        raise ValueError("worker checksum differs from the fixture contract")
    if require_native and worker["native_responses"] != requests:
        raise ValueError("Rust-backed Python did not route every response natively")
    if worker["elapsed_ns"] <= 0 or (
        worker["cpu_seconds"] is not None and worker["cpu_seconds"] < 0
    ):
        raise ValueError("worker returned invalid timing data")
    if not worker["connection_ids"]:
        raise ValueError("worker did not observe a fixture connection")


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
    if arguments.surface == "python-oracle":
        sys.path.insert(0, str(ORACLE_ROOT / "src"))
    import requests  # noqa: PLC0415

    implementation = pathlib.Path(requests.__file__).resolve()
    expected_root = ORACLE_ROOT if arguments.surface == "python-oracle" else ROOT
    if not implementation.is_relative_to(expected_root):
        raise RuntimeError(
            f"{arguments.surface} imported {implementation}, outside {expected_root}"
        )

    native_responses = 0
    lock = threading.Lock()

    def consume(session, url: str) -> tuple[int, int, int, int]:
        nonlocal native_responses
        start = time.perf_counter_ns()
        with session.get(url, stream=arguments.read == "streaming") as response:
            response.raise_for_status()
            connection_id = int(response.headers["X-Benchmark-Connection"])
            is_native = type(response.raw).__module__ == "requests._requests_rust"
            if arguments.read == "buffered":
                body = response.content
                size = len(body)
                checksum = body_checksum(body)
            else:
                size = 0
                checksum = 0
                for chunk in response.iter_content(arguments.chunk_size):
                    size += len(chunk)
                    checksum += body_checksum(chunk)
        latency = time.perf_counter_ns() - start
        if is_native:
            with lock:
                native_responses += 1
        return latency, size, checksum, connection_id

    def run_client(count: int) -> list[tuple[int, int, int, int]]:
        pooled = None
        if arguments.mode == "pooled":
            pooled = requests.Session()
            pooled.trust_env = False
        output = []
        try:
            for _ in range(count):
                if pooled is None:
                    with requests.Session() as one_shot:
                        one_shot.trust_env = False
                        output.append(consume(one_shot, arguments.url))
                else:
                    output.append(consume(pooled, arguments.url))
        finally:
            if pooled is not None:
                pooled.close()
        return output

    if arguments.measure_allocations:
        tracemalloc.start()
    cpu_start = time.process_time()
    wall_start = time.perf_counter_ns()
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=min(arguments.concurrency, arguments.requests)
    ) as executor:
        nested = list(
            executor.map(
                run_client, split_work(arguments.requests, arguments.concurrency)
            )
        )
    elapsed_ns = time.perf_counter_ns() - wall_start
    cpu_seconds = time.process_time() - cpu_start
    allocation_peak = None
    if arguments.measure_allocations:
        _, allocation_peak = tracemalloc.get_traced_memory()
        tracemalloc.stop()

    observations = [item for group in nested for item in group]
    rss_peak_bytes = (
        resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
        if resource is not None
        else None
    )
    result = {
        "latencies_ns": [item[0] for item in observations],
        "body_bytes": sum(item[1] for item in observations),
        "checksum": sum(item[2] for item in observations),
        "connection_ids": sorted({item[3] for item in observations}),
        "elapsed_ns": elapsed_ns,
        "cpu_seconds": cpu_seconds,
        "rss_peak_bytes": rss_peak_bytes,
        "python_peak_alloc_bytes": allocation_peak,
        "native_responses": native_responses,
        "implementation": str(implementation),
    }
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
        "--offline",
    ]
    command_log.append(shlex.join(command))
    subprocess.run(command, cwd=ROOT, check=True)
    if not NATIVE_BINARY.is_file():
        raise RuntimeError(f"native benchmark binary missing: {NATIVE_BINARY}")


def run_json_command(command: list[str], *, cwd: pathlib.Path) -> dict[str, Any]:
    completed = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        text=True,
        capture_output=True,
    )
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
    commit = tool_version(["git", "rev-parse", "HEAD"])
    status = tool_version(["git", "status", "--short"])
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
    return {
        "rewrite_commit": commit,
        "rewrite_dirty": bool(status),
        "rewrite_status": status.splitlines() if status else [],
        "oracle_commit": oracle_commit,
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
) -> dict[str, Any]:
    milliseconds = [value / 1_000_000 for value in worker["latencies_ns"]]
    elapsed_seconds = worker["elapsed_ns"] / 1_000_000_000
    return {
        "surface": surface,
        "mode": mode,
        "body": body,
        "read": read,
        "concurrency": concurrency,
        "requests": requests,
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
        "python_peak_alloc_bytes": worker["python_peak_alloc_bytes"],
        "body_bytes_total": worker["body_bytes"],
        "checksum_total": worker["checksum"],
        "connections_opened": accepted_connections,
        "connection_ids": worker["connection_ids"],
        "requests_reusing_connections": requests - accepted_connections,
        "implementation": worker["implementation"],
    }


def orchestrate(arguments: argparse.Namespace) -> int:
    profile = {
        "smoke": (2, 64, 32 * 1024, 2),
        "default": (12, 128, 256 * 1024, 4),
    }[arguments.profile]
    requests = arguments.requests or profile[0]
    small_size = arguments.small_bytes or profile[1]
    large_size = arguments.large_bytes or profile[2]
    maximum_concurrency = arguments.concurrency or profile[3]
    if (
        min(requests, small_size, large_size, maximum_concurrency, arguments.chunk_size)
        < 1
    ):
        raise ValueError("all workload sizes must be positive")
    concurrencies = sorted({1, min(requests, maximum_concurrency)})
    surfaces = arguments.surfaces or list(SURFACES)
    unknown = set(surfaces) - set(SURFACES)
    if unknown:
        raise ValueError(f"unknown surfaces: {sorted(unknown)}")

    small_body = bytes(index % 251 for index in range(small_size))
    large_body = bytes(index % 251 for index in range(large_size))
    bodies = {"small": small_body, "large": large_body}
    commands = [
        shlex.join(
            [sys.executable, str(pathlib.Path(__file__).resolve()), *sys.argv[1:]]
        )
    ]
    build_native(commands)
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
        "config": {
            "profile": arguments.profile,
            "requests_per_case": requests,
            "small_bytes": small_size,
            "large_bytes": large_size,
            "stream_chunk_bytes": arguments.chunk_size,
            "concurrency_levels": concurrencies,
            "surfaces": surfaces,
        },
    }
    rows = []
    with fixture_server(small_body, large_body) as server:
        fixture_self_check(server, small_body)
        host, port = server.server_address
        for surface in surfaces:
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
                            )
                            before_requests, before_connections = server.snapshot()
                            commands.append(shlex.join(command))
                            worker = run_json_command(command, cwd=ROOT)
                            after_requests, after_connections = server.snapshot()
                            request_delta = after_requests - before_requests
                            accepted_delta = after_connections - before_connections
                            if request_delta != requests:
                                raise RuntimeError(
                                    f"fixture observed {request_delta} requests, expected {requests}"
                                )
                            if accepted_delta != len(worker["connection_ids"]):
                                raise RuntimeError(
                                    "fixture and worker disagree on accepted connections"
                                )
                            validate_worker_result(
                                worker,
                                requests=requests,
                                expected_bytes=len(expected_body),
                                expected_checksum=body_checksum(expected_body),
                                require_native=surface == "python-rust",
                            )
                            if (
                                surface == "python-oracle"
                                and worker["native_responses"] != 0
                            ):
                                raise RuntimeError(
                                    "frozen Python oracle unexpectedly used a native response"
                                )
                            if surface.startswith("python-"):
                                allocation_command = [*command, "--measure-allocations"]
                                allocation_before, _ = server.snapshot()
                                commands.append(shlex.join(allocation_command))
                                allocation_worker = run_json_command(
                                    allocation_command, cwd=ROOT
                                )
                                allocation_after, _ = server.snapshot()
                                if allocation_after - allocation_before != requests:
                                    raise RuntimeError(
                                        "allocation replay sent the wrong request count"
                                    )
                                try:
                                    validate_worker_result(
                                        allocation_worker,
                                        requests=requests,
                                        expected_bytes=len(expected_body),
                                        expected_checksum=body_checksum(expected_body),
                                        require_native=surface == "python-rust",
                                    )
                                except ValueError as error:
                                    raise RuntimeError(
                                        "allocation replay failed for "
                                        f"{surface}/{mode}/{body_name}/{read}/"
                                        f"concurrency-{concurrency}: {error}; "
                                        "native responses="
                                        f"{allocation_worker['native_responses']}"
                                    ) from error
                                worker["python_peak_alloc_bytes"] = allocation_worker[
                                    "python_peak_alloc_bytes"
                                ]
                            rows.append(
                                summarize(
                                    worker,
                                    surface=surface,
                                    mode=mode,
                                    body=body_name,
                                    read=read,
                                    concurrency=concurrency,
                                    requests=requests,
                                    accepted_connections=accepted_delta,
                                )
                            )

    document = {**metadata, "commands": commands, "results": rows}
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
    command.add_argument("--concurrency", type=int)
    command.add_argument("--small-bytes", type=int)
    command.add_argument("--large-bytes", type=int)
    command.add_argument("--chunk-size", type=int, default=16 * 1024)
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
