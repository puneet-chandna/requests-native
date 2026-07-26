from __future__ import annotations

import subprocess
import sys
import zipfile
from pathlib import Path


def test_official_urllib3_harness_rejects_a_wheel_with_the_wrong_hash(tmp_path: Path):
    wheel = tmp_path / "urllib3-1.26.20-py2.py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("urllib3/__init__.py", '__version__ = "1.26.20"\n')

    result = subprocess.run(
        [
            sys.executable,
            "scripts/run-urllib3-126-adapter-evidence.py",
            "--wheel",
            str(wheel),
        ],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 2
    assert "sha256 mismatch" in result.stderr
