# rpi Extension ABI v1（wasm 扩展宿主）

T15 W6 定稿。宿主实现：`crates/rpi-ext-host/src/wasm/`；guest SDK：
`crates/rpi-ext-sdk`；示例：`examples/wasm-extension/`。

## 1. 内存模型与字节布局

- guest 导出（wasm32-unknown-unknown，无 WASI）：

  | 导出 | 签名 | 用途 |
  |---|---|---|
  | `memory` | — | 线性内存，所有载荷所在 |
  | `rpi_alloc` | `(len: u32) -> u32` | host 写入响应前调用，分配 guest 内存 |
  | `rpi_dealloc` | `(ptr: u32, len: u32)` | host 读完响应后调用 |
  | `rpi_extension_init` | `() -> u64` | 加载入口（注册发生在这里） |
  | `rpi_dispatch` | `(ptr: u32, len: u32) -> u64` | 事件/工具/命令/渲染分发 |

- host 导入（模块名 `rpi`）：

  | 导入 | 签名 | 用途 |
  |---|---|---|
  | `rpi_host_call` | `(ptr: u32, len: u32) -> u64` | guest → host 的 API 调用 |

- 载荷均为 guest 线性内存中的 UTF-8 JSON。guest→host 传 `(ptr, len)`；
  host→guest 返回 `u64 = (ptr << 32) | len`（host 调 guest 的
  `rpi_alloc` 分配、写入，guest 读完后 host 调 `rpi_dealloc`）。
- host 侧对 guest 返回的 `u64 = 0` 视为内部错误（分配/写入失败）。

### 1.1 L0 原生动态库插件（abi_stable，T15 W7）

wasm 之外的第二种载体：进程内动态库（`.so` / `.dll` / `.dylib`，cdylib）。
宿主侧实现 `crates/rpi-ext-host/src/native.rs`；参考插件
`crates/rpi-test-native-plugin`。

- 消息格式与 wasm ABI 完全一致：同一 JSON method 表（§2/§3）、同一
  capability 强制（§3）、同一错误 kind 表（§4）。差别只在字节怎么过边界：
  - 插件用 `#[export_root_module]` 导出 `RpiNativeModule`（prefix 模块），
    两个 extern "C" 字段：`rpi_extension_init(RpiHostCalls, cookie)`
    与 `rpi_dispatch(cookie, RVec<u8>) -> RVec<u8>`。
  - host-call 句柄**按值**打包进 `repr(C)` 的 `RpiHostCalls` 结构体传给
    init——abi_stable 无法为「以 fn 指针为参数的 fn 指针」派生 StableAbi，
    故句柄不能作参数，只能乘结构体。
  - `cookie` 为 `*const c_void` 不透明上下文指针（abi_stable 不布局
    `usize`），指向宿主侧按插件持有的调用上下文，生命周期覆盖所有调用。
  - 缓冲一律 `RVec<u8>` 拥有型双向传递（借用切片会把生命周期带进 fn
    指针类型）；无 `rpi_alloc`/`rpi_dealloc` 舞步。
- 信任模型：原生代码**无沙箱**——capability 系统只管扩展 API 面，插件
  本身拥有宿主进程的全部 OS 权限（设计 §14 既定的 L0 口径）。无
  Store/线程/fuel，dispatch 在调用线程内同步执行。
- 发现规则与 wasm 相同（manifest `native` 字段，见 §5；裸动态库散文件
  `capabilities = []`）。

## 2. 两条通道

### 2.1 `rpi_host_call`（guest → host）

请求：`{"call": "<method>", "args": {...}, "seq": N}`（`seq` 为
guest 侧自增序号，仅供日志关联）。
响应：`{"ok": <value>}` 或 `{"error": {"kind": "<kind>", "message": "..."}}`。

### 2.2 `rpi_dispatch`（host → guest）

消息：`{"kind": "event", "event": "<name>", "payload": {...}}` → 返回
handler 结果 JSON（`null` = undefined）。其余 kind：
`{"kind": "toolExecute", "toolName", "toolCallId", "params"}` →
`AgentToolResult` JSON；`{"kind": "command", "name", "args"}`（此分发内才允许
`command.*` host call）；`{"kind": "shortcut", "shortcut"}`；
`{"kind": "render", "what": "toolCall"|"toolResult"|"message"|"entry", ...}`
→ ComponentTree JSON 或 `null`；`{"kind": "bus", "channel", "data"}`（fire-and-forget）。

