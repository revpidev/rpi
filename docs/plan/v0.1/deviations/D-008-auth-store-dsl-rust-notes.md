# D-008：auth 存储与 key DSL 的 Rust 落地差异

- **状态**：已回写
- **关联任务**：T04
- **级别**：实现细节偏离
- **发现日期**：2026-07-31

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.5；`docs/01-requirements.md` §5.4；`docs/coding-standards.md` §9.2、附录 A
- 原文约定：CredentialStore 为 JSON 文件（0600）、`modify` 唯一写路径 + 跨进程文件锁（`fs2`）；key 值解析 DSL（`!cmd` / `$VAR` / `${VAR}` / `$$` / `$!`）；凭据 JSON 与 Pi `auth.json` 兼容。

## 实际实现与偏离原因

按设计落地，以下实现细节与上游（`auth-storage.ts` / `resolve-config-value.ts`）存在差异：

1. **锁语义**：`fs2` 无 proper-lockfile 的 `stale` / `onCompromised` 对应物——flock 随进程退出自动释放，无 stale 概念；上游「lock compromised」测试场景无法构造，未移植（编码规范 §9.2 已钉死 fs2，属既定选型而非新增偏离）。
2. **锁重试 jitter**：proper-lockfile `randomize:true` 用真随机；依赖基线无 rand，用 `SystemTime` 纳秒派生 [1,2) 伪随机 jitter。
3. **`!cmd` 仅 unix 语义**：固定 `/bin/sh -c`（对齐上游 `execSync` 默认 shell）；上游 win32 的 configured-shell/stdin 分支未移植（v0.1 以 unix 为目标平台）。
4. **快照保序方案**：文件快照与写路径用 `serde_json::Map`（preserve_order）保插入序以通过字节对拍；附带效果：`list()` 对未知 `type` 标签的条目跳过（上游会原样列出字符串 type），`read()`/`modify` 回调对未知形状条目报错但不写回（文件不损）。
5. **`resolve_headers`** 操作 `ProviderHeaders`（值可为 `None` 抑制标记，原样透传），上游为 `Record<string,string>`。
6. **保真修正**（非偏离，记录备查）：`get_provider_env_value`（T03 既有代码）补上 JS `||` 空串回退语义（空 override 落 process env，空 process 值 → None）。

## 影响面

无（纯内部）。凭据 JSON 线格式字节级对齐上游并有 fixtures 对拍（`fixtures/generated/auth/`）。

## 处置

- **回写位置**：`docs/02-design.md` §3.5（Rust 落地注记）；`docs/01-requirements.md` §5.4（落地注记指针）
- **回写日期**：2026-07-31
- **ADR**：不需要
