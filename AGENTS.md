# rc-device

125 kHz TEMIC 门锁卡读写器 + 身份证读卡器的 Rust SDK（Windows 硬件层，不含任何业务规则）。

## 项目

- crate `rc-device`，edition 2021，`rust-version = 1.77`（不要升依赖超过它：`tungstenite` 钉在 0.29）。
- 入口 `src/lib.rs`：导出两个互不相关的 trait —— `CardDevice`（门锁卡）与 `IdCardReader`（身份证），以及 `DeviceError`、卡类型、sidecar 协议。
- 两个 bin：`src/bin/probe.rs`（真机联调）、`src/bin/rc-device-sidecar.rs`（32 位设备宿主，按 NDJSON 走 stdin/stdout，供 64 位宿主使用）。
- 驱动 `drivers/LC2000/LC32RFRW.dll` 随仓库分发：**只有 x86**，102,492 字节 / 92 导出 / SHA-256 `f2f483e0…bccbc0`。卡格式结论只对该版本成立。
- 责任边界：本 crate 只做设备 I/O，不知道房号文本、房态、入住单、发卡上限，也不访问数据库。

## 命令

```powershell
cargo test                                                    # 190 单测 + 9 契约 + 4 sidecar；真机测试静默跳过
cargo clippy --all-targets -- -D warnings
cargo build --target i686-pc-windows-msvc                     # 真机运行必须 32 位（驱动只有 x86）
cargo run --bin probe --target i686-pc-windows-msvc           # 真机联调
cargo build --bin rc-device-sidecar --target i686-pc-windows-msvc
$env:RC_HW_TEST=1; cargo test --test real_hardware --target i686-pc-windows-msvc -- --nocapture --test-threads=1
```

- `rustfmt` 组件当前未安装（`rustup component add rustfmt` 后可用 `cargo fmt --check`）；代码风格按 `.editorconfig`：LF、4 空格、md 保留行尾空格、`drivers/**` 一律不改写。
- 64 位构建只用于 `cargo check`/单测；64 位加载 32 位 DLL 会返回 `DeviceError::DriverArchMismatch`，sidecar 亦然。

## 架构

| 模块 | 职责 |
|---|---|
| `src/protocol.rs` | 协议常量（魔数 `5B EC`、位域、上限、写密码）；`card.rs` 再导出 |
| `src/card.rs` | 硬件类型：`CardKind`、`*Spec` 入参、`CardSnapshot`（28 字节快照，无房号字段） |
| `src/builder.rs` | 卡字节构造、卡号 `timestamp_card_no`/`next_stamp`、时长码、原始直写 `build_raw` |
| `src/parser.rs` | 28 字节反解析，按字节判卡型（解析不出绝不猜成客卡） |
| `src/selftest.rs` | 46 项离线黄金向量（33 字节 + 7 流水号 + 6 时长），不碰硬件 |
| `src/card_device.rs` | `CardDevice` 契约；`src/dll/device.rs` 是唯一生产实现 `DirectDllDevice` |
| `src/sim.rs` | 内存模拟后端 `SimCardDevice`（`--sim`），不加载驱动、字节仍走 builder/parser |
| `src/dll/` | 唯一 `unsafe` 区：`mod.rs` 导出绑定（stdcall）、`pe.rs` 架构校验、`timeout.rs` 超时线程、`timing.rs` 实测延时常量、`arch.rs` 驱动哈希基准 |
| `src/error.rs` | `DeviceError` + `DeviceErrorPayload`；`sdk_code()` 对齐原 SDK `LockResult` 1–11，本层自有故障为 `-1` |
| `src/idcard*.rs` | `IdCardReader` 契约、厂商文本解析、`ws://127.0.0.1:9001` 同步短连接客户端 |
| `src/sidecar.rs` | NDJSON 线上协议：`Request`/`Response`/`Operation`，错误体复用 `DeviceErrorPayload` |
| `tests/` | `contract_is_mockable.rs`（两个 trait 可 mock）、`real_hardware.rs`、`sidecar_protocol.rs` |

## 约定

- **lint 是硬约束**：crate 级 `deny(unsafe_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)` + `warn(clippy::pedantic)`。生产代码禁止 `unwrap`/`expect`/`panic`，错误一律 `DeviceResult`；`unsafe` 只允许出现在 `src/dll/**` 并就地写明安全前提。
- 测试模块统一 `#[cfg(test)]` + `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] mod tests`，单测就近放模块内；跨进程/契约测试放 `tests/`。
- **不加业务概念**：不引入房号字符串、房态、权限、数据库。`room_index` 是唯一身份，有效范围 **0–4094**（4095 与总控卡字节相同）。
- **绝不自动重试写卡/超时**；回读校验的 3 次重试（间隔 200 ms）是协议的一部分。超时后 `untrusted` 标记会话并拒绝后续调用。
- 所有驱动调用经 `Mutex` 串行 + `run_with_timeout`；写卡前必须 `config_parameter(0)`（明文），否则"能读不能写"。
- 协议常量进 `protocol.rs`、实测延时进 `dll/timing.rs`，不写字面量魔法数。
- 注释/文档用中文，写"为什么"并标注【实测】来源；黄金向量固定基准 `2026-11-08 14:00:00`（年月字节恒 `0xAB`）。
- IPC/序列化结构用 `#[serde(rename_all = "camelCase")]`；错误对外形状固定为 `DeviceErrorPayload`（`kind`/`sdkCode`/`message`/`hint`/`deviceFault`），前端与 sidecar 共用。
- sidecar 的 **stdout 只写响应行**，日志一律 stderr；收到 `shutdown` 或 stdin EOF 即退出。
- 改动编码逻辑后先跑 `selftest`（46 项全绿）再动真机；换驱动须同步更新 `dll/arch.rs` 的哈希/字节数/导出数与 `docs/card-format.md`。
- 提交信息风格：`type(scope): 中文描述 [TD-xxx]`。

## Notes
