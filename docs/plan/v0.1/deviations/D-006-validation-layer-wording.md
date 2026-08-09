# D-006：校验/解析层差异（jsonschema、models.json serde、错误措辞）

- **状态**：已回写
- **关联任务**：T03
- **级别**：实现细节偏离
- **发现日期**：2026-07-29

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §5.5（校验条目）、`docs/02-design.md`
  §3.6（validation.rs「TypeBox/JSON-Schema 双路径」）
- 原文约定：工具参数校验双路径（TypeBox symbol 前奏 + Compile 校验 / 纯
  JSON-Schema 递归强转）；models.json 用 TypeBox schema 校验（上游
  `coding-agent/src/core/model-config.ts`）。

## 实际实现与偏离原因

1. 工具参数校验：`jsonschema` crate 单路径 + 宽松类型强转层。Rust 无 TypeBox，
   「TypeBox symbol 前奏」路径无对应物；校验失败措辞来自 jsonschema（路径/
   关键字文案与 TypeBox 不同）。强转表（null→0、"123"→123、bool→1/0 等）与
   组合递归规则按上游逐条移植并有测试锚点。
2. models.json 校验（`rpi-ai/src/models_json.rs`）：serde 派生 + 手工
   `minLength:1`/`oauth` 字面量 pass 替代 TypeBox；错误路径格式对齐
   （`providers.<id>.models.<n>.id`），措辞不同。上游 per-API compat 三选一
   union 用平铺 `ModelCompat` 验证（D-002 的同一合并结构），比 TypeBox union
   宽松（不拒绝跨 API 字段混用）。
3. thinking signature 重放的 `JSON.parse` 用 `serde_json`，解析失败文案为
   serde 错误文本。

## 影响面

无（错误文案不进对拍锚点；通过校验后的数据形状与上游一致）。

## 处置

- **回写位置**：`docs/01-requirements.md` §5.5（校验条目注记）；
  `docs/02-design.md` §3.6（validation.rs 行）
- **回写日期**：2026-07-30
- **ADR**：不需要
