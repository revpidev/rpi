# D-028：内置模型目录管线与 provider 注册表骨架的 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W4 阶段 1）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）
- 原文约定：
  - 「模型目录为**生成物**：`build.rs` 从 models.dev 数据生成内置 catalog（对齐 `generate-models.ts` 的修正规则……正式数据管线在 T13/T14）」
  - 「38 个内置工厂（需求 §5.3 清单）」「应用可按需只注册子集（feature flags 等价 tree-shake）」

## 实际实现与偏离原因

1. **目录管线形态**：上游构建期生成 TS 字面量（`models.generated.ts`）；Rust 侧
   改为 `build.rs` 扫描 vendored `src/providers/data/*.json`（37 份 + `.manifest.json`，
   逐字节复制自上游 `providers/data/`，sha256 与 manifest 一致，有测试兜底），
   生成 `include_str!` 表，运行时 serde 惰性解析（`OnceLock`，首次访问解析一次）。
   理由：1153 条模型字面量 codegen 的编译期开销无收益，数据内容二者一致
   （`tests/model_catalog.rs` 对上游 JSON 逐字段对拍）。
2. **修正规则不在 Rust 侧重放**：上游 `*.models.ts` 经验证是纯 re-export
   （`flattenModelCatalog`，37 份全部 8 行同形），compat delta /
   thinkingLevelMap / 定价 tiers / Kimi 隐含成本等修正全部由
   `generate-models.ts` 在生成期烘焙进 JSON。Rust 侧照单解析即可，语义与上游一致。
3. **注册表骨架**：`providers.rs` 以 `BUILTIN_PROVIDERS` 静态 spec 表列出全部
   38 工厂 id（上游 `builtinProviders()` 注册序逐条核对）；工厂槽
   `factory: Option<ProviderFactory>`，W4 后续波次逐个填充。阶段内
   `builtin_providers()` 只产出已移植子集（阶段 1 为空）——与上游「一次构造
   38 个」有**阶段性**差异，W4 完成后对齐。
4. **按需注册子集不用 cargo feature flags**：38 个 feature 是清单噪声；子集化由
   per-id factory 槽 + `get_builtin_provider_spec` 完成，未引用工厂由链接期
   死代码消除自然 tree-shake。
5. **generatedAt 手写 ISO-8601 解析**（`generated.rs`，days-from-civil），不为此
   引入 chrono；`Date.parse` 失败语义（`NaN`→`undefined`）镜像为 `None`。
6. **备注（非偏离）**：任务书称目录数据「30 份」，实际钉死版上游为 37 份
   JSON + 隐藏文件 `.manifest.json`，以钉死 commit 实际内容为准。

## 影响面

无（纯内部）：catalog API 为新增，线格式与 session/RPC 不受影响；vendored
数据与上游逐字段一致（有对拍测试）。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（38 工厂、模型目录、按需注册三条）
- **回写日期**：2026-08-06
- **ADR**：不需要
