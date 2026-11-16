#!/usr/bin/env python3
"""Generate deterministic V19-00 replay bundles and their hash manifest."""

from __future__ import annotations

import argparse
import binascii
import hashlib
import json
import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = ROOT / "fixtures" / "v019-benchmark" / "bundles"
SCHEMA = "rpi-v19-replay-v1"
CADENCE_MS = 16


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def markdown_history(target_bytes: int, code_heavy: bool = False) -> str:
    output = ""
    index = 0
    while len(output.encode("utf-8")) < target_bytes:
        if code_heavy or index % 3 == 0:
            output += (
                f"\n## Historical section {index}\n\n"
                "```rust\n"
                f"fn stable_block_{index}(value: usize) -> usize {{\n"
                f"    value + {index}\n"
                "}\n```\n"
            )
        else:
            output += (
                f"\n## Historical note {index}\n\n"
                "Stable Markdown text with **bold**, [links](https://example.com), "
                "and enough content to exercise wrapping and retained history.\n"
            )
        index += 1
    encoded = output.encode("utf-8")[:target_bytes]
    return encoded.decode("ascii")


def stream_events(
    kind: str,
    deltas: int,
    *,
    keyboard: bool = True,
    scroll_back: bool = False,
    resize: bool = False,
) -> list[dict[str, object]]:
    events: list[dict[str, object]] = [
        {"atMs": 0, "kind": "agent_start"},
        {"atMs": CADENCE_MS, "kind": "message_start"},
    ]
    at_ms = CADENCE_MS * 2
    raw_tool = ""
    if kind == "tool":
        payload = "x" * (deltas * 257)
        raw_tool = json.dumps(
            {"path": "/tmp/rpi-v19-replay.txt", "content": payload},
            separators=(",", ":"),
        )

    for index in range(deltas):
        if kind == "text":
            event = {
                "kind": "text_delta",
                "text": f"Plain streaming sentence {index}. The renderer keeps the final text.\n",
            }
        elif kind == "code":
            event = {
                "kind": "text_delta",
                "text": (
                    "```rust\n" if index == 0 else ""
                )
                + f"pub fn stream_{index}(value: usize) -> usize {{ value * {index + 1} }}\n"
                + ("```\n" if index == deltas - 1 else ""),
            }
        elif kind == "cjk":
            event = {
                "kind": "text_delta",
                "text": f"第 {index} 段：流式输出保持中文、emoji 🚀 与组合字符 cafe\u0301。\n",
            }
        elif kind == "mermaid":
            if index == 0:
                text = "Before the diagram.\n```mer"
            elif index == 1:
                text = "maid\ngraph TD\n"
            elif index == deltas - 1:
                text = "```\nAfter the diagram.\n"
            else:
                text = f"  N{index}[Step {index}] --> N{index + 1}[Next]\n"
            event = {"kind": "text_delta", "text": text}
        elif kind == "tool":
            start = index * 257
            event = {
                "kind": "tool_call_delta",
                "id": "replay-write",
                "name": "write",
                "text": raw_tool[start : start + 257],
            }
        else:
            raise ValueError(kind)
        event["atMs"] = at_ms
        events.append(event)
        at_ms += CADENCE_MS

        if keyboard and index == deltas // 2:
            events.append({"atMs": at_ms, "kind": "keyboard_input", "text": "x"})
            at_ms += CADENCE_MS
        if scroll_back and index == (deltas * 2) // 3:
            events.append({"atMs": at_ms, "kind": "scroll_back", "count": 3})
            at_ms += CADENCE_MS
        if resize and index == deltas // 2:
            events.append(
                {"atMs": at_ms, "kind": "resize", "columns": 200, "rows": 60}
            )
            at_ms += CADENCE_MS

    if kind == "tool":
        events.extend(
            [
                {"atMs": at_ms, "kind": "message_end"},
                {
                    "atMs": at_ms + CADENCE_MS,
                    "kind": "tool_execution_start",
                    "id": "replay-write",
                    "name": "write",
                    "arguments": {},
                },
                {
                    "atMs": at_ms + CADENCE_MS * 2,
                    "kind": "tool_execution_end",
                    "id": "replay-write",
                    "result": {"content": [{"type": "text", "text": "ok"}]},
                },
            ]
        )
        at_ms += CADENCE_MS * 2
    else:
        events.append({"atMs": at_ms, "kind": "message_end"})
    events.append({"atMs": at_ms + CADENCE_MS, "kind": "agent_settled"})
    return events


