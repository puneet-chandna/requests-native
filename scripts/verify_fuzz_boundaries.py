from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "fuzz_url": "url",
    "fuzz_headers": "headers",
    "fuzz_cookies": "cookies",
    "fuzz_redirect": "redirect",
    "fuzz_body_state": "body-state",
}


def main() -> int:
    for target, corpus_name in TARGETS.items():
        corpus = ROOT / "crates" / "requests" / "fuzz-corpus" / corpus_name
        if not corpus.is_dir() or not any(path.is_file() for path in corpus.iterdir()):
            print(f"missing replay seed for {target}: {corpus}", file=sys.stderr)
            return 1
        completed = subprocess.run(
            [
                "cargo",
                "bolero",
                "test",
                target,
                "--package",
                "requests-native",
                "--toolchain",
                "nightly",
                "--runs",
                "1000",
                "--corpus-dir",
                str(corpus),
            ],
            cwd=ROOT,
            check=False,
        )
        if completed.returncode:
            return completed.returncode
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
