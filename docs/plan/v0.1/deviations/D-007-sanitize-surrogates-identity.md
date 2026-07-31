# D-007：sanitize_surrogates 在 Rust 侧为恒等

- **状态**：已回写
- **关联任务**：T03
- **级别**：实现细节偏离
- **发现日期**：2026-07-29

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.6（sanitize_unicode.rs「去孤立 surrogate」）、
  `docs/01-requirements.md` §5.5（`sanitizeSurrogates` 去孤立 surrogate）
- 原文约定：移植上游 `sanitizeSurrogates`（将 U+D800–DFFF 孤立代理替换为
  U+FFFD）。

## 实际实现与偏离原因

Rust 的 `String`/`&str` 保证合法 UTF-8，不可能包含孤立代理码点；来自
`serde_json` 的解析结果同样不可能（`\uD800` 类转义在 serde_json 中要么配对
成功要么解析失败——见 json_parse.rs 的 lenient repair 路径，repair 后产生的
也是合法标量值）。因此 `sanitize_surrogates` 在 Rust 侧是恒等函数，保留同名
API 与调用点以维持与上游逐点对应的移植结构。

## 影响面

无（行为等价：上游替换后的输出同样是合法 Unicode 标量序列）。

## 处置

- **回写位置**：`docs/02-design.md` §3.6（sanitize_unicode.rs 行）
- **回写日期**：2026-07-30
- **ADR**：不需要
