# Task for reviewer

[Read from: /home/leven/develop/ai/pir/plan.md, /home/leven/develop/ai/pir/progress.md]

审查 /home/leven/develop/ai/pir/crates/pir-tui/src/keys.rs（2445 行，Kitty 键盘协议 flags=7 解析 + legacy CSI 全表）与 src/keybindings.rs（1040 行，31 条键位定义 + JSON 配置）。这是对上游 external/pi/packages/tui/src/keys.ts 和 keybindings.ts（commit 2efa728d）的 Rust 移植。审查重点：1) Kitty flags 各修饰键位组合解析正确性（对照 external/pi/packages/tui/src/keys.ts 逐字段）；2) legacy CSI 序列表完整性；3) keybindings JSON 反序列化/保序/解绑语义；4) 边界（溢出、非法输入）。输出：按严重度（高/中/低/提示）列出发现，每条带 文件:行号、问题描述、建议。不要修改任何文件，只读审查。

---
**Output:**
Write your findings to exactly this path: /tmp/review-keys.md
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