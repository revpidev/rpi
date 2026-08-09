# T24：资源加载与包管理

- **状态**：未开始
- **里程碑**：M2
- **依赖**：—
- **上游对照**：`8ecf8a988`（AGENTS.override.md）、`66eead652`（资源元数据保留 #6968）、`cced6a21d`（嵌套 worktree 去重）、`bff5ab717`（system prompt source，v0.83.0）、`b06dc76fd`+`0563a7c01`（git 包安装容错）、`src/core/pi-manifest.ts`（新）；`resource-loader.ts:71`（候选顺序）
- **需求章节**：v0.11 需求 R3.5、R3.7；设计 §5.5
- **预估**：0.25 人月

---

## 目标

资源加载与包管理的增量对齐，全部为独立小项，可与其他 M2 任务完全并行。

## 范围

### In

- `AGENTS.override.md` 加入上下文文件候选链首位：`AGENTS.override.md` > `AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` > `CLAUDE.MD`（同目录覆盖语义）
- 扩展 reload 资源后保留 skills/prompts/themes 的 package source 元数据（#6968）
- 嵌套 worktree 上下文文件去重：`find_shadowed_context_file()` 用 git commonDir/mainRepoRoot 判定影子文件，避免 AGENTS.md 加载两次
- `ResourceLoader` 接口补 `get_system_prompt_source`/`get_append_system_prompt_sources`（v0.83.0 继承项，v0.1 未覆盖则补齐）
- git 包安装容错：`git clean` 失败后检测缺失依赖并重装；安装失败清理残留目录；`.rpi-update-incomplete` marker 续传语义
- `read_pi_manifest()` 独立化：package.json `pi` 字段解析从 package-manager 抽出 + 类型校验（rpi 侧读 `rpi` 字段的等价物，沿用 v0.1 包格式决策）

### Out

- 扩展 API 的 ResourceLoader 面向外暴露（T27）
- packages 其他行为（v0.1 T14 已交付部分不重做）

## 开发要点

- 候选链顺序是对拍契约，golden 用例覆盖五候选优先级与 override 覆盖
- worktree 去重需要构造嵌套 worktree fixture（git worktree + 子模块场景）
- `.rpi-update-incomplete` 命名沿用 APP_NAME 派生惯例（上游 `.pi-update-incomplete`）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上下文文件候选链五优先级 golden（含 override 覆盖 AGENTS.md/CLAUDE.md）
- [ ] reload 后 package source 元数据保留（#6968 场景）
- [ ] 嵌套 worktree 去重 fixture（commonDir 判定）
- [ ] git 包安装：clean 失败依赖重装 / 失败清理 / marker 续传三路径
- [ ] `read_pi_manifest` 类型校验拒绝非法形状

## 门禁验收

通用门禁 G1–G7 全过（G3：资源黄金对拍随 `generate-resources-golden.mjs` 重新生成）。

任务特有标准：

- [ ] 需求 R3.5/R3.7 逐条核对表

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
