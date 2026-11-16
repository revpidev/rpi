#!/usr/bin/env python3
"""Run and summarize deterministic release-profile V19-00 replay scenarios."""

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
import threading
import time
from pathlib import Path

import proc_sample

ROOT = Path(__file__).resolve().parents[2]
BUNDLES = ROOT / "fixtures" / "v019-benchmark" / "bundles"
FRAME_FIELDS = (
    "layout_ns",
    "paint_ns",
    "write_compute_ns",
    "pure_compute_ns",
    "terminal_io_ns",
    "output_bytes",
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def median(values: list[float]) -> float:
    if not values:
        raise ValueError("cannot compute a median of zero runs")
    return float(statistics.median(values))


def optional_median(values: list[float | None]) -> float | None:
    present = [value for value in values if value is not None]
    return median(present) if present else None


def p95(values: list[int]) -> int:
    if not values:
        raise ValueError("p95 requires samples")
    values = sorted(values)
    return values[max(0, (len(values) * 95 + 99) // 100 - 1)]


def task_jiffies(pid: int) -> dict[str, dict[str, object]]:
    result: dict[str, dict[str, object]] = {}
    try:
        tasks = list(Path(f"/proc/{pid}/task").iterdir())
    except OSError:
        return result
    for task in tasks:
        try:
            raw = (task / "stat").read_text()
            close = raw.rindex(")")
            fields = raw[close + 2 :].split()
            result[task.name] = {
                "comm": raw[raw.index("(") + 1 : close],
                "jiffies": int(fields[11]) + int(fields[12]),
            }
        except (OSError, ValueError, IndexError):
            continue
    return result


class ProcessMonitor:
    def __init__(
        self, process: subprocess.Popen[bytes], sample_dir: Path | None = None
    ) -> None:
        self.process = process
        self.sample_dir = sample_dir
        self.first: dict[str, dict[str, object]] = {}
        self.last: dict[str, dict[str, object]] = {}
        self.samples = 0
        self.captured: set[str] = set()
        self.error: str | None = None
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _sample(self) -> None:
        current = task_jiffies(self.process.pid)
        if not current:
            return
        if not self.first:
            self.first = current
        self.last = current
        self.samples += 1

    def _capture_samples(self) -> None:
        if self.sample_dir is None or not self.sample_dir.is_dir():
            return
        for marker in sorted(self.sample_dir.glob("*.marker")):
            name = marker.stem
            if name in self.captured:
                continue
            try:
                proc_sample.capture(self.process.pid, name, self.sample_dir / name)
            except Exception as error:  # preserve the failure and unblock the child
                self.error = f"capture {name}: {error}"
            finally:
                self.captured.add(name)
                (self.sample_dir / f"{name}.ack").write_text(name)

    def _run(self) -> None:
        self._sample()
        self._capture_samples()
        while self.process.poll() is None:
            time.sleep(0.005)
            self._sample()
            self._capture_samples()

    def finish(self) -> dict[str, object]:
        self.thread.join(timeout=30)
        self._capture_samples()
        if self.error is not None:
            raise ValueError(self.error)
        if self.thread.is_alive():
            raise ValueError("process monitor did not stop")
        ticks = os.sysconf("SC_CLK_TCK")
        threads = []
        for tid, last in sorted(self.last.items()):
            first = self.first.get(tid, last)
            elapsed = int(last["jiffies"]) - int(first["jiffies"])
            threads.append(
                {
                    "tid": tid,
                    "comm": last["comm"],
                    "cpuSeconds": elapsed / ticks,
                    "firstJiffies": first["jiffies"],
                    "lastJiffies": last["jiffies"],
                }
            )
        return {"samples": self.samples, "clockTicksPerSecond": ticks, "threads": threads}


def prepare_agent(bundle: dict[str, object], sandbox: Path) -> tuple[Path, Path]:
    agent = sandbox / "agent"
    cwd = sandbox / "cwd"
    home = sandbox / "home"
    agent.mkdir(parents=True, exist_ok=True)
    cwd.mkdir(exist_ok=True)
    home.mkdir(exist_ok=True)
    (agent / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "custom": {
                        "baseUrl": "https://example.invalid/v1",
                        "api": "openai-completions",
                        "apiKey": "replay-not-a-credential",
                        "models": [{"id": "m1", "contextWindow": 200000}],
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    settings = dict(bundle.get("settings", {}))
    settings["enableInstallTelemetry"] = False
    (agent / "settings.json").write_text(json.dumps(settings), encoding="utf-8")

    extension_kind = bundle.get("extensions", "none")
    if extension_kind == "default":
        extension = agent / "extensions" / "rpi-statusline"
        extension.mkdir(parents=True, exist_ok=True)
        manifest = ROOT / "crates" / "rpi-ext-statusline" / "rpi-extension.json"
        library = ROOT / "target" / "release" / "librpi_ext_statusline.so"
        if not library.is_file():
            raise FileNotFoundError(f"build the release statusline extension first: {library}")
        (extension / "rpi-extension.json").write_bytes(manifest.read_bytes())
        (extension / "librpi_ext_statusline.so").write_bytes(library.read_bytes())
    elif extension_kind == "cumulative":
        extension = agent / "extensions" / "rpi-v19-cumulative"
        extension.mkdir(parents=True, exist_ok=True)
        library = ROOT / "target" / "release" / "librpi_test_native_plugin.so"
        if not library.is_file():
            raise FileNotFoundError(f"build the release native fixture first: {library}")
        (extension / "librpi_test_native_plugin.so").write_bytes(library.read_bytes())
        (extension / "rpi-extension.json").write_text(
            json.dumps(
                {
                    "name": "rpi-v19-cumulative-fixture",
                    "version": "0.1.0",
                    "native": "librpi_test_native_plugin.so",
                    "capabilities": ["events"],
                    "rpiAbi": 1,
                }
            ),
            encoding="utf-8",
        )
    elif extension_kind != "none":
        raise ValueError(f"unknown extension scenario {extension_kind}")
    return agent, cwd


def validate_frames(path: Path) -> list[dict[str, object]]:
    if not path.is_file() or path.stat().st_size == 0:
        raise ValueError(f"missing or empty frame log {path}")
    frames = []
    for line_number, line in enumerate(path.read_text().splitlines(), 1):
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid frame JSON at line {line_number}: {error}") from error
        if event.get("type") != "frame":
            raise ValueError(f"unexpected event at line {line_number}")
        if event.get("sequence") != line_number:
            raise ValueError(f"non-contiguous frame sequence at line {line_number}")
        for field in FRAME_FIELDS:
            if not isinstance(event.get(field), int) or event[field] < 0:
                raise ValueError(f"invalid {field} at line {line_number}")
        if not isinstance(event.get("output_hash"), str):
            raise ValueError(f"missing output hash at line {line_number}")
        frames.append(event)
    return frames


def run_one(
    binary: Path,
    bundle_path: Path,
    bundle: dict[str, object],
    input_hash: str,
    run_dir: Path,
    sandbox: Path,
    arena: str | None,
) -> dict[str, object]:
    run_dir.mkdir(parents=True)
    agent, cwd = prepare_agent(bundle, sandbox)
    frame_path = run_dir / "frames.jsonl"
    driver_summary = run_dir / "driver-summary.json"
    terminal_output = run_dir / "terminal.bytes"
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(run_dir / "home"),
        "LANG": "C.UTF-8",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "RPI_OFFLINE": "1",
        # Local-only model availability; replay never opens a provider stream.
        "KIMI_API_KEY": "replay-not-a-credential",
        "ZAI_API_KEY": "replay-not-a-credential",
        "RPI_V19_DISABLE_ADAPTIVE_BACKPRESSURE": "1",
        "RPI_V19_BENCHMARK_LOG": str(frame_path),
    }
    if arena is not None:
        env["MALLOC_ARENA_MAX"] = arena
    sample_dir = run_dir / "samples"
    if any(event.get("kind") == "sample" for event in bundle["events"]):
        env["RPI_V19_SAMPLE_DIR"] = str(sample_dir)
    if bundle.get("extensions") == "cumulative":
        env["RPI_V19_CUMULATIVE_FIXTURE"] = "1"
        env["RPI_V19_CUMULATIVE_COUNT_FILE"] = str(run_dir / "cumulative-count.txt")

    command = [
        str(binary),
        "--bundle",
        str(bundle_path),
        "--agent-dir",
        str(agent),
        "--cwd",
        str(cwd),
        "--summary",
        str(driver_summary),
        "--terminal-output",
        str(terminal_output),
        "--input-sha256",
        input_hash,
    ]
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    monitor = ProcessMonitor(process, sample_dir)
    monitor.thread.start()
    try:
        stdout, stderr = process.communicate(timeout=600)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise TimeoutError(f"replay timed out: {bundle['scenario']}")
    wall = time.monotonic() - started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    process_monitor = monitor.finish()
    (run_dir / "stdout.log").write_bytes(stdout)
    (run_dir / "stderr.log").write_bytes(stderr)
    if process.returncode != 0:
        raise RuntimeError(
            f"replay exited {process.returncode}: {stderr.decode('utf-8', 'replace')[-4000:]}"
        )
    if not driver_summary.is_file():
        raise ValueError("replay exited successfully without a summary")
    summary = json.loads(driver_summary.read_text())
    frames = validate_frames(frame_path)
    if summary["frames"] != len(frames):
        raise ValueError("driver/frame-log count mismatch")
    if bundle.get("extensions") == "cumulative":
        count_path = run_dir / "cumulative-count.txt"
        if not count_path.is_file() or int(count_path.read_text()) != summary["inputDeltas"]:
            raise ValueError("cumulative extension did not receive every provider delta")
    for name in summary.get("samples", []):
        if not (sample_dir / name / "capture.json").is_file():
            raise ValueError(f"missing memory capture for {name}")
    if summary.get("samples"):
        summary["memoryCaptures"] = [
            str(sample_dir / name / "capture.json") for name in summary["samples"]
        ]

    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    pure_values = [int(frame["pure_compute_ns"]) for frame in frames]
    intervals = [
        int(frame["frame_interval_ns"])
        for frame in frames
        if isinstance(frame.get("frame_interval_ns"), int)
    ]
    input_latencies = [int(value) for value in summary["inputLatenciesNs"]]
    statusline_cpu = sum(
        float(thread["cpuSeconds"])
        for thread in process_monitor["threads"]
        if thread["comm"] == "rpi-statusline"
    )
    summary.update(
        {
            "command": command,
            "statuslineWorkerCpuSeconds": statusline_cpu,
            "wallSeconds": wall,
            "processCpuSeconds": cpu,
            "processCpuWallRatio": cpu / wall,
            "cpuPerRenderedFrameSeconds": cpu / len(frames),
            "cpuPerInputDeltaSeconds": cpu / max(1, int(summary["inputDeltas"])),
            "pureComputeP95Ns": p95(pure_values),
            "frameIntervalP95Ns": p95(intervals),
            "inputFirstVisibleP95Ns": p95(input_latencies) if input_latencies else None,
            "terminalIoP95Ns": p95([int(frame["terminal_io_ns"]) for frame in frames]),
            "terminalSimulatorCpuSeconds": sum(
                int(frame["terminal_io_ns"]) for frame in frames
            )
            / 1_000_000_000,
            "terminalSimulator": "rpi-tui VirtualTerminal (in-process; terminal_io_ns)",
            "processMonitor": process_monitor,
            "frameHashes": [frame["output_hash"] for frame in frames],
        }
    )
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def aggregate(scenario: str, input_hash: str, runs: list[dict[str, object]]) -> dict[str, object]:
    hashes = {run["finalScreenSha256"] for run in runs}
    if len(hashes) != 1 or len({tuple(run["frameHashes"]) for run in runs}) != 1:
        raise ValueError("same input/build produced different rendered output")
    all_pure = [
        int(frame["pure_compute_ns"])
        for run in runs
        for frame in validate_frames(Path(str(run["frameLog"])))
    ]
    report = {
        "schema": "rpi-v19-benchmark-report-v1",
        "scenario": scenario,
        "inputSha256": input_hash,
        "runs": len(runs),
        "processCpuSecondsMedian": median([float(run["processCpuSeconds"]) for run in runs]),
        "wallSecondsMedian": median([float(run["wallSeconds"]) for run in runs]),
        "processCpuWallRatioMedian": median(
            [float(run["processCpuWallRatio"]) for run in runs]
        ),
        "cpuPerRenderedFrameSecondsMedian": median(
            [float(run["cpuPerRenderedFrameSeconds"]) for run in runs]
        ),
        "cpuPerInputDeltaSecondsMedian": median(
            [float(run["cpuPerInputDeltaSeconds"]) for run in runs]
        ),
        "pureComputeP95Ns": p95(all_pure),
        "pureComputeRunP95NsMedian": median([float(run["pureComputeP95Ns"]) for run in runs]),
        "frameIntervalP95NsMedian": median(
            [float(run["frameIntervalP95Ns"]) for run in runs]
        ),
        "inputFirstVisibleP95NsMedian": optional_median(
            [
                float(run["inputFirstVisibleP95Ns"])
                if run["inputFirstVisibleP95Ns"] is not None
                else None
                for run in runs
            ]
        ),
        "terminalSimulatorCpuSecondsMedian": median(
            [float(run["terminalSimulatorCpuSeconds"]) for run in runs]
        ),
        "finalScreenSha256": next(iter(hashes)),
        "terminalOutputSha256PerRun": [run["terminalOutputSha256"] for run in runs],
        "runSummaries": [str(Path(str(run["frameLog"])).parent / "summary.json") for run in runs],
        "frameLogs": [run["frameLog"] for run in runs],
        "terminalOutputs": [run["terminalOutput"] for run in runs],
    }
    return report


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--arena")
    parser.add_argument("--input-sha256", help="negative-test override")
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    binary = args.binary.resolve()
    bundle_path = args.bundle.resolve()
    bundle_bytes = bundle_path.read_bytes()
    bundle = json.loads(bundle_bytes)
    expected_hash = sha256(bundle_bytes)
    manifest_path = BUNDLES / "manifest.json"
    if bundle_path.is_relative_to(BUNDLES):
        manifest = json.loads(manifest_path.read_text())
        entry = manifest["bundles"].get(bundle_path.name)
        if entry is None or entry["sha256"] != expected_hash or entry["bytes"] != len(bundle_bytes):
            raise ValueError("bundle is absent from or stale in manifest.json")
    supplied_hash = args.input_sha256 or expected_hash
    # The runner-level hash check is intentional: the binary repeats it before
    # executing any event, so an incorrect comparison fails closed.
    if args.input_sha256 is not None:
        expected_hash = supplied_hash

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    runs = []
    for index in range(args.repeat):
        runs.append(
            run_one(
                binary,
                bundle_path,
                bundle,
                supplied_hash,
                output / f"run-{index + 1}",
                output / "sandbox",
                args.arena,
            )
        )
    report = aggregate(str(bundle["scenario"]), expected_hash, runs)
    report_path = output / "report.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    print(f"report: {report_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
