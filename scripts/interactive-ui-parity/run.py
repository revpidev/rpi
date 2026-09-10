#!/usr/bin/env python3
"""V14-22 dual-carrier interactive-UI parity runner (R-U7.4 / G11 item 2).

Builds both fixture guests, runs the Rust driver over the JSONL corpus plus a
seeded fuzz batch, aggregates the driver's per-scenario ``*.diff.json``
verdicts and writes
``fixtures/generated/interactive-ui-parity/parity-report.md``. Non-zero exit
on any difference, load failure or missing fixture.

Usage (from the repo root):
    python3 scripts/interactive-ui-parity/run.py
    python3 scripts/interactive-ui-parity/run.py --report <path> --fuzz 32 --seed 7
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
CORPUS = ROOT / "scripts" / "interactive-ui-parity" / "corpus"
OUT_DIR = ROOT / "fixtures" / "generated" / "interactive-ui-parity"
WASM_FILE = ROOT / "examples" / "wasm-extension" / "target" / "wasm32-unknown-unknown" / "release" / "rpi_wasm_extension_example.wasm"


def run(cmd: list[str], cwd: Path | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
    print(f"$ {' '.join(cmd)}")
    return subprocess.run(cmd, cwd=cwd, env=env, text=True, capture_output=True)


def build_fixtures() -> list[str]:
    """Build the native cdylib and the wasm32 guest; returns log lines."""
    log: list[str] = []

    native = run(["cargo", "build", "-p", "rpi-test-native-plugin"], cwd=ROOT)
    log.append(f"native fixture: exit {native.returncode}")
    if native.returncode != 0:
        log.append(native.stderr.strip())
        raise RuntimeError("native fixture build failed")

    wasm = run(
        ["cargo", "build", "--target", "wasm32-unknown-unknown", "--release"],
        cwd=ROOT / "examples" / "wasm-extension",
    )
    if wasm.returncode != 0:
        # System toolchains may lack the wasm32 target; retry with a
        # user-local rustup home when one provides it.
        local_rustup = Path.home() / ".rustup"
        if (local_rustup / "toolchains").is_dir():
            retry_env = dict(os.environ)
            retry_env["RUSTUP_HOME"] = str(local_rustup)
            retry_env["RUSTUP_TOOLCHAIN"] = "stable"
            wasm = run(
                ["cargo", "build", "--target", "wasm32-unknown-unknown", "--release"],
                cwd=ROOT / "examples" / "wasm-extension",
                env=retry_env,
            )
    log.append(f"wasm fixture: exit {wasm.returncode}")
    if wasm.returncode != 0:
        log.append(wasm.stderr.strip()[-4000:])
        raise RuntimeError("wasm fixture build failed (install wasm32-unknown-unknown)")
    if not WASM_FILE.is_file():
        raise RuntimeError(f"wasm fixture artifact missing after build: {WASM_FILE}")
    log.append(f"wasm fixture: {WASM_FILE.relative_to(ROOT)}")
    return log


def run_driver(out_dir: Path, fuzz: int, seed: int) -> tuple[int, str]:
    cmd = [
        "cargo",
        "run",
        "-q",
        "-p",
        "rpi-test-support",
        "--bin",
        "interactive-ui-parity",
        "--",
        "--fixture",
        str(CORPUS),
        "--out",
        str(out_dir),
        "--fuzz",
        str(fuzz),
        "--seed",
        str(seed),
    ]
    result = run(cmd, cwd=ROOT)
    output = result.stdout + result.stderr
    return result.returncode, output


def scenario_rows(out_dir: Path) -> list[dict]:
    rows = []
    for path in sorted(out_dir.glob("*.diff.json")):
        data = json.loads(path.read_text())
        native_frames = len(data["native"]["frames"])
        wasm_frames = len(data["wasm"]["frames"])
        rows.append(
            {
                "scenario": data["scenario"],
                "native_frames": native_frames,
                "wasm_frames": wasm_frames,
                "matched": bool(data["matched"]),
                "constraint": data.get("documentedConstraint"),
                "terminal_native": data["native"]["terminal"],
                "terminal_wasm": data["wasm"]["terminal"],
            }
        )
    return rows


def write_report(report: Path, log: list[str], exit_code: int, output: str, rows: list[dict], fuzz: int, seed: int) -> None:
    failures = [row for row in rows if not row["matched"]]
    lines = [
        "# interactive-ui-parity 报告（V14-22 C2；R-U7.4 / G11 第 2 条）",
        "",
        f"- 语料：`scripts/interactive-ui-parity/corpus/`（{len(rows)} 个场景，JSONL 事件脚本）",
        f"- fuzz：{fuzz} 场景（seed `{seed}`，固定可重放；§4.5）",
        f"- 结论：{'通过（零差异）' if exit_code == 0 and not failures else '不通过'}",
        "",
        "## 构建",
        "",
        "```",
        *log,
        "```",
        "",
        "## 逐场景（frames = `{lines,cursor?,done?}` 数）",
        "",
        "| 场景 | native 帧 | wasm 帧 | 一致 | 文档化载体约束 |",
        "|------|-----------|---------|------|----------------|",
    ]
    for row in rows:
        constraint = row["constraint"] or "—"
        lines.append(
            f"| `{row['scenario']}` | {row['native_frames']} | {row['wasm_frames']} | "
            f"{'是' if row['matched'] else '**否**'} | {constraint} |"
        )
    lines += [
        "",
        "> 文档化约束：wasm 载体的帧总预算固定上限 512 KiB（native 保持 guest 请求值），"
        "属设计 §4.4 已定执行约束，不占偏离编号（任务 §2.1）。",
        "",
        "## 驱动输出",
        "",
        "```",
        *output.strip().splitlines()[-40:],
        "```",
        "",
        "## 产物",
        "",
        "- `*.native.json` / `*.wasm.json`：各载体完整转录（frames/terminal/toolResult/mountOptions）。",
        "- `*.diff.json`：逐场景一致性判定（parity 投影 + 文档化约束标记）。",
        "",
    ]
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text("\n".join(lines))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", type=Path, default=OUT_DIR / "parity-report.md")
    parser.add_argument("--fuzz", type=int, default=24)
    parser.add_argument("--seed", type=int, default=20260910)
    parser.add_argument("--no-build", action="store_true", help="use existing fixture artifacts")
    args = parser.parse_args()

    log: list[str] = []
    try:
        if not args.no_build:
            log = build_fixtures()
        exit_code, output = run_driver(OUT_DIR, args.fuzz, args.seed)
    except RuntimeError as error:
        print(f"interactive-ui-parity: {error}", file=sys.stderr)
        return 1
    except FileNotFoundError as error:
        print(f"interactive-ui-parity: {error}", file=sys.stderr)
        return 1

    rows = scenario_rows(OUT_DIR)
    write_report(args.report, log, exit_code, output, rows, args.fuzz, args.seed)
    print(output.strip())
    print(f"report: {args.report}")
    return 0 if exit_code == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
