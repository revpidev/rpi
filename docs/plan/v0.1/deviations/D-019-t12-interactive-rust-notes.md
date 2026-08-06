# D-019：T12 interactive 模式移植 Rust 落地笔记（汇总型）

- **状态**：已回写
- **关联任务**：T12（S4a-S7a）
- **级别**：实现细节偏离（多数为无行为差异/行为等价；逐条标注）
- **发现日期**：2026-08-05
- **最近修订**：2026-08-06（阶段 A/B 修复后收尾：「会话切换不重订阅」修复关闭；新增 --resume 独立 picker、OutputPad streaming、git watcher 轮询三条；footer branch watcher 落地）

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §8（Interactive UX 全章）、`docs/02-design.md` §5（TUI 组件清单）、§12（模块映射表）、`docs/plan/v0.1/T12-interactive-mode.md`（设计细化与实现子阶段）
- 原文约定：上游 `packages/coding-agent/src/modes/interactive/*` 与 `packages/tui/src/components/*` 行为 1:1；组件/模块命名镜像上游

## 实际实现与偏离原因

（汇总型偏离：逐条收集自各模块头部注释的「Intentional differences」清单，均已在代码内登记。标注档位：**无行为差异** / **行为等价**（实现路径不同、观察行为一致）/ **已知差异**（影响面见条目）。）

### pir-tui 组件层（S1-S3）

| 条目 | 位置 | 档位 |
|------|------|------|
| CJK 词典式分词替代 UAX#29：内嵌 6 条钉死词典（常见字/姓氏/名/量词/地名/连接词），全量 cjdict 留待后续 | `word_navigation.rs` | 行为等价（覆盖对拍用例集；生僻 CJK 边界可能与上游不同） |
| autocomplete 为同步 trait + `AtomicBool` 中止（上游 async + AbortSignal） | `autocomplete.rs` | 行为等价 |
| editor autocomplete SelectList 主题 `Arc` 化（多实例共享） | `editor.rs` | 无行为差异 |
| `terminal_rows()` 缓存访问器（渲染期锁内免死锁读尺寸） | `tui.rs` | 无行为差异 |
| `trim()` 语义差异：Rust Unicode White_Space vs ECMA-262 空白集（仅 U+FEFF 等 exotic 字符分歧） | `editor.rs` 等 | 已知差异（影响面：exotic 空白 trim 决策） |
| SelectList 空列表早退（上游环绕算术会产生不可观察的 -1/1 索引） | `select_list.rs` | 行为等价 |
| 子菜单 done 回调 VecDeque drain（上游嵌套 setTimeout 语义） | `components/*.rs` | 行为等价 |

### interactive 模式层（S4-S7）

