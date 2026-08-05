# Task for reviewer

[Read from: /home/leven/develop/ai/pir/plan.md, /home/leven/develop/ai/pir/progress.md]

审查 /home/leven/develop/ai/pir/crates/pir-tui/src/tui.rs 第 1-2600 行（TUI 核心结构、SharedComponent 与重入队列、渲染管线 do_render 前半、VirtualTerminal 差分）。这是对上游 external/pi/packages/tui/src/tui.ts（commit 2efa728d）的 Rust 移植。审查重点：1) 逻辑 bug、边界条件错误（如差分算法、全量回退条件、16ms 节流）；2) panic/死锁风险（Mutex 锁顺序、reentrant）；3) 与上游语义不一致的地方（可对照 external/pi/packages/tui/src/tui.ts）；4) 未处理的错误路径。输出：按严重度（高/中/低/提示）列出发现，每条带 文件:行号、问题描述、建议。不要修改任何文件，只读审查。

---
**Output:**
Write your findings to exactly this path: /tmp/review-tui1.md
This path is authoritative for this run.
Ignore any other output filename or output path mentioned elsewhere, including output destinations in the base agent prompt, system prompt, or task instructions.

## Acceptance Contract
Acceptance level: attested
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Return concrete findings with file paths and severity when applicable

Required evidence: review-findings, residual-risks

Finish with a fenced JSON block tagged `acceptance-report` in this shape:
Use empty arrays when no items apply; array fields contain strings unless object entries are shown.
`criteriaSatisfied[].status` must be exactly one of: satisfied, not-satisfied, not-applicable.
`commandsRun[].result` must be exactly one of: passed, failed, not-run.
`manualNotes` and `notes` are optional strings; an empty string means no note and does not satisfy `manual-notes` evidence.
```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "specific proof"
    }
  ],
  "changedFiles": [
    "src/file.ts"
  ],
  "testsAddedOrUpdated": [
    "test/file.test.ts"
  ],
  "commandsRun": [
    {
      "command": "command",
      "result": "passed",
      "summary": "short result"
    }
  ],
  "validationOutput": [
    "validation output or concise summary"
  ],
  "residualRisks": [
    "none"
  ],
  "noStagedFiles": true,
  "diffSummary": "short description of the diff",
  "reviewFindings": [
    "blocker: file.ts:12 - issue found, or no blockers"
  ],
  "manualNotes": "anything else the parent should know"
}
```