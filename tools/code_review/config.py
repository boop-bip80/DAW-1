"""Configuration for the local reviewer (``.code-review.toml``)."""

from __future__ import annotations

import functools
import ipaddress
import os
import re
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

from .findings import SEVERITIES

CONFIG_FILE = ".code-review.toml"
DEFAULT_BASE_URL = "https://api.deepseek.com/v1"
DEFAULT_MODEL = "deepseek-chat"
DEFAULT_API_KEY_ENV = "DEEPSEEK_API_KEY"


class ConfigError(ValueError):
    """The configuration file is missing, malformed or inconsistent."""


@functools.lru_cache(maxsize=256)
def glob_to_regex(pattern: str) -> re.Pattern[str]:
    """Translate a small glob dialect (``**``, ``*``, ``?``) to a path regex."""
    out: list[str] = ["^"]
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if char == "*":
            if pattern.startswith("**", index):
                out.append(".*")
                index += 2
                continue
            out.append("[^/]*")
        elif char == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(char))
        index += 1
    out.append("$")
    return re.compile("".join(out))


@dataclass(frozen=True)
class PathInstruction:
    """Extra review guidance applied to files matching ``pattern``."""

    pattern: str
    instructions: str

    def matches(self, path: str) -> bool:
        return bool(glob_to_regex(self.pattern).match(path))


@dataclass
class Config:
    base: str = "origin/main"
    fail_on: str = "major"
    max_input_tokens: int = 24_000
    max_findings: int = 40
    verify: bool = True
    include_untracked: bool = True
    context_lines: int = 40
    max_file_chars: int = 300_000
    exclude: list[str] = field(default_factory=list)
    path_instructions: list[PathInstruction] = field(default_factory=list)
    rule_severity: dict[str, str] = field(default_factory=dict)
    base_url: str = DEFAULT_BASE_URL
    model: str = DEFAULT_MODEL
    api_key_env: str = DEFAULT_API_KEY_ENV
    timeout_seconds: int = 180
    max_retries: int = 2

    @property
    def fail_rank(self) -> int:
        return SEVERITIES.index(self.fail_on)

    def excluded(self, path: str) -> bool:
        return any(glob_to_regex(pattern).match(path) for pattern in self.exclude)

    def instructions_for(self, path: str) -> list[str]:
        return [item.instructions for item in self.path_instructions if item.matches(path)]

    def api_key(self) -> str:
        direct = os.environ.get("REVIEW_API_KEY", "").strip()
        return direct or os.environ.get(self.api_key_env, "").strip()

    @property
    def is_local_endpoint(self) -> bool:
        """True only for a loopback endpoint, which may run without an API key."""
        host = (urlparse(self.base_url).hostname or "").lower()
        if host == "localhost":
            return True
        try:
            return ipaddress.ip_address(host).is_loopback
        except ValueError:
            return False


def _env(name: str, default: str) -> str:
    return os.environ.get(name, "").strip() or default


def _string(data: dict, key: str, default: str) -> str:
    value = data.get(key, default)
    if not isinstance(value, str):
        raise ConfigError(f"{key} must be a string")
    return value


def _integer(data: dict, key: str, default: int) -> int:
    value = data.get(key, default)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ConfigError(f"{key} must be a positive integer")
    return value


def _boolean(data: dict, key: str, default: bool) -> bool:
    value = data.get(key, default)
    if not isinstance(value, bool):
        raise ConfigError(f"{key} must be true or false")
    return value


def load_config(root: Path, path: Path | None = None) -> Config:
    """Load ``.code-review.toml`` from ``root`` (or an explicit path)."""
    file = path or (root / CONFIG_FILE)
    data: dict = {}
    if file.exists():
        try:
            data = tomllib.loads(file.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as exc:
            raise ConfigError(f"{file}: {exc}") from exc
    elif path is not None:
        raise ConfigError(f"{path}: no such file")

    review = data.get("review", {})
    llm = data.get("llm", {})
    if not isinstance(review, dict) or not isinstance(llm, dict):
        raise ConfigError(f"{file}: [review] and [llm] must be tables")

    # The endpoint and the key's variable name are environment-only. A checked-in file must not
    # be able to point the reviewer (and its API key) at a different host.
    for key in ("base_url", "api_key_env"):
        if key in llm:
            raise ConfigError(
                f"{file}: llm.{key} is set from the environment, not the repository "
                "(REVIEW_API_BASE / REVIEW_API_KEY_ENV); remove it from the file"
            )

    config = Config(
        base=_string(review, "base", Config.base),
        fail_on=_string(review, "fail_on", Config.fail_on),
        max_input_tokens=_integer(review, "max_input_tokens", Config.max_input_tokens),
        max_findings=_integer(review, "max_findings", Config.max_findings),
        verify=_boolean(review, "verify", Config.verify),
        include_untracked=_boolean(review, "include_untracked", Config.include_untracked),
        context_lines=_integer(review, "context_lines", Config.context_lines),
        max_file_chars=_integer(review, "max_file_chars", Config.max_file_chars),
        model=_string(llm, "model", Config.model),
        timeout_seconds=_integer(llm, "timeout_seconds", Config.timeout_seconds),
        max_retries=_integer(llm, "max_retries", Config.max_retries),
    )

    if config.fail_on not in SEVERITIES:
        raise ConfigError(f"fail_on must be one of {', '.join(SEVERITIES)}")

    exclude = review.get("exclude", [])
    if not isinstance(exclude, list) or not all(isinstance(item, str) for item in exclude):
        raise ConfigError("review.exclude must be a list of strings")
    config.exclude = list(exclude)

    instructions = review.get("path_instructions", [])
    if not isinstance(instructions, list):
        raise ConfigError("review.path_instructions must be an array of tables")
    for item in instructions:
        if not isinstance(item, dict) or not isinstance(item.get("path"), str) or not isinstance(item.get("instructions"), str):
            raise ConfigError("each review.path_instructions entry needs a string path and instructions")
        config.path_instructions.append(PathInstruction(item["path"], item["instructions"]))

    severity = review.get("rule_severity", {})
    if not isinstance(severity, dict) or not all(isinstance(v, str) for v in severity.values()):
        raise ConfigError("review.rule_severity must map rule ids to severities")
    for key, value in severity.items():
        if value not in SEVERITIES:
            raise ConfigError(f"rule severity for {key} must be one of {', '.join(SEVERITIES)}")
    config.rule_severity = dict(severity)

    # Environment overrides keep per-machine endpoints (Ollama, llama.cpp) out of the file.
    config.base = _env("REVIEW_BASE", config.base)
    config.base_url = _env("REVIEW_API_BASE", config.base_url)
    config.model = _env("REVIEW_MODEL", config.model)
    config.api_key_env = _env("REVIEW_API_KEY_ENV", config.api_key_env)
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", config.api_key_env):
        raise ConfigError(f"REVIEW_API_KEY_ENV must name an environment variable, got {config.api_key_env!r}")

    return config
