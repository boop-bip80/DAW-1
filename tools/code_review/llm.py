"""A minimal OpenAI-compatible chat client, standard library only.

Works with DeepSeek, OpenAI, Ollama (``/v1``), llama.cpp's server and any other
endpoint that speaks ``POST /chat/completions``. Retries transient failures;
never logs the API key.
"""

from __future__ import annotations

import json
import math
import time
import urllib.error
import urllib.request

from .config import Config


class LLMError(RuntimeError):
    """The model endpoint was unreachable, rejected the request or returned junk."""


def estimate_tokens(text: str) -> int:
    """Rough token estimate; good enough for budgeting (CJK is denser than this)."""
    return max(1, math.ceil(len(text) / 3.5))


def extract_json(text: str) -> dict:
    """Extract the first JSON object from a reply, tolerating fences and prose."""
    stripped = text.strip()
    if stripped.startswith("```"):
        lines = stripped.splitlines()
        if len(lines) >= 2:
            stripped = "\n".join(lines[1:])
            if stripped.rstrip().endswith("```"):
                stripped = stripped.rstrip()[:-3]
    decoder = json.JSONDecoder()
    for index, char in enumerate(stripped):
        if char != "{":
            continue
        try:
            value, _ = decoder.raw_decode(stripped, index)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    raise LLMError(f"model reply contained no JSON object (starts: {text.strip()[:200]!r})")


class LLMClient:
    """Chat client for an OpenAI-compatible base URL."""

    def __init__(self, config: Config) -> None:
        self.config = config

    @property
    def available(self) -> bool:
        return bool(self.config.api_key()) or self.config.is_local_endpoint

    @property
    def endpoint(self) -> str:
        return self.config.base_url.rstrip("/") + "/chat/completions"

    def chat(self, system: str, user: str, *, max_tokens: int = 2000) -> str:
        """Send one chat request and return the assistant text."""
        if not self.available:
            raise LLMError(
                f"no API key: set {self.config.api_key_env} (or REVIEW_API_KEY), "
                "or point [llm] base_url at a local server and pass --offline only for rules"
            )
        payload: dict = {
            "model": self.config.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.1,
            "max_tokens": max_tokens,
            "stream": False,
        }
        if "deepseek" in self.config.base_url or "openai.com" in self.config.base_url:
            payload["response_format"] = {"type": "json_object"}
        body = json.dumps(payload).encode("utf-8")
        headers = {"Content-Type": "application/json"}
        key = self.config.api_key()
        if key:
            headers["Authorization"] = f"Bearer {key}"

        last_error: Exception | None = None
        for attempt in range(self.config.max_retries + 1):
            request = urllib.request.Request(self.endpoint, data=body, headers=headers, method="POST")
            try:
                with urllib.request.urlopen(request, timeout=self.config.timeout_seconds) as response:
                    raw = response.read().decode("utf-8", errors="replace")
                data = json.loads(raw)
                choices = data.get("choices") or []
                if not choices:
                    raise LLMError(f"endpoint returned no choices: {raw[:300]!r}")
                content = choices[0].get("message", {}).get("content")
                if not isinstance(content, str) or not content.strip():
                    raise LLMError("endpoint returned an empty message")
                return content
            except urllib.error.HTTPError as exc:
                detail = exc.read().decode("utf-8", errors="replace")[:500]
                last_error = LLMError(f"HTTP {exc.code} from {self.endpoint}: {detail}")
                if exc.code in {400, 401, 403, 404, 422}:
                    raise last_error from exc
            except (urllib.error.URLError, TimeoutError, OSError) as exc:
                last_error = LLMError(f"cannot reach {self.endpoint}: {exc}")
            except json.JSONDecodeError as exc:
                last_error = LLMError(f"endpoint returned invalid JSON: {exc}")
            if attempt < self.config.max_retries:
                time.sleep(min(2**attempt, 8))
        raise last_error or LLMError("request failed")

    def chat_json(self, system: str, user: str, *, max_tokens: int = 2000) -> dict:
        return extract_json(self.chat(system, user, max_tokens=max_tokens))
