"""Command line entry point for ``cargo xtask review`` / ``python3 -m tools.code_review``."""

from __future__ import annotations

import argparse
import dataclasses
import sys
from pathlib import Path

from . import __version__
from .config import ConfigError, load_config
from .gitio import GitError, Repo
from .llm import LLMClient
from .pipeline import run
from .report import render_console, render_json, render_markdown

EPILOG = """examples:
  python3 -m tools.code_review --offline
  python3 -m tools.code_review --base origin/main --format markdown
  REVIEW_API_BASE=http://127.0.0.1:11434/v1 REVIEW_MODEL=qwen2.5-coder:14b \\
      python3 -m tools.code_review --base origin/main

exit codes: 0 clean or below --fail-on, 1 findings at/above --fail-on,
            2 configuration/git error, 3 model pass requested but unavailable."""


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="code_review",
        description="Local, CodeRabbit-style review for this repository: deterministic SoundCraft "
        "rules plus an optional model pass over the diff.",
        epilog=EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--version", action="version", version=f"soundcraft-code-review {__version__}")
    parser.add_argument("--config", type=Path, help="path to .code-review.toml (default: repository root)")
    parser.add_argument("--base", help="git revision to compare against, branch mode only (default: [review].base)")
    parser.add_argument(
        "--mode",
        choices=["branch", "working", "staged"],
        default="branch",
        help="branch: merge-base(base, HEAD)..worktree (default); working: HEAD..worktree; staged: HEAD..index",
    )
    parser.add_argument("--path", action="append", default=[], metavar="PATH", help="only review these paths (repeatable)")
    parser.add_argument("--offline", action="store_true", help="deterministic rules only, no model calls")
    parser.add_argument("--no-verify", action="store_true", help="skip the verification pass")
    parser.add_argument("--no-untracked", action="store_true", help="ignore untracked files")
    parser.add_argument("--max-findings", type=int, help="cap reported findings (default: [review].max_findings)")
    parser.add_argument(
        "--fail-on",
        choices=["critical", "major", "minor", "nit", "none"],
        help="exit 1 on findings at this severity or worse (default: [review].fail_on)",
    )
    parser.add_argument("--format", choices=["console", "markdown", "json"], default="console")
    parser.add_argument("--output", type=Path, help="write the report to a file instead of stdout")
    parser.add_argument("--quiet", action="store_true", help="no progress output on stderr")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    progress_stream = sys.stderr

    def progress(message: str) -> None:
        if not args.quiet:
            print(f"[review] {message}", file=progress_stream)

    try:
        repo = Repo.discover(Path.cwd())
    except GitError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    try:
        config = load_config(repo.root, args.config)
    except ConfigError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    overrides: dict = {}
    if args.fail_on is not None and args.fail_on != "none":
        overrides["fail_on"] = args.fail_on
    if args.max_findings is not None:
        if args.max_findings < 0:
            print("error: --max-findings must be >= 0", file=sys.stderr)
            return 2
        overrides["max_findings"] = args.max_findings
    if args.no_verify:
        overrides["verify"] = False
    if args.no_untracked:
        overrides["include_untracked"] = False
    config = dataclasses.replace(config, **overrides)
    fail_on = args.fail_on or config.fail_on

    use_llm = not args.offline
    if use_llm and not LLMClient(config).available:
        print(
            "error: the model pass needs an API key or a local endpoint.\n"
            f"  set {config.api_key_env} (or REVIEW_API_KEY), or run with --offline for rules only.\n"
            "  local example: REVIEW_API_BASE=http://127.0.0.1:11434/v1 REVIEW_MODEL=qwen2.5-coder:14b",
            file=sys.stderr,
        )
        return 3

    if use_llm:
        progress(f"model endpoint {config.base_url} (key from REVIEW_API_KEY or {config.api_key_env})")

    try:
        result = run(
            repo,
            config,
            base=args.base,
            mode=args.mode,
            paths=args.path,
            use_llm=use_llm,
            progress=progress,
        )
    except GitError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    for warning in result.warnings:
        print(f"warning: {warning}", file=sys.stderr)

    reviewable = any(item.diff.added_line_numbers() for item in result.files)
    if use_llm and reviewable and not result.llm_used:
        print("error: no file received a model review; refusing to report a partial result as success", file=sys.stderr)
        return 3
    if use_llm and result.llm_windows and result.llm_errors == result.llm_windows:
        print("error: every model request failed; refusing to report a rules-only result as success", file=sys.stderr)
        return 3

    if args.format == "json":
        text = render_json(result)
    elif args.format == "markdown":
        text = render_markdown(result)
    else:
        text = render_console(result, fail_on)

    if args.output:
        try:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(text + "\n", encoding="utf-8")
        except OSError as exc:
            print(f"error: cannot write {args.output}: {exc}", file=sys.stderr)
            return 2
        print(f"wrote {args.output} ({len(result.findings)} finding(s))")
    else:
        print(text)

    failing = result.failing(fail_on)
    return 1 if failing else 0


if __name__ == "__main__":
    raise SystemExit(main())