def png_image(seed: int, size: int = 64) -> str:
    import base64

    state = seed
    raw = bytearray()
    for _row in range(size):
        raw.append(0)
        for _ in range(size):
            for _channel in range(3):
                state = (1103515245 * state + 12345) & 0x7FFFFFFF
                raw.append((state >> 8) & 0xFF)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", binascii.crc32(kind + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    encoded = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 1))
        + chunk(b"IEND", b"")
    )
    return base64.b64encode(encoded).decode("ascii")


def memory_events(model_switch: str | None = None) -> list[dict[str, object]]:
    events: list[dict[str, object]] = [
        {"atMs": 0, "kind": "sample", "name": "startup"},
        {"atMs": CADENCE_MS, "kind": "agent_start"},
        {"atMs": CADENCE_MS * 2, "kind": "message_start"},
        {"atMs": CADENCE_MS * 3, "kind": "text_delta", "text": "memory sample "},
        {"atMs": CADENCE_MS * 4, "kind": "sample", "name": "first-tokenizer"},
        {"atMs": CADENCE_MS * 5, "kind": "text_delta", "text": "streaming"},
        {"atMs": CADENCE_MS * 6, "kind": "sample", "name": "streaming"},
        {"atMs": CADENCE_MS * 7, "kind": "message_end"},
        {"atMs": CADENCE_MS * 8, "kind": "sample", "name": "message-end"},
    ]
    at_ms = CADENCE_MS * 9
    for _ in range(50):
        events.append({"atMs": at_ms, "kind": "wait"})
        at_ms += CADENCE_MS
    events.append({"atMs": at_ms, "kind": "sample", "name": "idle-50"})
    at_ms += CADENCE_MS
    events.extend(
        [
            {"atMs": at_ms, "kind": "new_session"},
            {"atMs": at_ms + CADENCE_MS, "kind": "sample", "name": "new-session"},
            {"atMs": at_ms + CADENCE_MS * 2, "kind": "resume_session"},
            {"atMs": at_ms + CADENCE_MS * 3, "kind": "sample", "name": "resume"},
        ]
    )
    at_ms += CADENCE_MS * 4
    if model_switch:
        events.extend(
            [
                {"atMs": at_ms, "kind": "model_switch", "text": model_switch},
                {"atMs": at_ms + CADENCE_MS, "kind": "sample", "name": "model-switch"},
            ]
        )
    return events


def memory_bundle(
    scenario: str,
    *,
    history: str = "",
    extensions: str = "default",
    provider: str = "custom",
    model: str = "m1",
    model_switch: str | None = None,
    images: int = 0,
) -> dict[str, object]:
    result = bundle(
        scenario,
        "text",
        history=history,
        extensions=extensions,
        provider=provider,
        model=model,
        deltas=1,
    )
    result["suite"] = "memory"
    result["events"] = memory_events(model_switch)
    if images:
        result["imageHistory"] = [png_image(index + 1) for index in range(images)]
    return result


def bundle(
    scenario: str,
    kind: str,
    *,
    history: str = "",
    mode: str = "fullscreen",
    columns: int = 80,
    rows: int = 24,
    deltas: int = 80,
    settings: dict[str, object] | None = None,
    extensions: str = "none",
    provider: str = "custom",
    model: str = "m1",
    scroll_back: bool = False,
    resize: bool = False,
) -> dict[str, object]:
    return {
        "schema": SCHEMA,
        "scenario": scenario,
        "config": {
            "mode": mode,
            "columns": columns,
            "rows": rows,
            "cadenceMs": CADENCE_MS,
            "provider": provider,
            "model": model,
        },
        "settings": settings or {},
        "extensions": extensions,
        "history": [history] if history else [],
        "events": stream_events(
            kind,
            deltas,
            scroll_back=scroll_back,
            resize=resize,
        ),
    }


