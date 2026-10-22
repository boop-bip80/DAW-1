# Local code review

`cargo xtask review` runs a CodeRabbit-style review on the changes in this
working tree, without the hosted bot. It is for authors, before a pull request:
run it, fix what it finds, then push.

It complements upstream's `.coderabbit.yaml`, which configures the hosted bot on
GitHub. The hosted engine is proprietary and cannot be built locally; this is an
independent reimplementation of the workflow (diff → rules → model → verification
→ report), tuned to the standards in [`AGENTS.md`](../../AGENTS.md).

## Quick start

```sh
# Deterministic rules only: no key, no network, safe in CI.
cargo xtask review --offline --base origin/main

# With the default model endpoint (DeepSeek; set DEEPSEEK_API_KEY first).
cargo xtask review --base origin/main

# Markdown for a PR description, JSON for a bot.
cargo xtask review --format markdown --output review.md
cargo xtask review --format json --output review.json
```

On a fork whose `origin` is the fork itself rather than the upstream repository,
add the upstream as a second remote and review against it —
`cargo xtask review --base upstream/main`, or export `REVIEW_BASE=upstream/main`.
Comparing against the fork's own, older `main` pulls unrelated history into the
diff and buries the actual changes.

The tool is plain Python 3.11+ (standard library only) and lives in
`tools/code_review/`. `cargo xtask review` forwards every argument to
`python3 -m tools.code_review`; set `PYTHON` to use a different interpreter.

### With a local model

Any OpenAI-compatible `POST /chat/completions` endpoint works. No API key is
required for loopback endpoints:

```sh
REVIEW_API_BASE=http://127.0.0.1:11434/v1 REVIEW_MODEL=qwen2.5-coder:14b \
  cargo xtask review --base origin/main
```

llama.cpp's server and LM Studio work the same way. Put the values in
`[llm]` in `.code-review.toml` to make them the default.

## What it does

1. **Collects the diff** against the merge-base of `--base` and `HEAD`
   (or `HEAD..worktree` / `HEAD..index`), including untracked files.
2. **Applies deterministic rules** that encode rules from `AGENTS.md`:
   panics in production code, `unsafe` outside the FFI host crates, missing
   `// SAFETY:` comments, `static mut`, `process::exit` in libraries, `dbg!`,
   printing from DSP/playback code, and missing `ATTRIBUTION.md` rows for new
   assets. Tests are exempt.
3. **Asks the model** about numbered windows around each hunk, with
   path-specific instructions from `.code-review.toml` and the repository
   standards in the system prompt.
4. **Verifies** every model finding in a second, skeptical pass; rejected
   findings are dropped, uncertain ones are marked.
5. **Merges, de-duplicates and sorts** the results, then renders a console,
   Markdown or JSON report. The process exits non-zero when a finding at or
   above `--fail-on` survives.

## Configuration

`.code-review.toml` at the repository root (see the commented file):

| Key | Meaning |
|---|---|
| `review.base` | Default `--base` revision (`origin/main`; use `upstream/main` on a fork). |
| `review.fail_on` | `critical`, `major`, `minor` or `nit`; exit 1 at/above it. |
| `review.max_findings` | Cap on reported findings. |
| `review.max_input_tokens` | Input budget per model window. |
| `review.context_lines` | Context lines around each hunk. |
| `review.verify` | Run the second verification pass. |
| `review.include_untracked` | Review new, untracked files too. |
| `review.exclude` | Glob list of paths to skip. |
| `review.path_instructions` | Extra guidance for matching paths. |
| `review.rule_severity` | Override the severity of a deterministic rule id. |
| `llm.base_url` | OpenAI-compatible endpoint. |
| `llm.model` | Model name. |
| `llm.api_key_env` | Environment variable holding the key. |
| `llm.timeout_seconds`, `llm.max_retries` | Request behaviour. |

Environment overrides: `REVIEW_BASE`, `REVIEW_API_BASE`, `REVIEW_MODEL`,
`REVIEW_API_KEY` (wins over `llm.api_key_env`).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | No finding at or above `--fail-on`. |
| 1 | At least one finding at or above `--fail-on`. |
| 2 | Bad configuration, git error, or unwritable report. |
| 3 | The model pass was requested but no endpoint/key is available, or every model request failed. |

## Scope and limits

- It is **not** CodeRabbit and shares no code with it. There is no sandboxed
  cloud execution, no 50-linter toolchain, no auto-fix, no chat, and no GitHub
  review posting. The report is text you can paste into a PR.
- The model sees your diff and the surrounding file content. Do not run it
  against code you may not send to the configured endpoint; `--offline` keeps
  everything local.
- Findings are advisory. `cargo xtask ci` remains the gate that must pass.
- The rules are intentionally narrow: every deterministic hit is a documented
  violation, so `--offline` remains trustworthy.

## Tests

```sh
python3 -m unittest discover -s tools/code_review/tests -t .
```

## Adding a deterministic rule

Add the pattern and message to `tools/code_review/rules.py`, give it a stable
`source` id (reported in the output and overridable via `rule_severity`), and
add a case to `tools/code_review/tests/test_rules.py`. Rules must not flag
comment-only lines or test modules.
