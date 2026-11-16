#!/usr/bin/env python3
"""Capture Linux process memory evidence and compare mapping-level A/B data."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

MAPPING_RE = re.compile(r"^[0-9a-f]+-[0-9a-f]+\s+(\S+)\s+\S+\s+\S+\s+\S+(?:\s+(.*))?$")
METRIC_RE = re.compile(r"^([A-Za-z0-9_]+):\s+(\d+)(?:\s+kB)?\s*$")
STATUS_FIELDS = (
    "VmSize",
    "VmRSS",
    "VmHWM",
    "RssAnon",
    "RssFile",
    "RssShmem",
    "Threads",
)
ROLLUP_FIELDS = (
    "Rss",
    "Pss",
    "Pss_Anon",
    "Pss_File",
    "Private_Clean",
    "Private_Dirty",
    "Shared_Clean",
    "Shared_Dirty",
    "Swap",
)


def parse_metrics(text: str) -> dict[str, int]:
    result = {}
    for line in text.splitlines():
        match = METRIC_RE.match(line)
        if match:
            result[match.group(1)] = int(match.group(2))
    return result


def parse_smaps(text: str) -> list[dict[str, object]]:
    mappings: list[dict[str, object]] = []
    current: dict[str, object] | None = None
    for line in text.splitlines():
        match = MAPPING_RE.match(line)
        if match:
            pathname = (match.group(2) or "[anonymous]").strip() or "[anonymous]"
            current = {
                "path": pathname,
                "permissions": match.group(1),
                "rssKib": 0,
                "pssKib": 0,
                "sharedKib": 0,
                "privateKib": 0,
            }
            mappings.append(current)
            continue
        metric = METRIC_RE.match(line)
        if current is None or metric is None:
            continue
        name, value = metric.group(1), int(metric.group(2))
        if name == "Rss":
            current["rssKib"] = value
        elif name == "Pss":
            current["pssKib"] = value
        elif name in ("Shared_Clean", "Shared_Dirty"):
            current["sharedKib"] = int(current["sharedKib"]) + value
        elif name in ("Private_Clean", "Private_Dirty"):
            current["privateKib"] = int(current["privateKib"]) + value
    return mappings


def aggregate_mappings(mappings: list[dict[str, object]]) -> list[dict[str, object]]:
    grouped: dict[tuple[str, str], dict[str, object]] = {}
    for mapping in mappings:
        key = (str(mapping["path"]), str(mapping["permissions"]))
        entry = grouped.setdefault(
            key,
            {
                "path": key[0],
                "permissions": key[1],
                "rssKib": 0,
                "pssKib": 0,
                "sharedKib": 0,
                "privateKib": 0,
                "segments": 0,
            },
        )
        for field in ("rssKib", "pssKib", "sharedKib", "privateKib"):
            entry[field] = int(entry[field]) + int(mapping[field])
        entry["segments"] = int(entry["segments"]) + 1
    return sorted(grouped.values(), key=lambda item: (item["path"], item["permissions"]))


def child_pids(pid: int) -> list[int]:
    children = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "stat").read_text()
            fields = raw[raw.rindex(")") + 2 :].split()
            if int(fields[1]) == pid:
                children.append(int(entry.name))
        except (OSError, ValueError, IndexError):
            continue
    return sorted(children)


def top_memory_kib(value: str) -> float:
    units = {"": 1.0, "k": 1.0, "m": 1024.0, "g": 1024.0**2, "t": 1024.0**3}
    suffix = value[-1:].lower()
    if suffix in units:
        return float(value[:-1] or "0") * units[suffix] if suffix else float(value)
    return float(value)


def parse_top(text: str, pid: int) -> dict[str, float]:
    lines = text.splitlines()
    for index, line in enumerate(lines):
        columns = line.split()
        if not columns or columns[0] != "PID" or not {"VIRT", "RES", "SHR"}.issubset(columns):
            continue
        for row in lines[index + 1 :]:
            values = row.split()
            if not values or values[0] != str(pid):
                continue
            return {
                "virtKib": top_memory_kib(values[columns.index("VIRT")]),
                "resKib": top_memory_kib(values[columns.index("RES")]),
                "shrKib": top_memory_kib(values[columns.index("SHR")]),
            }
    raise ValueError(f"top output has no VIRT/RES/SHR row for pid {pid}")


def capture_one(pid: int, output: Path) -> dict[str, object]:
    proc = Path(f"/proc/{pid}")
    output.mkdir(parents=True, exist_ok=True)
    raw_files = {}
    for name in ("status", "smaps_rollup", "smaps"):
        data = (proc / name).read_bytes()
        (output / name).write_bytes(data)
        raw_files[name] = str(output / name)
    top = subprocess.run(
        ["top", "-b", "-n", "1", "-p", str(pid)],
        check=False,
        capture_output=True,
        text=True,
    )
    (output / "top.txt").write_text(top.stdout + top.stderr, encoding="utf-8")
    if top.returncode != 0:
        raise ValueError(f"top failed for pid {pid}: {top.stderr.strip()}")
    top_metrics = parse_top(top.stdout, pid)
    status_text = (proc / "status").read_text()
    rollup_text = (proc / "smaps_rollup").read_text()
    mappings = aggregate_mappings(parse_smaps((proc / "smaps").read_text()))
    summary = {
        "pid": pid,
        "status": {key: parse_metrics(status_text).get(key) for key in STATUS_FIELDS},
        "smapsRollup": {key: parse_metrics(rollup_text).get(key) for key in ROLLUP_FIELDS},
        "top": top_metrics,
        "mappings": mappings,
        "raw": {**raw_files, "top": str(output / "top.txt")},
        "topRaw": str(output / "top.txt"),
    }
    missing = [key for key, value in summary["status"].items() if value is None]
    if missing:
        raise ValueError(f"pid {pid} missing status fields: {', '.join(missing)}")
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def capture(pid: int, label: str, output: Path) -> dict[str, object]:
    output.mkdir(parents=True, exist_ok=True)
    pids = []
    pending = [pid]
    while pending:
        current = pending.pop(0)
        if current in pids:
            continue
        pids.append(current)
        pending.extend(child_pids(current))
    processes = []
    for child in pids:
        processes.append(capture_one(child, output / str(child)))
    metadata = {
        "schema": "rpi-v19-proc-capture-v1",
        "label": label,
        "capturedAtUnix": time.time(),
        "hostname": platform.node(),
        "os": platform.platform(),
        "kernel": platform.release(),
        "architecture": platform.machine(),
        "libc": " ".join(platform.libc_ver()),
        "terminal": {
            key: os.environ.get(key) for key in ("TERM", "TERM_PROGRAM", "COLORTERM")
        },
        "processes": [{"pid": item["pid"], "summary": str(output / str(item["pid"]) / "summary.json")} for item in processes],
        "topNote": "VIRT/RES/SHR remain in raw top.txt; status/smaps values are KiB and are not summed across parent/child.",
    }
    (output / "capture.json").write_text(json.dumps(metadata, indent=2) + "\n")
    return metadata


def load_mappings(path: Path) -> list[dict[str, object]]:
    document = json.loads(path.read_text())
    if "mappings" in document:
        return list(document["mappings"])
    if "processes" in document:
        raise ValueError("compare one process summary at a time; parent/child reports stay separate")
    raise ValueError(f"no mappings in {path}")


def compare(baseline: Path, candidate: Path) -> dict[str, object]:
    before = {(str(m["path"]), str(m["permissions"])): m for m in load_mappings(baseline)}
    after = {(str(m["path"]), str(m["permissions"])): m for m in load_mappings(candidate)}
    deltas = []
    for key in sorted(set(before) | set(after)):
        old = before.get(key, {})
        new = after.get(key, {})
        delta = {
            "path": key[0],
            "permissions": key[1],
            "sharedKibDelta": int(new.get("sharedKib", 0)) - int(old.get("sharedKib", 0)),
            "rssKibDelta": int(new.get("rssKib", 0)) - int(old.get("rssKib", 0)),
            "rssFileKibDelta": (
                int(new.get("rssKib", 0)) - int(old.get("rssKib", 0))
                if not key[0].startswith("[")
                else 0
            ),
            "pssKibDelta": int(new.get("pssKib", 0)) - int(old.get("pssKib", 0)),
            "existing": key in before,
            "readOnlyCode": key[1].startswith("r") and "x" in key[1],
        }
        deltas.append(delta)
    report = {
        "schema": "rpi-v19-mapping-delta-v1",
        "baseline": str(baseline),
        "candidate": str(candidate),
        "deltas": deltas,
        "budgetTemplate": {
            "jsonschemaCodegen": "source attribution required by V19-01",
            "otherV019ReadOnlyCode": "all other new r-x code-page deltas share this budget",
            "existingMappings": "existing mappings must not grow without attribution",
            "newNonCodeMappings": "new non-code mappings are not automatically exempt",
        },
    }
    try:
        check_budget(report)
    except ValueError as error:
        # The caller writes the report before returning non-zero, preserving
        # the offending mapping evidence instead of losing it on failure.
        report["budgetError"] = str(error)
    return report


def check_budget(report: dict[str, object]) -> dict[str, int]:
    code_shared = 0
    code_rss = 0
    non_code_shared = 0
    for delta in report["deltas"]:
        shared = max(0, int(delta["sharedKibDelta"]))
        rss = max(0, int(delta.get("rssFileKibDelta", delta["rssKibDelta"])))
        is_code = str(delta["permissions"]).startswith("r") and "x" in str(delta["permissions"])
        if is_code:
            code_shared += shared
            code_rss += rss
        elif not delta["existing"]:
            non_code_shared += shared
    result = {
        "newReadOnlyCodeSharedKib": code_shared,
        "newReadOnlyCodeRssFileKib": code_rss,
        "newNonCodeSharedKib": non_code_shared,
        "existingSharedKibGrowth": sum(
            max(0, int(delta["sharedKibDelta"]))
            for delta in report["deltas"]
            if delta["existing"]
        ),
    }
    if code_shared > 2048 or code_rss > 2048:
        raise ValueError(f"new read-only code pages exceed the shared 2 MiB budget: {result}")
    report["budgetCheck"] = result
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    cap = sub.add_parser("capture")
    cap.add_argument("--pid", type=int, required=True)
    cap.add_argument("--label", required=True)
    cap.add_argument("--output", type=Path, required=True)
    cmp = sub.add_parser("compare")
    cmp.add_argument("--baseline", type=Path, required=True)
    cmp.add_argument("--candidate", type=Path, required=True)
    cmp.add_argument("--output", type=Path, required=True)
    check = sub.add_parser("check-budget")
    check.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "capture":
        capture(args.pid, args.label, args.output)
    elif args.command == "compare":
        report = compare(args.baseline, args.candidate)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        if "budgetError" in report:
            raise ValueError(str(report["budgetError"]))
    else:
        report = json.loads(args.report.read_text())
        print(json.dumps(check_budget(report), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
