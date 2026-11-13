# rc-device

客房门锁卡读写器与身份证读卡器的 Rust SDK（Windows）。

本仓库从「客房微管理发卡台」项目抽离而来，只包含硬件层：动态加载
`LC32RFRW.dll`、按协议读写 125 kHz TEMIC 卡、解析卡内字节、连接本机
身份证读卡服务。它不含数据库，也不含房态、入住、权限等任何业务规则。

## 能力

| 设备 | 接入方式 | 模块 |
|---|---|---|
| 125 kHz TEMIC 门锁卡读写器 | 动态加载 `LC32RFRW.dll` | `dll::DirectDllDevice` + `CardDevice` |
| 身份证读卡器 | 本机 `ws://127.0.0.1:9001` 华视服务 | `idcard_ws::HsWsIdCardReader` + `IdCardReader` |
| 无硬件时的内存模拟 | 进程内 `SimCardDevice`（`--sim` 启用） | `sim::SimCardDevice` + `CardDevice` |

前两种设备互不相关：任一故障都不影响另一个，测试时也各自 mock。
模拟后端实现同一个 `CardDevice` 契约，字节仍由 `builder` 生成、由 `parser` 读回，
所以没有读卡器也能跑完整发卡流程。

支持的卡型：客卡（日租 / 钟点）、总控卡、安装卡、复位卡、清除卡、
退房钟点设置卡、校时卡，以及擦卡与**原始直写**（把 8 字节原样写进卡，
破坏性接口，仅供逆向验证与现场排障）。

### 不做什么

- 不处理房号文本、房态、入住单、退房/换房；
- 不做发卡数量上限、权限、角色；
- 不访问 SQLite 或任何数据库；
- 不自动重试写卡（重试会把同一张卡写两遍、卡号推进两次）。

## 环境要求

- Windows（驱动是 Win32 DLL）；
- Rust 1.77+；
- 驱动只有 **x86** 版本，运行时必须按 32 位构建：

```powershell
rustup target add i686-pc-windows-msvc
cargo build --target i686-pc-windows-msvc
```

位数不匹配会返回 `DeviceError::DriverArchMismatch`，而不是含糊的系统错误 193。

## 快速开始

```rust
use std::path::Path;

use rc_device::dll::DirectDllDevice;
use rc_device::{timestamp_card_no, CardDevice, CardIssuedAt, GuestDaySpec};

fn issue_guest_card() -> Result<(), rc_device::DeviceError> {
    let dev = DirectDllDevice::with_auto_port(Path::new("drivers/LC2000"));
    dev.open()?;

    let now = CardIssuedAt { year: 2026, month: 11, day: 14, hour: 15 };
    let card_no = timestamp_card_no(&now, 1)?;

    let card = dev.write_guest_day(&GuestDaySpec {
        room_index: 15, // 12 bit；0–4094 可分配给真实房间
        days: 1,
        card_no,
        lost: false,
        issued_at: now,
    })?;

    println!("已写入：{}", card.raw_hex());
    Ok(())
}
```

`room_index` 是硬件层唯一的「身份」：房号文本由业务层自己维护，
读写卡时只在两者之间做映射。**不要分配 4095**——它与总控卡字节相同。

## 驱动

驱动随仓库分发：

```text
drivers/LC2000/LC32RFRW.dll
drivers/LC2000/IODesc.ini
```

已验证版本：SHA-256 `f2f483e08ba8048ffd1c57e66cb35d9e32c1c1e280dc5f2ff100a5d881bccbc0`、
102,492 字节、92 个导出。卡格式结论只对这个版本成立，换驱动需重新评估。
`src/dll/arch.rs` 的测试会在哈希、架构、字节数、导出数不符时立刻失败。

## 离线自检（46 项）

自检不打开串口，也不需要插卡：

```rust
let report = dev.selftest()?;
assert!(report.is_all_green(), "{} 项失败", report.failed);
```

33 条字节向量 + 7 条作业流水号编码 + 6 条时长码。改任何编码逻辑后先让它全绿。

## 测试

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
```

真机测试默认跳过，需要插读卡器并放卡：

```powershell
$env:RC_HW_TEST=1
cargo test --test real_hardware --target i686-pc-windows-msvc -- --nocapture --test-threads=1
```

联调工具：

```powershell
cargo run --bin probe --target i686-pc-windows-msvc
```

## 错误模型

所有错误统一为 `DeviceError`：

- `sdk_code()`：与原 SDK `LockResult` 对齐的 0–11；
- `kind()`：稳定的分类字符串；
- `hint()`：给现场人员看的中文建议；
- `payload()`：可直接序列化为 IPC 响应的结构。

写卡失败**不会自动重试**；驱动调用超时同样只隔离会话，不重试。

## 目录

```text
src/card.rs          硬件类型与卡快照
src/card_device.rs   CardDevice trait
src/error.rs         DeviceError
src/builder.rs       卡字节构造 / 卡号 / 时长码 / 原始直写
src/parser.rs        28 字节反解析
src/selftest.rs      46 项自检
src/protocol.rs      协议常量
src/dll/             LC32RFRW.dll 动态加载、PE 校验、超时
src/sim.rs           内存模拟后端（--sim，不碰硬件）
src/idcard*.rs       身份证读卡器 trait、协议解析、9001 WS 客户端
src/bin/probe.rs     联调工具（真机 / --sim）
tests/               契约可测性、真机回归、sidecar 协议
docs/card-format.md  卡内字节格式
```

## 作为 sidecar 运行（64 位宿主）

Tauri 或任何 64 位程序都加载不了 32 位驱动，因此门锁卡操作可以交给本仓库的
`rc-device-sidecar` 二进制：

```powershell
cargo build --bin rc-device-sidecar --target i686-pc-windows-msvc
```

它常驻运行，按 NDJSON（一行一个 JSON）读 stdin、写 stdout：

```text
{"id":1,"op":"read_card"}
{"id":2,"op":"write_guest_day","spec":{...}}
{"id":3,"op":"shutdown"}
```
响应形如：

```text
{"id":1,"ok":true,"result":{...}}
{"id":1,"ok":false,"error":{"kind":"no_card","sdkCode":5,"hint":"…","message":"…"}}
```

`error` 就是 `DeviceErrorPayload`，与进程内调用给前端的形状完全一致。
调用方不必自己拼 JSON：`rc_device::sidecar` 里的 `Request` / `Response` /
`Operation` 就是协议本身。驱动目录按 `--driver-dir` > 环境变量 `RC_DRIVER_DIR`
> exe 同目录 `drivers/LC2000` 解析。

口径提醒：sidecar 只是把「谁碰硬件」换了个进程，**不改变**「驱动只有 32 位」
这个事实。64 位 sidecar 会直接返回 `driver_arch_mismatch`。

## 集成

其他 Rust 项目直接引用：

```toml
[dependencies]
rc-device = { path = "../rc-device-sdk" }
```

## 许可

MIT。
