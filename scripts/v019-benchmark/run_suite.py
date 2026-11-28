#!/usr/bin/env python3
"""Run every V19-00 formal replay scenario sequentially and index the evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BUNDLES = ROOT / "fixtures" / "v019-benchmark" / "bundles"


def hash_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command_output(command: list[str]) -> str:
    return subprocess.run(command, check=False, capture_output=True, text=True).stdout.strip()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument(
        "--suite", choices=("cpu", "memory", "backpressure"), default="cpu"
    )
    parser.add_argument("--arena")
    parser.add_argument("--only", help="comma-separated scenario names for development")
    parser.add_argument("--include-smoke", action="store_true")
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--overlay-sha256", required=True)
    parser.add_argument("--cargo-lock", type=Path)
    args = parser.parse_args()

    subprocess.run(
        [sys.executable, str(ROOT / "scripts/v019-benchmark/generate_inputs.py"), "--check"],
        cwd=ROOT,
        check=True,
    )
    manifest = json.loads((BUNDLES / "manifest.json").read_text())
    selected = set(args.only.split(",")) if args.only else None
    names = [
        name
        for name in sorted(manifest["bundles"])
        if manifest["bundles"][name].get("suite", "cpu")
        in ({"cpu"} if args.suite == "backpressure" else {args.suite})
        and (
            args.suite == "memory"
            or args.include_smoke
            or name != "smoke-text-regular.json"
        )
        and (selected is None or name.removesuffix(".json") in selected)
    ]
    if not names or (selected is not None and len(names) != len(selected)):
        raise ValueError("unknown or empty scenario selection")

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    binary = args.binary.resolve()
    reports = []
    started = time.monotonic()
    for name in names:
        scenario = name.removesuffix(".json")
        log = output / f"{scenario}.runner.log"
        command = [
            sys.executable,
            str(ROOT / "scripts/v019-benchmark/run_replay.py"),
            "--binary",
            str(binary),
            "--bundle",
            str(BUNDLES / name),
            "--output-dir",
            str(output / scenario),
            "--repeat",
            str(args.repeat),
        ]
        if args.suite == "backpressure":
            # The backpressure report is a separate measurement: the driver's
            # tick-driven schedule with adaptive 60/30/20 fps tiers enabled.
            command.extend(["--render-mode", "throttled"])
        if args.arena is not None:
            command.extend(["--arena", args.arena])
        with log.open("w") as log_file:
            subprocess.run(
                command,
                cwd=ROOT,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                check=True,
            )
        report_path = output / scenario / "report.json"
        report = json.loads(report_path.read_text())
        entry = {
            "scenario": scenario,
            "inputSha256": report["inputSha256"],
            "report": str(report_path),
            "frameLogs": report["frameLogs"],
            "processCpuSecondsMedian": report["processCpuSecondsMedian"],
            "processCpuWallRatioMedian": report["processCpuWallRatioMedian"],
            "cpuPerRenderedFrameSecondsMedian": report["cpuPerRenderedFrameSecondsMedian"],
            "cpuPerInputDeltaSecondsMedian": report["cpuPerInputDeltaSecondsMedian"],
            "pureComputeP95Ns": report["pureComputeP95Ns"],
            "frameIntervalP95NsMedian": report["frameIntervalP95NsMedian"],
            "inputFirstVisibleP95NsMedian": report["inputFirstVisibleP95NsMedian"],
        }
        if args.suite == "backpressure":
            entry["backpressurePerRun"] = report["backpressurePerRun"]
            entry["finalScreenSha256"] = report["finalScreenSha256"]
        if args.suite == "memory":
            samples = []
            for run_summary_path in report["runSummaries"]:
                run_summary = json.loads(Path(run_summary_path).read_text())
                for capture_path in run_summary.get("memoryCaptures", []):
                    capture = json.loads(Path(capture_path).read_text())
                    processes = []
                    for process in capture["processes"]:
                        process_summary_path = Path(process["summary"])
                        process_summary = json.loads(process_summary_path.read_text())
                        processes.append(
                            {
                                "pid": process["pid"],
                                "top": process_summary["top"],
                                "status": process_summary["status"],
                                "smapsRollup": process_summary["smapsRollup"],
                                "summary": str(process_summary_path),
                            }
                        )
                    samples.append(
                        {
                            "runSummary": run_summary_path,
                            "capture": capture_path,
                            "label": capture["label"],
                            "processes": processes,
                        }
                    )
            entry["memorySamples"] = samples
        reports.append(entry)

    cpu_model = ""
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.is_file():
        for line in cpuinfo.read_text().splitlines():
            if line.startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    suite = {
        "schema": "rpi-v19-benchmark-suite-v1",
        "suite": args.suite,
        "arena": args.arena,
        "sourceCommit": args.source_commit,
        "overlaySha256": args.overlay_sha256,
        "binary": str(binary),
        "binarySha256": hash_file(binary),
        "cargoLock": str((args.cargo_lock or ROOT / "Cargo.lock").resolve()),
        "cargoLockSha256": hash_file(args.cargo_lock or ROOT / "Cargo.lock"),
        "buildProfile": "release",
        # The CPU gate runs at a fixed cadence with adaptive backpressure
        # disabled; the backpressure suite is the separate throttled report.
        "fixedCadence": args.suite != "backpressure",
        "adaptiveBackpressure": (
            "enabled-throttled" if args.suite == "backpressure" else "disabled-by-test-switch"
        ),
        "runsPerScenario": args.repeat,
        "scenarioCount": len(reports),
        "wallSeconds": time.monotonic() - started,
        "hardware": {
            "hostname": platform.node(),
            "os": platform.platform(),
            "kernel": platform.release(),
            "architecture": platform.machine(),
            "libc": " ".join(platform.libc_ver()),
            "cpuModel": cpu_model,
            "logicalCpus": os.cpu_count(),
        },
        "rust": {
            "rustc": command_output(["rustc", "--version"]),
            "cargo": command_output(["cargo", "--version"]),
        },
        "reports": reports,
        "note": "Baseline facts only; no v0.1.9 target is claimed. Memory-suite timings include sampling pauses and are not CPU-gate measurements. Backpressure-suite frame boundaries depend on wall-clock frame costs and never count towards the fixed-cadence CPU gate.",
    }
    suite_path = output / "suite-report.json"
    suite_path.write_text(json.dumps(suite, indent=2) + "\n")
    print(f"ok: {len(reports)} scenarios x {args.repeat} runs; report: {suite_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
