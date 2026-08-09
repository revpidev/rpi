# T26：新 provider 与模型目录更新

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T20、T21
- **上游对照**：`c1019d920`（Baseten）、`c03d78bdc`（Qwen Token Plan Individual）、`2f7f75a20`（qwen3.8-max 更名 #7670）、`14cc26e86`（Copilot policy fallback #7672）、`720f0e8ee`（Copilot Grok 4.5 路由 #7560）、`a688e257c`（Fireworks Kimi K3 #7199）、`b9497c8c1`（GLM 5.2 #7676）、`b889a0ce3`（GPT-5.6 降价）、`71f6c25c3`/`05558a792`/`c0947e644`；`scripts/generate-models.ts`（`processBasetenModels()`、`assertExactModelIds()`）；测试：`baseten-models.test.ts`、`generate-models-strict.test.ts`
- **需求章节**：v0.11 需求 R2.7；设计 §2.6
- **预估**：0.35 人月

---

## 目标

新增 Baseten 与 Qwen Token Plan Individual 两个 provider，模型目录管线同步上游修正。

## 范围

### In

- **Baseten**：OpenAI-compatible 工厂；`BASETEN_API_KEY`；baseUrl `https://inference.baseten.co/v1`；`thinkingFormat: "baseten"`（toggle）/ `"openai"`（effort）按模型能力自动选择；compat `chatTemplateArgs`；跳过 `status == "deprecated"` 模型
- **Qwen Token Plan Individual**：白名单 7 模型（deepseek-v4-flash-0731/deepseek-v4-pro/glm-5.2/qwen3.6-flash/qwen3.6-plus/qwen3.7-max/qwen3.7-plus/qwen3.8-max）；共享 `QWEN_TOKEN_PLAN_API_KEY` 与国际端点
- **目录生成管线**（`scripts/refresh-model-catalog.sh` + vendored JSON 生成）同步：`processBasetenModels` 等价逻辑、白名单 `assertExactModelIds` 严格对拍、`thinkingFormat: "baseten"` 与四个新 compat 字段透传、long-context 定价 `roundCost`、image 目录新增 qwen-image-3/3-pro
- **目录修正**：`qwen3.8-max-preview` → `qwen3.8-max`；Copilot Individual 端点 picker 全 false 回退 `policy.state == "enabled"`（`parseAvailableCopilotModelIds` 的 `allowPolicyFallback`）；Copilot Grok 4.5 改 Responses 路由；Fireworks Kimi K3 改 OpenAI-compatible + reasoning-effort + deferred tools；GLM 5.2 不发 `prompt_cache_retention` + session affinity；GPT-5.6 Terra/Luna 价格覆盖；Groq Qwen 推理 override 指向 `qwen/qwen3.6-27b`；OpenCode Go 显示名；`claude-opus-5-fast` 移出 adaptive-thinking 列表

### Out

- compat 字段的适配器消费逻辑（T20 已落地）
- 上游后续模型目录日常漂移（属既有目录刷新流程，非本任务）

## 开发要点

- 先更新目录生成管线并重跑（`refresh-model-catalog.sh`），diff vendored JSON 审查后再接 provider 工厂
- 白名单严格对拍（`assertExactModelIds`）要进 CI/测试：上游目录漂移时显式失败而非静默
- Copilot policy fallback 只在 Individual 端点生效，其他端点保持严格 picker 语义——边界用例锁死

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] Baseten 工厂 + thinkingFormat 自动选择 + deprecated 过滤 golden（移植 `baseten-models.test.ts`）
- [ ] Qwen Token Plan Individual 白名单 7 模型 + 共享凭据 + 严格对拍失败路径（移植 `generate-models-strict.test.ts`）
- [ ] 目录修正八项各配断言（更名/policy fallback/路由/GLM affinity/价格覆盖等）
- [ ] vendored JSON 与上游 `4181f66` 生成物 diff 为空（或差异有登记）

## 门禁验收

通用门禁 G1–G7 全过（G3：目录对拍）。

任务特有标准：

- [ ] 需求 R2.7 四条逐条核对表
- [ ] vendored JSON diff 审查记录附验收

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
