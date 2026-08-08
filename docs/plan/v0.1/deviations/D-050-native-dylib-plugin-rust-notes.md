# D-050：L0 原生动态库插件（abi_stable）落地差异

- **状态**：已回写
- **关联任务**：T15（W7）
- **级别**：实现细节偏离
- **发现日期**：2026-08-08

## 原文档约定

- 设计 §7.2：「动态库插件（`abi_stable`，已决策，见 §14）」；§14 技术选型
  速查钉 abi_stable。
- `docs/extension-abi.md`：T15 W6 定稿时仅覆盖 wasm（L1）ABI。

## 实际实现与偏离原因

abi_stable 0.11.3 动态库插件按设计落地（`crates/pir-ext-host/src/native.rs`；
参考插件 `crates/pir-test-native-plugin`，cdylib），消息格式与 wasm ABI v1
完全一致（同一 JSON method 表 / capability 强制 / 错误 kind 表）。落地细节
相对设计稿的补充与收敛：

1. **ABI 形状受 abi_stable 约束收敛**：fn 指针不能作 fn 参数（嵌套 fn ptr
   无 StableAbi 派生）→ host-call 句柄打包为 `repr(C)` 的 `PirHostCalls`
   结构体**按值**传入 `pir_extension_init`；`usize` 无 StableAbi → 上下文
   cookie 用 `*const c_void`；借用切片会把生命周期带进 fn 指针类型 → 缓冲
   一律 `RVec<u8>` 拥有型双向传递（无 wasm 的 alloc/dealloc 舞步）。
2. **无沙箱信任模型明示**：原生插件在宿主进程内运行，拥有全部 OS 权限；
   capability 系统只管扩展 API 面（host call 层强制与 wasm 相同），不构成
   安全边界。此为 L0 载体的固有属性，已在 extension-abi.md §1.1 与
   native.rs 模块头明示，供扩展安装者决策。
3. **manifest 新增 `native` 字段**：`pir-extension.json` 的包相对动态库路径，
   与 `wasm` 互斥（并存时 `wasm` 优先）；裸 `.so`/`.dll`/`.dylib` 散文件
   `capabilities = []`（extension-abi.md §5）。
4. **无 fuel/Store/专属线程**：dispatch 在调用线程内同步执行，死循环插件可
   拖死宿主（与 wasm 的 fuel 治理不同，同属无沙箱口径的一部分）。

## 影响面

扩展 API（新增载体形态，不改变既有 method 表语义）；协议 / session 格式 /
TUI 行为无变化。二进制体积：abi_stable 进入主二进制依赖树，W8 体积复测
（<50MB）时一并核对。

## 处置

- **回写位置**：`docs/extension-abi.md` §1.1 / §5（native 段，2026-08-08
  已补）；`02-design.md` §7.2 落地注记；本表 D-050 行；T15 任务文件偏离
  记录表
- **回写日期**：2026-08-08
- **ADR**：不需要（abi_stable 选型本身 ADR-0002/设计 §14 已决策）
