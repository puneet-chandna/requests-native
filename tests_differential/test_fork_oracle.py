from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
REWRITE_SOURCE = ROOT / "src"

pytestmark = pytest.mark.skipif(
    not hasattr(os, "fork"), reason="fork characterization requires POSIX"
)


_FORK_PROBE = r"""
import json
import os
import select
import threading
from pathlib import Path

import requests

assert Path(requests.__file__).resolve() == (
    Path(os.environ["REQUESTS_FORK_SOURCE"]) / "requests/__init__.py"
)

scenario = os.environ["REQUESTS_FORK_SCENARIO"]
session = requests.Session() if scenario == "idle-session" else None
assert threading.active_count() == 1

read_fd, write_fd = os.pipe()
parent_pid = os.getpid()
child_pid = os.fork()
if child_pid == 0:
    os.close(read_fd)
    exit_code = 0
    try:
        active_session = session if session is not None else requests.Session()
        prepared = active_session.prepare_request(
            requests.Request("GET", "https://example.invalid/fork")
        )
        active_session.close()
        payload = json.dumps(
            {
                "method": prepared.method,
                "path": prepared.path_url,
                "pid_changed": os.getpid() != parent_pid,
            },
            sort_keys=True,
        ).encode()
        os.write(write_fd, payload)
    except BaseException as error:
        exit_code = 70
        os.write(
            write_fd,
            json.dumps(
                {"error": f"{type(error).__module__}.{type(error).__qualname__}"}
            ).encode(),
        )
    finally:
        os.close(write_fd)
        os._exit(exit_code)

os.close(write_fd)
ready, _, _ = select.select([read_fd], [], [], 5.0)
if not ready:
    os.kill(child_pid, 9)
    os.waitpid(child_pid, 0)
    raise TimeoutError("fork child did not write to its pipe")
child_payload = os.read(read_fd, 4096)
os.close(read_fd)
waited_pid, status = os.waitpid(child_pid, 0)
assert waited_pid == child_pid
assert os.waitstatus_to_exitcode(status) == 0

if session is not None:
    prepared = session.prepare_request(
        requests.Request("GET", "https://example.invalid/parent")
    )
    assert prepared.path_url == "/parent"
    session.close()

print(child_payload.decode())
"""


def _run_probe(package_source: Path, scenario: str) -> dict[str, object]:
    package_source = package_source.resolve()
    if not (package_source / "requests" / "__init__.py").is_file():
        raise FileNotFoundError(
            f"requests source package not found under {package_source}"
        )
    environment = {
        name: os.environ[name]
        for name in (
            "COMSPEC",
            "DYLD_LIBRARY_PATH",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LD_LIBRARY_PATH",
            "PATH",
            "PATHEXT",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "TMPDIR",
            "WINDIR",
        )
        if name in os.environ
    }
    environment.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONHASHSEED": "0",
            "PYTHONNOUSERSITE": "1",
            "PYTHONPATH": str(package_source),
            "REQUESTS_FORK_SCENARIO": scenario,
            "REQUESTS_FORK_SOURCE": str(package_source),
        }
    )
    completed = subprocess.run(
        [sys.executable, "-c", _FORK_PROBE],
        cwd=ROOT,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert completed.returncode == 0, completed.stderr
    return json.loads(completed.stdout)


@pytest.mark.parametrize("scenario", ["import-only", "idle-session"])
def test_rewrite_matches_single_threaded_frozen_oracle_fork_behavior(
    scenario: str,
    oracle_root: Path,
) -> None:
    expected = {
        "method": "GET",
        "path": "/fork",
        "pid_changed": True,
    }
    assert _run_probe(oracle_root / "src", scenario) == expected
    assert _run_probe(REWRITE_SOURCE, scenario) == expected


def test_fork_probe_rejects_missing_source_before_import(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError, match="requests source package not found"):
        _run_probe(tmp_path / "missing", "import-only")
