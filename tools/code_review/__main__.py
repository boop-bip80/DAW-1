"""Run the local reviewer: ``python3 -m tools.code_review``."""

from __future__ import annotations

from .cli import main

if __name__ == "__main__":
    raise SystemExit(main())