| 条目 | 位置 | 档位 |
|------|------|------|
| 主题/组件显式注入 `Arc<Theme>`（上游进程全局 theme getter） | 全部组件 | 无行为差异 |
| 组件树 region 模式（`Arc<Mutex<T>>` + SharedChild/FocusableRegion 包装；上游 JS 引用直接持有） | `interactive_mode.rs` | 无行为差异（锁契约：回调内不锁组件、变更走 drain） |
| `/copy` 仅 OSC52 转义（100KB 上限；无 xclip/xsel 原生回退，base64 手写） | `commands.rs` | 已知差异（无原生剪贴板工具时仅 OSC52 生效） |
| `/debug` 全量渲染行段缺失（Tui 无公开 render API）——只写 agent 消息 JSONL 段 | `commands.rs` | 已知差异（功能缺口，`/debug` 输出行段待补；无认领任务，v0.1 内挂起） |
| Ctrl+Z suspend：`kill(0, SIGTSTP)` + run 循环 SIGCONT 恢复（上游 once 监听等价） | `commands_selectors.rs` + `interactive_mode.rs` | 行为等价 |
| 主题热重载用轮询 watcher（100ms，免 notify 依赖）替代 fs.watch | `theme_watcher.rs` | 行为等价（延迟 ≤100ms） |
| git 分支 watcher 用 100ms 内容轮询 `.git/HEAD` 替代 fs.watch + 500ms 防抖；reftable watchers 与 WSL `watchFile` 回退未移植（轮询 HEAD 已涵盖分支切换；`commondir` 不解析因 reftable 未移植）；支持 worktree（`gitdir:` 文件） | `git_branch_watcher.rs` | 行为等价（延迟 ≤100ms；footer 分支随切换真正刷新，2026-08-06 接线） |
| flushCompactionQueue `willRetry` 分支为死代码（compaction_end 的 willRetry 未传到 run loop），注释保留 | `interactive_mode.rs` | 已知差异（retry 轮冲刷未接线；无认领任务，v0.1 内挂起） |
| 首启 setup 判定 = `PIR_EXPERIMENTAL=1` + 无全局 settings.json（上游文件存在语义）；挂在 runtime 创建之后 | `startup_ui.rs` | 已知差异（时序与判定口径，上游 main.ts 之前） |
| ~~会话切换（/new /resume /import）不重订阅 session 事件~~ | `commands_selectors.rs` | **已修复（2026-08-06）**：`InteractiveUi.session` 改 `RwLock` 可替换，`rebind_session_ui`（对上游 `rebindCurrentSession`，interactive-mode.ts:1732-1758）做全量 rebind——注销旧订阅、换 session、`apply_runtime_settings`、清容器、`render_initial_messages`、重新订阅；/new、/resume、/clone、/fork、/import 全走此路径 |
| `--resume` CLI 走独立启动选择器 `cli/session_picker.rs`（对齐上游 main.ts:321-333 + cli/session-picker.ts；取消时打印 "No session selected" 并 exit 0），而非 T12 原设计细化的「--resume 分支接模式内选择器」挂载点 | `cli/session_picker.rs` + `app.rs` | 行为等价（实现路径不同：picker 在 session manager 创建前独立起 TUI，与上游一致） |
| OutputPad 设置热应用：streaming 中只就地更新活动 streaming 组件，历史 chat child 保持旧 padding（无组件 downcast，无法走上游 `chatContainer.children` 遍历，interactive-mode.ts:4274-4282）；非 streaming 时 rebuild | `commands_selectors.rs` | 已知差异（streaming 中历史消息 padding 不更新） |
| bash `record_bash_result` 由本地 execute_bash 内部完成（上游显式调用，行为等价） | `commands_selectors.rs` | 行为等价 |
| 外部编辑器 spawn+wait（上游 spawn+close 回调；TUI 已停时驱动线程阻塞等价）；非 0 退出加状态提示（上游静默） | `external_editor.rs` | 行为等价 / 已知差异（提示文案） |
| Ctrl+V 粘贴：xclip PNG 探测（3s 超时）+ 平台工具文本回退（无 native addon/Photon） | `commands_selectors.rs` | 已知差异（剪贴板图片路径） |
| loadedResources 诊断区合并单块 `[Resource issues]`（上游四类分块）；scope-group 展开视图省略 | `interactive_mode.rs` | 已知差异（展示结构） |
| cache-miss 通知为挂点（T14/cache-stats） | `interactive_mode.rs` | 已知差异（功能缺口，T14） |
| 主题热重载后 chat 历史/footer 保持旧色（上游全局 proxy 懒解析），新内容用新主题 | `interactive_mode.rs` | 已知差异（热重载视觉一致性） |

## 影响面

TUI 行为（少数已知差异条目）；其余为纯内部结构。

## 处置

- **回写位置**：`docs/02-design.md` §5、§12、`docs/01-requirements.md` §8（对应条目注记）、`docs/plan/v0.1/T12-interactive-mode.md`（偏离记录节）
- **回写日期**：2026-08-05；2026-08-06 修订回写（会话切换修复、--resume picker、OutputPad、git watcher 四条同步至 T12 任务文件与映射表）
- **ADR**：不需要（均为实现细节；其中 /debug 渲染行段缺口与 willRetry 冲刷接线为功能缺口挂点——经查 T13/T14/T15 任务文件均未认领，v0.1 内挂起）
