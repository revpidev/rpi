# D-036：cross-provider-handoff live 测试不移植（意图由纯函数测试覆盖）

- **状态**：已回写
- **关联任务**：T13 W6-B
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.6（横切模块）、`coding-standards.md` §12.2（测试意图移植：external/pi 中 vitest 用例意图移植为同名 Rust 测试）与 §12.4（所有非 live 测试用 faux provider / fake stream，禁止打真实网络）

## 实际实现与偏离原因

上游 `packages/ai/test/cross-provider-handoff.test.ts` 是 live 测试：`describe.skipIf(!hasAnyApiKey())`，须用真实 provider API keys 在 `beforeAll` 中为每个 provider/model 生成 context（真实 tool call id 与 thinking 块），再交叉拼接喂给目标 provider 断言成功。pir 不移植该 live 测试，其意图（一个 provider/model 生成的上下文可被另一 provider 消费：tool call id 格式、thinking 块转换、消息格式兼容）由以下纯函数测试覆盖：

- `utils/transform_messages.rs` 全规则：非 vision 图片占位符、thinking 跨模型转文本、redacted 丢弃、thoughtSignature 剥离、toolCallId 归一 + toolResult 回填、孤儿 tool call 合成 error 结果、error/aborted 消息不回放、多调用仅缺结果者合成
- 六个适配器的 normalize 回调：anthropic / bedrock `[^a-zA-Z0-9_-]→_` + 64 上限、google 按模型条件化、mistral 专用 normalizer、openai-completions pipe 拆分 + 40 上限 + shortHash 回填、openai-responses pipe 拆分 + `fc_` 前缀 + shortHash

另：上游 `transform-messages.ts:71-73` 的 null/undefined content 归一化步骤在 Rust 侧无运行时对应（Message 类型不能持 null，`content: null` 由 serde 边界容忍为空，`types.rs::null_default`）——T03 已落地，此处一并记录，与 D-006（解析层差异）同范畴。

## 影响面

无（纯内部测试基建差异；协议 / session 格式 / 扩展 API / TUI 行为均不受影响）

## 处置

- **回写位置**：`docs/02-design.md` §3.6（Rust 落地注记）
- **回写日期**：2026-08-06
- **ADR**：不需要
