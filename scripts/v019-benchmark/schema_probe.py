#!/usr/bin/env python3
"""Measure the real models/settings Validator::new paths in fresh release processes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import resource
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path

from proc_sample import capture, compare as compare_mappings, load_attribution

ROOT = Path(__file__).resolve().parents[2]
MODES = ("none", "models", "settings", "both")
STATUS_FIELDS = ("VmSize", "VmRSS", "VmHWM", "RssAnon", "RssFile", "RssShmem", "Threads")
ROLLUP_FIELDS = ("Rss", "Pss", "Pss_Anon", "Pss_File", "Private_Clean", "Private_Dirty", "Shared_Clean", "Shared_Dirty", "Swap")


def hash_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def wait_marker(process: subprocess.Popen, path: Path) -> None:
    deadline = time.monotonic() + 30
    while not path.is_file():
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise RuntimeError(
                f"probe exited before {path.name}: {stderr.decode('utf-8', 'replace')[-2000:]}"
            )
        if time.monotonic() >= deadline:
            raise TimeoutError(f"timed out waiting for {path}")
        time.sleep(0.005)


def process_cpu_seconds(pid: int) -> float:
    raw = Path(f"/proc/{pid}/stat").read_text()
    fields = raw[raw.rindex(")") + 2 :].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def one_summary(capture_dir: Path) -> tuple[dict, dict]:
    document = json.loads((capture_dir / "capture.json").read_text())
    if len(document["processes"]) != 1:
        raise ValueError("schema probe unexpectedly spawned child processes")
    summary_path = Path(document["processes"][0]["summary"])
    return document, json.loads(summary_path.read_text())


def run_one(binary: Path, models_json: Path, mode: str, run_dir: Path) -> dict:
    run_dir.mkdir(parents=True, exist_ok=True)
    samples = run_dir / "samples"
    summary_path = run_dir / "summary.json"
    command = [str(binary), mode, str(samples), str(summary_path), str(models_json)]
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(run_dir / "home"),
        "LANG": "C.UTF-8",
        "RPI_OFFLINE": "1",
    }
    before_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        captures = {}
        cpu = {}
        for label in ("before", "after"):
            wait_marker(process, samples / f"{label}.marker")
            cpu[label] = process_cpu_seconds(process.pid)
            capture(process.pid, label, samples / label)
            (samples / f"{label}.ack").write_text(label)
        stdout, stderr = process.communicate(timeout=30)
    except Exception:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise
    wall = time.monotonic() - started
    after_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    (run_dir / "stdout.log").write_bytes(stdout)
    (run_dir / "stderr.log").write_bytes(stderr)
    if process.returncode != 0:
        raise RuntimeError(
            f"schema probe exited {process.returncode}: {stderr.decode('utf-8', 'replace')[-2000:]}"
        )
    summary = json.loads(summary_path.read_text())
    expected = {
        "none": (False, False, False),
        "models": (True, False, False),
        "settings": (False, True, True),
        "both": (True, True, True),
    }[mode]
    actual = tuple(
        summary["validation"][key]
        for key in ("modelsValidated", "settingsValidated", "runtimeSettingsValidated")
    )
    if actual != expected:
        raise ValueError(f"probe did not execute the expected Validator::new paths: {actual}")
    result = {
        "mode": mode,
        "command": command,
        "summary": str(summary_path),
        "wallSeconds": wall,
        "processCpuSeconds": (after_usage.ru_utime - before_usage.ru_utime)
        + (after_usage.ru_stime - before_usage.ru_stime),
        "initializationCpuSeconds": cpu["after"] - cpu["before"],
        "captures": {},
    }
    for label in ("before", "after"):
        metadata, process_summary = one_summary(samples / label)
        result["captures"][label] = {
            "capture": str(samples / label / "capture.json"),
            "summary": str(samples / label / str(process.pid) / "summary.json"),
            "top": process_summary["top"],
            "status": process_summary["status"],
            "smapsRollup": process_summary["smapsRollup"],
        }
    (run_dir / "probe-run.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def median(values: list[float]) -> float:
    return float(statistics.median(values))


def aggregate_runs(runs: list[dict]) -> dict:
    result = {
        "runs": len(runs),
        "initializationNanosMedian": median([run["summary"] and json.loads(Path(run["summary"]).read_text())["initializationNanos"] for run in runs]),
        "initializationCpuSecondsMedian": median([run["initializationCpuSeconds"] for run in runs]),
        "processCpuSecondsMedian": median([run["processCpuSeconds"] for run in runs]),
        "after": {},
        "retainedDelta": {},
        "allocationDelta": {},
        "runDetails": [str(Path(run["summary"]).parent / "probe-run.json") for run in runs],
    }
    for field in STATUS_FIELDS:
        result["after"][field] = median([run["captures"]["after"]["status"][field] for run in runs])
        result["retainedDelta"][field] = median(
            [run["captures"]["after"]["status"][field] - run["captures"]["before"]["status"][field] for run in runs]
        )
    for field in ROLLUP_FIELDS:
        result["after"][field] = median([run["captures"]["after"]["smapsRollup"][field] for run in runs])
        result["retainedDelta"][field] = median(
            [
                run["captures"]["after"]["smapsRollup"][field]
                - run["captures"]["before"]["smapsRollup"][field]
                for run in runs
            ]
        )
    for field in ("virtKib", "resKib", "shrKib"):
        result["after"][field] = median([run["captures"]["after"]["top"][field] for run in runs])
    for field in ("allocations", "deallocations", "allocatedBytes", "deallocatedBytes", "retainedBytes"):
        result["allocationDelta"][field] = median(
            [
                json.loads(Path(run["summary"]).read_text())["afterAllocations"][field]
                - json.loads(Path(run["summary"]).read_text())["beforeAllocations"][field]
                for run in runs
            ]
        )
    return result


def run_suite(args: argparse.Namespace) -> int:
    binary = args.binary.resolve()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    models_json = output / "models.json"
    models_json.write_text('{"providers":{}}\n')
    reports = {}
    for mode in MODES:
        runs = []
        for index in range(1, args.repeat + 1):
            runs.append(run_one(binary, models_json, mode, output / mode / f"run-{index}"))
        reports[mode] = aggregate_runs(runs)
    document = {
        "schema": "rpi-v19-schema-probe-suite-v1",
        "binary": str(binary),
        "binarySha256": hash_file(binary),
        "binaryBytes": binary.stat().st_size,
        "cargoLock": str((args.cargo_lock or ROOT / "Cargo.lock").resolve()),
        "cargoLockSha256": hash_file(args.cargo_lock or ROOT / "Cargo.lock"),
        "sourceCommit": args.source_commit,
        "overlaySha256": args.overlay_sha256,
        "repeat": args.repeat,
        "modes": reports,
    }
    path = output / "suite-report.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"ok: schema probe report: {path}")
    return 0


def run_attribution(args: argparse.Namespace, stem: str) -> dict[str, object] | None:
    if args.attribution_dir is None:
        return None
    return load_attribution(args.attribution_dir / f"{stem}.json")


def compare_suites(args: argparse.Namespace) -> int:
    baseline_path = args.baseline.resolve()
    candidate_path = args.candidate.resolve()
    baseline = json.loads(baseline_path.read_text())
    candidate = json.loads(candidate_path.read_text())
    if baseline["repeat"] != candidate["repeat"]:
        raise ValueError("A/B reports use different repeat counts")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    result = {
        "schema": "rpi-v19-schema-probe-comparison-v1",
        "baseline": str(baseline_path),
        "candidate": str(candidate_path),
        "binaryBytesDelta": candidate["binaryBytes"] - baseline["binaryBytes"],
        "modes": {},
        "mappingRuns": [],
    }
    for mode in MODES:
        old = baseline["modes"][mode]
        new = candidate["modes"][mode]
        metrics = {}
        for field in ("VmRSS", "RssAnon", "RssFile", "RssShmem", "Pss", "resKib", "shrKib"):
            metrics[field] = {
                "baseline": old["after"][field],
                "candidate": new["after"][field],
                "delta": new["after"][field] - old["after"][field],
                "percent": (new["after"][field] / old["after"][field] * 100) if old["after"][field] else None,
            }
        result["modes"][mode] = {
            "after": metrics,
            "initializationCpuSecondsDelta": new["initializationCpuSecondsMedian"] - old["initializationCpuSecondsMedian"],
            "initializationNanosDelta": new["initializationNanosMedian"] - old["initializationNanosMedian"],
            "allocationDelta": new["allocationDelta"],
            "baselineAllocationDelta": old["allocationDelta"],
        }
        for index in range(baseline["repeat"]):
            old_run = json.loads(Path(old["runDetails"][index]).read_text())
            new_run = json.loads(Path(new["runDetails"][index]).read_text())
            mapping_path = output / f"{mode}-run-{index + 1}-mapping.json"
            mapping = compare_mappings(
                Path(old_run["captures"]["after"]["summary"]),
                Path(new_run["captures"]["after"]["summary"]),
                run_attribution(args, f"{mode}-run-{index + 1}"),
            )
            mapping_path.write_text(json.dumps(mapping, indent=2) + "\n")
            result["mappingRuns"].append({
                "mode": mode,
                "run": index + 1,
                "report": str(mapping_path),
                "attributionComplete": mapping.get("attributionComplete"),
            })
    if args.attribution_dir is not None:
        result["attributionComplete"] = all(
            run["attributionComplete"] for run in result["mappingRuns"]
        )
    path = output / "comparison.json"
    path.write_text(json.dumps(result, indent=2) + "\n")
    print(f"ok: schema probe comparison: {path}")
    return 0


def compare_replay(args: argparse.Namespace) -> int:
    baseline_path = args.baseline.resolve()
    candidate_path = args.candidate.resolve()
    baseline = json.loads(baseline_path.read_text())
    candidate = json.loads(candidate_path.read_text())

    def scenario(document: dict, path: Path) -> dict:
        matches = [report for report in document["reports"] if report["scenario"] == args.scenario]
        if len(matches) != 1:
            raise ValueError(f"{path} does not contain exactly one {args.scenario} report")
        samples = [sample for sample in matches[0]["memorySamples"] if sample["label"] == args.label]
        if not samples:
            raise ValueError(f"{path} has no {args.label} memory sample")
        return {"report": matches[0], "samples": samples}

    old = scenario(baseline, baseline_path)
    new = scenario(candidate, candidate_path)
    metrics = {}
    for field in (*STATUS_FIELDS, *ROLLUP_FIELDS, "virtKib", "resKib", "shrKib"):
        source = "top" if field.endswith("Kib") and field not in ROLLUP_FIELDS else "status"
        if field in ROLLUP_FIELDS:
            source = "smapsRollup"
        old_values = [sample["processes"][0][source][field] for sample in old["samples"]]
        new_values = [sample["processes"][0][source][field] for sample in new["samples"]]
        old_median = median(old_values)
        new_median = median(new_values)
        metrics[field] = {
            "baseline": old_median,
            "candidate": new_median,
            "delta": new_median - old_median,
            "percent": (new_median / old_median * 100) if old_median else None,
        }
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    mappings = []
    for index, (old_sample, new_sample) in enumerate(zip(old["samples"], new["samples"], strict=True), 1):
        report = compare_mappings(
            Path(old_sample["processes"][0]["summary"]),
            Path(new_sample["processes"][0]["summary"]),
            run_attribution(args, f"run-{index}"),
        )
        path = output / f"mapping-run-{index}.json"
        path.write_text(json.dumps(report, indent=2) + "\n")
        mappings.append({
            "run": index,
            "report": str(path),
            "budgetCheck": report.get("budgetCheck"),
            "budgetError": report.get("budgetError"),
            "attributionComplete": report.get("attributionComplete"),
        })
    old_report = json.loads(Path(old["report"]["report"]).read_text())
    new_report = json.loads(Path(new["report"]["report"]).read_text())
    res_reduction = 100 - metrics["VmRSS"]["percent"]
    document = {
        "schema": "rpi-v19-replay-memory-comparison-v1",
        "scenario": args.scenario,
        "label": args.label,
        "baseline": str(baseline_path),
        "candidate": str(candidate_path),
        "metrics": metrics,
        "resReductionPercent": res_reduction,
        "pass20Percent": res_reduction >= 20,
        "mappingRuns": mappings,
        "attributionComplete": (
            all(run["attributionComplete"] for run in mappings)
            if args.attribution_dir is not None
            else None
        ),
        "finalScreenSha256": {
            "baseline": old_report["finalScreenSha256"],
            "candidate": new_report["finalScreenSha256"],
            "equal": old_report["finalScreenSha256"] == new_report["finalScreenSha256"],
        },
        "terminalOutputSha256PerRun": {
            "baseline": old_report["terminalOutputSha256PerRun"],
            "candidate": new_report["terminalOutputSha256PerRun"],
        },
    }
    path = output / "comparison.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    print(f"ok: replay memory comparison: {path}; RES reduction {res_reduction:.2f}%")
    return 0 if document["pass20Percent"] else 1


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run")
    run.add_argument("--binary", type=Path, required=True)
    run.add_argument("--output-dir", type=Path, required=True)
    run.add_argument("--repeat", type=int, default=3)
    run.add_argument("--source-commit", required=True)
    run.add_argument("--overlay-sha256", required=True)
    run.add_argument("--cargo-lock", type=Path)
    compare = sub.add_parser("compare")
    compare.add_argument("--baseline", type=Path, required=True)
    compare.add_argument("--candidate", type=Path, required=True)
    compare.add_argument("--output", type=Path, required=True)
    compare.add_argument("--attribution-dir", type=Path)
    replay = sub.add_parser("compare-replay")
    replay.add_argument("--baseline", type=Path, required=True)
    replay.add_argument("--candidate", type=Path, required=True)
    replay.add_argument("--scenario", default="memory-empty-default")
    replay.add_argument("--label", default="startup")
    replay.add_argument("--output", type=Path, required=True)
    replay.add_argument("--attribution-dir", type=Path)
    args = parser.parse_args()
    if args.command == "run":
        if args.repeat < 1:
            raise ValueError("repeat must be positive")
        return run_suite(args)
    if args.command == "compare-replay":
        return compare_replay(args)
    return compare_suites(args)


if __name__ == "__main__":
    raise SystemExit(main())