def scenarios() -> list[dict[str, object]]:
    history_1mib = markdown_history(1024 * 1024)
    code_history = markdown_history(256 * 1024, code_heavy=True)
    return [
        bundle("smoke-text-regular", "text", mode="regular", deltas=4),
        bundle("text-regular-new-80", "text", mode="regular"),
        bundle("text-fullscreen-new-80", "text"),
        bundle("text-fullscreen-new-200", "text", columns=200, rows=60),
        bundle("code-fullscreen-1mib-main", "code", history=history_1mib, deltas=240),
        bundle("code-regular-1mib-80", "code", history=history_1mib, mode="regular", deltas=160),
        bundle(
            "code-regular-1mib-200",
            "code",
            history=history_1mib,
            mode="regular",
            columns=200,
            rows=60,
            deltas=120,
        ),
        bundle("cjk-fullscreen-1mib-200", "cjk", history=history_1mib, columns=200, rows=60),
        bundle("mermaid-streaming-fullscreen", "mermaid"),
        bundle(
            "mermaid-off-fullscreen",
            "mermaid",
            settings={"markdown": {"mermaid": "off"}},
        ),
        bundle("long-arguments-fullscreen", "tool", deltas=160),
        bundle(
            "code-fullscreen-history-scroll",
            "code",
            history=code_history,
            deltas=120,
            scroll_back=True,
        ),
        bundle("code-fullscreen-resize", "code", history=code_history, deltas=120, resize=True),
        bundle(
            "text-regular-livetokens-off",
            "text",
            mode="regular",
            settings={"statusLine": {"liveTokens": False}},
        ),
        bundle(
            "text-fullscreen-default-statusline",
            "text",
            extensions="default",
            settings={
                "statusLine": {
                    "type": "items",
                    "items": ["live-total-tokens", "live-output-rate"],
                    "liveTokens": True,
                }
            },
        ),
        bundle("text-fullscreen-cumulative-extension", "text", extensions="cumulative"),
        bundle(
            "text-fullscreen-kimi",
            "text",
            extensions="default",
            provider="kimi-coding",
            model="k3.1-pro",
        ),
        bundle(
            "text-fullscreen-glm",
            "text",
            extensions="default",
            provider="zai",
            model="glm-5.3",
        ),
        memory_bundle("memory-empty-default"),
        memory_bundle("memory-empty-no-extensions", extensions="none"),
        memory_bundle("memory-long-default", history=history_1mib),
        memory_bundle("memory-long-no-extensions", history=history_1mib, extensions="none"),
        memory_bundle(
            "memory-kimi-switch-glm",
            provider="kimi-coding",
            model="k3.1-pro",
            model_switch="zai/glm-5.3",
        ),
        memory_bundle(
            "memory-glm-switch-kimi",
            provider="zai",
            model="glm-5.3",
            model_switch="kimi-coding/k3.1-pro",
        ),
        memory_bundle("memory-png-over-budget", images=40),
    ]


def render(bundle_value: dict[str, object]) -> bytes:
    return (json.dumps(bundle_value, ensure_ascii=False, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


def expected_files(output: Path) -> dict[str, bytes]:
    return {f"{item['scenario']}.json": render(item) for item in scenarios()}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    expected = expected_files(output)
    suite_by_name = {
        f"{item['scenario']}.json": item.get("suite", "cpu") for item in scenarios()
    }
    manifest = {
        "schema": "rpi-v19-benchmark-inputs-v1",
        "bundles": {
            name: {
                "sha256": sha256(data),
                "bytes": len(data),
                "suite": suite_by_name[name],
            }
            for name, data in sorted(expected.items())
        },
    }
    manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    expected["manifest.json"] = manifest_bytes

    if args.check:
        failures = []
        for name, data in expected.items():
            path = output / name
            if not path.is_file() or path.read_bytes() != data:
                failures.append(name)
        actual = {path.name for path in output.glob("*.json")} if output.is_dir() else set()
        for name in sorted(actual - set(expected)):
            failures.append(f"unexpected:{name}")
        if failures:
            print("stale replay inputs: " + ", ".join(failures), file=sys.stderr)
            return 1
        print(f"ok: {len(expected) - 1} replay bundles and manifest are current")
        return 0

    output.mkdir(parents=True, exist_ok=True)
    for path in output.glob("*.json"):
        if path.name not in expected:
            print(f"refusing to remove unexpected file {path}", file=sys.stderr)
            return 1
    for name, data in expected.items():
        (output / name).write_bytes(data)
    print(f"generated {len(expected) - 1} replay bundles in {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
