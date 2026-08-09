# D-039：可选工具 grep/find/ls Rust 原生落地差异（ignore/globset 替代外部 rg/fd）

- **状态**：已关闭
- **关联任务**：T14（W1）
- **级别**：第 1 条为行为级偏离（立 ADR-0005）；第 2–8 条为实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §4.5（可选工具行为锚点）、ADR-0003 §2
  （`rpi/src/tools/` 原生实现，ignore/globset，不引入 rg/fd 下载）；
  上游基准 `packages/coding-agent/src/core/tools/{grep,find,ls}.ts` @ 0.82.1 (2efa728)。
- 原文约定：grep 等价 `rg --json --line-number --color=never --hidden`；
  find 等价 `fd --glob --color=never --hidden`（repo 外 `--no-require-git`）；
  ls 大小写不敏感排序（上游 `toLowerCase().localeCompare(...)`）；三工具
  limit/截断/提示文案为对拍契约。

## 实际实现与偏离原因

1. **ls 排序用 Unicode code point 比较替代 ICU `localeCompare`**（ls.rs）：
   `a.toLowerCase().localeCompare(b.toLowerCase())` 依赖 ICU 排序表（引入 icu4x
   代价过大）。Rust 侧对小写后名称按码位排序。对纯 ASCII 字母数字文件名两者一致；
   仅当名称混用标点/下划线与字母时（如 `_a` vs `Z`）顺序可能不同。上游测试未覆盖
   该边界，需求 §4.5 的「大小写不敏感排序」口径不变。
2. **grep 错误文案为 regex/globset crate 原生文案**：上游透传 rg/fd 的 stderr
   （如 `rg: regex parse error: …`、`[fd error]: error parsing glob '[': …`）；
   原生实现分别产生 `regex parse error: …`（regex crate）与
   `error parsing glob '[': …`（globset crate，与 fd 正文一致、无 `[fd error]:` 前缀）。
   错误语义（拒绝执行 + 解析错误可读文案）一致，字面前缀不同。
3. **find 搜索路径非法的错误文案**：fd 输出两行 stderr（`Search path '…' is not a
   directory.` + `No valid search paths given.`），上游透传整段；原生实现只报第一行
   语义（无 `[fd error]:` 前缀）。
4. **grep 二进制文件判定按全文 NUL 扫描**：walk 模式 rg 对含 NUL 的文件整体抑制
   匹配（已用 rg 15 对拍确认匹配行也不输出）；原生实现对文件全文做 NUL 检查，
   命中即跳过。rg 按块检测的极晚期 NUL（首块之后）场景下 rg 可能已输出前面的匹配，
   原生实现则整体抑制——该边界无测试锚点。
5. **grep `--glob` 锚点取工具 `ctx.cwd`**：rg 把 glob 锚定在进程 cwd；原生实现锚定在
   会话 cwd（正常启动两者相同；进程 cwd 与会话 cwd 分离的 SDK 场景下锚点不同）。
6. **取消粒度**：上游 abort 直接杀 rg/fd 子进程；原生实现在 walk 循环内按条目检查
   `CancellationToken`，最坏延迟为单个文件的读取+搜索时间。
7. **find 固定剪枝 node_modules/.git 为整目录剪枝**：需求 §4.5 要求「固定忽略
   node_modules/.git」（源自上游 custom-ops 的 `**/node_modules/**`、`**/.git/**`
   忽略清单）；整目录剪枝使目录条目本身也不出现在结果中（custom-ops 语义下目录本身
   可被 `**` 类 pattern 匹配，仅其子项被忽略）。fd 代码路径上游本来无此排除，
   此处按需求文档口径实现。
8. **find custom-operations 分支的相对化回退简化**：上游对不以 searchPath 开头的
   结果做 `path.relative(searchPath, p)`（可产生 `../` 段）；原生实现对该回退只做
   posix 化（custom ops 尚无消费者，扩展宿主在 T15 落地）。

## 影响面

无（纯内部）：输出格式、limit、截断、提示文案等契约项均与上游逐条对齐并经
rg 15 / fd 10.4 实机交叉验证（混合树逐行 diff 一致；find 仅差契约规定的
node_modules/.git 剪枝）。第 1 条为可观察的顺序边缘差异，仅影响标点/字母混排
文件名的 ls 排序。

## 处置

- **回写位置**：`docs/01-requirements.md` §4.5（ls 行补充排序实现注记）；
  `docs/plan/v0.1/T14-packages-trust-export.md` 偏离登记表
- **回写日期**：2026-08-06
- **ADR**：第 1 条（ls 码位排序）立 [ADR-0005](../../adr/0005-ls-collation-codepoint-order.md)；
  第 2–8 条不需要（ADR-0003 §2 已授权原生实现路线）

## 终审修复补记（2026-08-07 审查修复波次）

1. **第 6 条（取消粒度）补 ls**：`ls.rs` 原实现仅在入口与循环后检查
   `CancellationToken`，`read_dir`+排序+stat 循环会跑完整轮才返回；已改为条目循环内逐条
   检查（与 grep/find 同粒度），取消延迟 = 单条 stat。
2. **grep 内存面**：原实现每文件 `std::fs::read` + 全量 lossy 解码（约 2× 文件大小内存）；
   已改为流式搜索（`search_stream`，64KB 块 + 整行字节累积后解码，跨块多字节字符不损坏，
   NUL 抑制带回滚；单行无换行的病态文件仍缓冲整行，属记录在案的边界）。
3. **测试锚点补 `.git` 内容**：新增 `test_git_contents_are_searched`（`--hidden` 含
   `.git/` 内文件，rg 15 实测确认；与 find 的固定 `.git` 剪枝区分）。用例数 38 → 39
   （grep 19 / find 10 / ls 7 / wiring 3），T14 文档相应更正。
