# ADR-0010：项目改名 pir → rpi（与域名 resetpi.com 对应）

- **状态**：已采纳
- **日期**：2026-08-09
- **关联**：ADR-0001（命名决策）、ADR-0009（端点域名 resetpi.com，已由 ADR-0011 更新为 revpi.dev）、
  扩展 ABI 文档 `docs/extension-abi.md`

## 背景

pir 是 Pi 的 Rust 移植，独立分发（ADR-0001 起已把命名从 `PI_*` 迁到 `PIR_*`、
配置目录从 `~/.pi` 迁到 `~/.pir`）。产品端点域名确定为 `resetpi.com`
（ADR-0009）后，项目名与域名不对应（pir ↔ resetpi）。本项目尚未对外发布，
改名成本最低，决定全面改名。

## 决策

项目名 **pir → rpi**，全量改名、不做旧名兼容：

| 面 | 旧 | 新 |
|---|---|---|
| 二进制 | `pir`、`pir-rpc` | `rpi`、`rpi-rpc` |
| crate | `pir`、`pir-ai`、`pir-agent`、`pir-tui`、`pir-ext-host`、`pir-ext-sdk`、`pir-test-*` | `rpi`、`rpi-ai`、`rpi-agent`、`rpi-tui`、`rpi-ext-host`、`rpi-ext-sdk`、`rpi-test-*` |
| env 前缀 | `PIR_*`（含 `PIR_ABI_VERSION`） | `RPI_*` |
| 配置目录 | `~/.pir`、`{cwd}/.pir` | `~/.rpi`、`{cwd}/.rpi` |
| 标识 | `APP_NAME`/`PACKAGE_NAME`/UA `pir/x.y.z` | `rpi` / `rpi/x.y.z` |
| 扩展 manifest 字段 | `pirAbi` | `rpiAbi` |
| 原生插件 ABI 符号 | `PirNativeModule` / `pir_extension_init` / `PirHostCalls` | `RpiNativeModule` / `rpi_extension_init` / `RpiHostCalls` |
| WASM 扩展 ABI 符号 | `pir_host_call` / `pir_alloc` / `pir_dealloc` | `rpi_host_call` / `rpi_alloc` / `rpi_dealloc` |

要点：

- **扩展 ABI 为破坏性变更**：manifest 字段与导出符号改名，旧扩展（含旧版
  测试夹具）需重新构建；`RPI_ABI_VERSION` 值保持 1（形状未变，仅命名）。
  项目未发布，无存量扩展兼容负担。
- **旧配置完全迁移、不保留兼容读取**：`~/.pir` → `~/.rpi` 由用户/维护脚本
  一次性搬移（本机已 `mv`，auth/settings/models/sessions 随迁）。
- **上游引用保留**：`Pi`、`external/pi/`、`pi.dev`（radius 上游托管服务）等
  对上游的指称不改；`parity_*`（对拍测试）命名保留。
- 文档、部署站点（`deploy/resetpi/`，brand 为 Rpi）、示例扩展同步改名。

## 影响与回退

- 影响面：全部 crate、环境变量、配置目录、UA、扩展 ABI、文档与站点品牌。
- 回退：改名不可逆（ABI 符号变更后旧插件需重编）；如反悔需另立 ADR。
