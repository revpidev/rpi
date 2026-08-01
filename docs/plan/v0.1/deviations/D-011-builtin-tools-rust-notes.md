# D-011：内置四工具与支撑设施的 Rust 落地差异

- **状态**：已回写
- **关联任务**：T06
- **级别**：实现细节偏离
- **发现日期**：2026-08-01

## 原文档约定

- 文档与章节：`docs/02-design.md` §6.5；`docs/01-requirements.md` §4.5；`docs/coding-standards.md` 附录 A
- 原文约定：`ToolContext { cwd, signal, on_update, session_env }` 注入；工具行为锚点（截断常数、fuzzy 匹配、超时、节流、环境注入顺序等）逐条对齐上游 `core/tools/`；图像 autoResize 2000×2000（上游 Photon WASM）。

## 实际实现与偏离原因

行为锚点全部按上游语义落地（常数逐值核对表见 T06 验收记录），以下实现细节因 Rust 语言/生态约束与上游存在差异（行为契约不变）：

1. **`ToolContext` 形状**：设计文档的 `signal`/`on_update` 字段不进 `ToolContext`——T05 钉死的 `AgentTool::execute` 签名已按调用传入 `CancellationToken` 与 update 回调；`ToolContext { cwd, session_env }` 仅承载构造期上下文（`tools.rs` 模块文档注记）。
2. **图像处理栈**：上游用 Photon（Rust/WASM）+ 手写 EXIF 解析器；pir 用 `image` crate（纯 Rust，default-features 关闭仅启用 png/jpeg/gif/webp/bmp）做解码/缩放/编码（Lanczos3、`[80,85,70,55,40]` 质量梯度、×0.75 尺寸回退与上游一致），`kamadak-exif` 做 EXIF 方向校正（JPEG/WebP 等价；PNG/GIF/BMP 无 EXIF 两侧均为无操作）。GIF 缩放取首帧，与上游 Photon 单帧行为一致。图像字节级输出不保证与上游一致（编码器不同），但 content block 形状、mime 类型、hint 文案为对拍契约面，均字面对齐。
3. **diff 生成自实现 Myers**：上游用 jsdiff 的 `diffLines`/`createTwoFilesPatch`；pir 在 `edit_diff.rs` 自实现 O(ND) Myers 行级 diff，并在其上生成自定义 diff（`+N/-N/ N` 前缀、上下文折叠四分支）与 unified patch（`---/+++` 头、context 4、`\ No newline at end of file`）。`details.diff/patch` 不进 Agent 层对拍契约（fixture 剥离 details），文本形状有移植测试锚定。
4. **`OutputAccumulator` 同步 API**：`append/finish/snapshot/close_temp_file` 为同步 `&mut self`，临时文件用 `std::fs` 小块追加写（非大文件阻塞 I/O，编码规范 §6 允许范围），便于在 bash 数据回调中直接调用。`append` 在 `finish` 后静默忽略（上游 throw），调用方契约内不会触及。
5. **`BashOperations::exec` 的 `on_data` 参数为 `Vec<u8`**：`async_trait` 下 `&[u8]` 会产生 HRTB 生命周期冲突；按值传递与上游 Node Buffer（同样按值拥有）语义等价。
6. **`trackDetachedChildPid` 全局注册表未移植**：上游用于父进程崩溃时清理孤儿 detached 进程组；pir 取消/超时路径的进程组 SIGKILL 语义完整（有「取消后无残留」测试锚定），崩溃兜底为已知简化，后续需要时补。
7. **`compute_edits_diff` 同步化**：用 `std::fs` 读文件，返回结构体而非 async Result；仅 TUI 预览消费者（T12）。
8. **`ProcessedImage` 增加 `width`/`height` 字段**：供维度 hint 计算与测试断言；不影响输出文案。
9. **`getShellEnv` 前置 `~/.pir/bin`**：上游前置 `getBinDir()`（agentDir/bin = `~/.pi/bin`）；pir 配置根为 `~/.pir`（ADR-0001），等价前置 `~/.pir/bin`。PIR_* 环境变量前缀重命名同为 ADR-0001 钉死项，非偏离。
10. **bash 节流更新实现**：上游 `Date.now()` 节流 + 定时器合并；pir 用 unbounded mpsc + drain 合并（收到通知先 drain 队列、距上次不足 100ms 则睡眠后再 drain），首次数据立即更新；合并语义等价（5000 行 chatty 输出更新数 < 25 的移植测试锚定）。
11. **`io_error_message` errno 映射**：Node `error.code` 文案经 `raw_os_error` 精确映射（EPERM/ENOENT/EACCES/EEXIST/ENOTDIR/EISDIR），无 raw code 时回退 `ErrorKind` → `Error code: ENOENT/EACCES` → `Error: {msg}`；覆盖上游测试断言的 ENOENT/EACCES/通用错误三形态。
12. **write 成功文案的「bytes」**：按上游 `content.length`（UTF-16 code unit 数）实现为 `encode_utf16().count()`，非字节数亦非字符数（注释说明，测试锚定）。

## 影响面

无（纯内部）。对拍契约面（事件序、content 文本、错误文案、截断/环境注入/节流行为）逐项有移植测试或 fixture 归一化 diff 锚定：`pir/tests/parity_tools_test.rs`（真实 read+bash 对 `tool-calls` fixture）。

## 处置

- **回写位置**：`docs/02-design.md` §6.5（Rust 落地注记）；`docs/coding-standards.md` 附录 A（依赖基线新增行）
- **回写日期**：2026-08-01
- **ADR**：不需要