**串行语义**：每扩展实例独立 Store + 专属阻塞线程，dispatch 按到达顺序
逐个执行——镜像上游 handler 串行（runner.ts:788-820）。

## 3. method 表与 capability 映射

`capabilities: []`（裸 `.wasm`）只允许 `on`（与 `getFlag`，只读自有
flag 值——注册表元数据）。逐 host call 强制；拒绝返回
`{"error":{"kind":"capabilityDenied",...}}`。

| method | capability | 落点 |
|---|---|---|
| `on` / `getFlag` | （免费） | `ExtensionApi::on` / `get_flag` |
| `registerTool` | `tools` | `ExtensionApi::register_tool`（execute/render 转发 guest） |
| `registerCommand` / `registerShortcut` / `registerFlag` | `commands` | 同名 API |
| `registerMessageRenderer` / `registerEntryRenderer` | `ui` | 同名 API（render 转发 guest） |
| `sendMessage` / `sendUserMessage` / `appendEntry` / `setSessionName` / `getSessionName` / `setLabel` / `getActiveTools` / `getAllTools` / `setActiveTools` / `getCommands` / `setModel` / `getThinkingLevel` / `setThinkingLevel` | `session` | `HostActions` 同名方法 |
| `exec` | `exec` | `HostActions::exec` |
| `registerProvider` / `unregisterProvider` | `provider` | `HostActions` 同名方法 |
| `events.emit` / `events.on` | `events` | 共享 `EventBus` |
| `ctx.*`（isIdle / isProjectTrusted / hasPendingMessages / getContextUsage / getSystemPrompt / model / cwd / mode / hasUI / abort / shutdown / compact） | `session` | `ContextActions` |
| `command.*`（waitForIdle / newSession / fork / navigateTree / switchSession / reload） | `session` + 仅 command 分发内 | `CommandContextActions`；事件上下文调用返回 `invalidRequest` |
| `ui.*`（28 方法，`ui.select` 等） | `ui` | `UiBridge` 同名方法 |

## 4. 错误 kind 表

| kind | 含义 |
|---|---|
| `capabilityDenied` | capability 未授予 |
| `stale` | 扩展实例已失效（session 替换/reload 后） |
| `unbound` | 宿主动作未绑定（加载期调动作方法） |
| `invalidRequest` | JSON 解析失败 / 缺字段 / 越界调用（如事件上下文调 command.*） |
| `unknownMethod` | 未知 method |
| `memoryAccess` / `missingExport` | ABI 层错误 |
| `call` | 宿主动作自身失败 |
| `internal` | 宿主内部错误 |

guest 端 trap（含 fuel 耗尽）→ 加载错误（init）或 handler 错误
（dispatch，经 `emit_error` 收集，agent 继续——与 native 一致）。

## 5. manifest（`rpi-extension.json`，目录级）

```json
{
  "name": "my-ext",
  "version": "0.1.0",
  "description": "...",
  "wasm": "dist/my_ext.wasm",
  "capabilities": ["tools", "commands", "ui", "session", "exec", "provider", "events"],
  "rpiAbi": 1
}
```

- `rpiAbi != 1` → 加载错误；未知 capability 字符串 → 加载错误。
- `native`（可选）：L0 原生插件动态库的包相对路径（§1.1），与 `wasm`
  互斥——两者并存时 `wasm` 优先（`native` 被忽略）。
- 裸 `.wasm` / 裸动态库（一层目录内的散文件）→ `capabilities = []`。
- 发现规则与一层目录约定见 resource-loader 语义（散文件 + 子目录
  index/manifest）。

## 6. 并发与资源治理

- 每扩展独立 Store + 专属线程；host call 内异步动作 spawn 到宿主 tokio
  runtime，guest 线程经 std channel 阻塞等待。
- 每次 guest 调用（init/dispatch/host-call 重入）授予固定 fuel
  （`CALL_FUEL`），耗尽即 trap——防死循环拖死宿主。
- Engine 全局共享（模块编译缓存所在）；FactoryCache 按 cwd+generation
  缓存「编译产物 Module 的 factory 闭包」，cwd 切换或 /reload 递增
  generation 失效。

## 7. 组件树

渲染类返回 ComponentTree v1（`rpi_ext_host::types::COMPONENT_TREE_SCHEMA_V1`
常量；映射器在 `rpi::modes::interactive::component_tree`）。

## 8. 版本治理

`rpiAbi` 为主版本号：宿主拒绝不认识的版本。ABI 演进（新
method/kind）向后兼容追加，不兼容变更升 rpiAbi 并在本文件记变更史。
