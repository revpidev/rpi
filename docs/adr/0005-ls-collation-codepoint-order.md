# ADR-0005：ls 工具排序采用码位比较——ICU `localeCompare` 不引入

- **状态**：已采纳
- **日期**：2026-08-07
- **关联**：[ADR-0003](./0003-coverage-review-scope-decisions.md) §2（grep/find/ls 原生实现）、[`01-requirements.md`](../01-requirements.md) §4.5、T14 偏离 D-039 第 1 条

## 背景

上游 ls 工具排序为 `entries.sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()))`
（`packages/coding-agent/src/core/tools/ls.ts:150` @ 0.82.1）。`localeCompare` 走宿主
ICU 排序表，其结果依赖 ICU 版本与默认 locale，本身不具备跨环境确定性。

T14（W1）按 ADR-0003 §2 以 Rust 原生实现 ls，若追求字面一致需引入 icu4x collator
（依赖体积大，且仍无法复刻「宿主默认 locale」这一环境变量）。T14 任务文件将 ls 的
排序列为对拍契约项，因此该差异按流程需立 ADR。

## 决策

ls 排序采用 **Unicode 码位比较**（小写化后按码位升序），不引入 icu4x：

- 对纯 ASCII 字母数字文件名，码位比较与 ICU root collation 结果一致，主路径行为不变。
- 差异仅出现在标点/下划线与字母混排的名称（如 `_a` 与 `Z` 的相对顺序），上游无测试锚点。
- 上游结果本身随宿主 ICU/locale 漂移，码位比较反而是跨环境确定、可测试的行为。

## 后果

- D-039 第 1 条按行为级偏离处理，引用本 ADR；`01-requirements.md` §4.5 的排序注记同步。
- 若未来确有 ICU 排序需求（如扩展生态反馈），评估 icu4x collator 后另立 ADR 替换，
  并补混排名称的对拍测试。
