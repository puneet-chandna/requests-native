from __future__ import annotations

import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tests_differential.runner import (  # noqa: E402
    DEFAULT_ORACLE_ROOT,
    run_oracle_case,
)


def main() -> int:
    oracle_root = Path(
        os.environ.get("REQUESTS_ORACLE_ROOT", DEFAULT_ORACLE_ROOT)
    ).resolve()
    run = run_oracle_case(
        {
            "source": (
                "import requests\n"
                "side_effects.append({"
                "'file': requests.__file__, "
                "'version': requests.__version__"
                "})\n"
            )
        }
    )
    imported = run.observations["side_effects"][0]
    requests_file = Path(imported["file"]).resolve()
    if not requests_file.is_relative_to(oracle_root):
        print(
            f"oracle contamination: imported {requests_file} outside {oracle_root}",
            file=sys.stderr,
        )
        return 1
    print(
        json.dumps(
            {
                "oracle_root": str(oracle_root),
                "requests_file": str(requests_file),
                "version": imported["version"],
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
