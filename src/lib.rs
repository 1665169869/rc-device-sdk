//! 客房微管理发卡台 · 硬件层（Hardware Layer）
//!
//! # 这一层的职责边界
//!
//! 本 crate **只做设备 I/O**：动态加载门锁读卡器驱动 `LC32RFRW.dll`、按协议打开串口、
//! 构造卡内字节、写入并回读校验、解析读出的卡数据、连接身份证读卡服务取证件信息。
//!
//! 它**不知道、也不允许知道**以下任何一件事：
//!
//! - 房号文本（`"220"`、`"总统套房"`）——硬件层只有 `room_index: u16`（12 bit，0–4095）；
//! - 房态（空净 / 空脏 / 维修 / 停用）、入住单、退房、换房；
//! - 发卡数量上限、权限与角色；
//! - 任何 SQLite 表。
//!
//! 依赖方向是**单向**的：
//!
//! ```text
//!   业务层（Vue / TS / Pinia / Drizzle）  ──invoke──▶  硬件层（本 crate）
//!                    ▲                                      │
//!                    └──────────── 返回值 ──────────────────┘
//! ```
//!
//! 硬件层拿不到、也绝不会反向调用业务层。业务规则全部在业务层；
//! 本 crate 里出现业务判断即为耦合缺陷，必须重构。
//!
//! # 对外契约
//!
//! 两个**互不相关**的物理设备，因此是两个独立 trait：
//!
//! - [`CardDevice`] —— 125 kHz TEMIC 门锁卡读写器（`LC32RFRW.dll`）；
//! - [`IdCardReader`] —— 身份证读卡器（本机 `ws://127.0.0.1:9001` 华视服务）。
//!
//! 任一设备故障都不影响另一个；测试时也各自 mock。
//!
//! # 为什么不再走 `PMSLockCard.exe`
//!
//! 旧的子进程方案要求 `RoomIndex.ini` 里存在 `房号 = 序号` 的映射，否则 CLI 直接退出
//! （`PMSLockCard.cs:178`）。那是把业务数据（房号文本）塞进硬件层的典型耦合。
//! 现在改为 Rust 直接 `LoadLibrary("LC32RFRW.dll")` 并复刻原 C# 上层的全部协议逻辑，
//! 房号文本只存在于业务层数据库，硬件层只认序号。
//!
//! # 编码纪律（crate 级强制）
//!
//! 本 crate 用 crate 级 lint 把几条规范固化下来，而不是靠自觉：
//!
//! | lint | 作用 |
//! |---|---|
//! | `deny(unsafe_code)` | 目前不需要任何 `unsafe`；TD-002 引入 P/Invoke 时在**具体模块内** `#[allow]` 并注明安全前提 |
//! | `deny(clippy::unwrap_used)` | 生产代码禁止 `unwrap()`，错误一律走 `DeviceResult` |
//! | `deny(clippy::expect_used)` | 同上，禁止 `expect()` |
//! | `deny(clippy::panic)` | 禁止 `panic!`；设备 I/O 层崩溃会直接黑掉前台界面 |
//!
//! 测试代码不受上述三条 restriction lint 约束——测试里 `unwrap()` 失败即失败，
//! 正是想要的语义，各测试模块内自行 `#[allow]`。

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(clippy::pedantic)]

// pedantic 档里**不适用**于本 crate 的 lint，逐项豁免并说明原因。
//
// 这些不是"懒得改"，而是改了对代码有害：
//
// - `module_name_repetitions`：本 crate 的模块名承载协议语义
//   （`card::CardKind`、`card_device::CardDevice`），去掉前缀反而看不出归属。
// - `missing_errors_doc`：错误分支已写在每个方法体的 `# 错误` 段或
//   `DeviceError` 变体注释里，逐个函数再写一遍是重复。
// - `must_use_candidate`：本 crate 的返回值几乎都是"用了才有意义"，
//   但设备句柄类类型加 `must_use` 会与 `Arc<dyn CardDevice>` 的用法冲突。
// - `cast_possible_truncation`：协议位域运算**刻意**先掩码再截断
//   （`(x & 0x3F) as u8`），截断由掩码保证安全，这在协议代码里是标准写法。
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

pub mod builder;
pub mod card;
pub mod card_device;
pub mod dll;
pub mod error;
pub mod idcard;
pub mod idcard_proto;
pub mod idcard_ws;
pub mod parser;
pub mod protocol;
pub mod selftest;

pub use builder::{
    build_checkout_hour, build_clear, build_guest_day, build_guest_hour, build_install,
    build_master, build_reset, build_timesync, decode_timestamp_card_no, duration_days_code,
    duration_hours_code, next_stamp, restore_year, timestamp_card_no, to_raw28, year_month_byte,
    Pages, ERASE_PAGES, WRITE_PASSWORD,
};

pub use card::{
    decode_room_index, encode_room_index, CardDuration, CardIssuedAt, CardKind, CardSnapshot,
    CardStamp, CheckoutHourSpec, DeviceBackend, DeviceInfo, DriverHashStatus, GuestDaySpec,
    GuestHourSpec, InstallSpec, MasterSpec, PlainCardSpec, PortProbe, SelfTestCase,
    SelfTestReport, TimeSyncSpec, CARD_BYTES, CARD_PREFIX, MASTER_ROOM_INDEX, PAGE_BYTES,
    ROOM_INDEX_MAX,
};
pub use card_device::CardDevice;
pub use error::{DeviceError, DeviceErrorPayload, DeviceResult, DriverArch};
pub use idcard::{IdCardData, IdCardProbeSpec, IdCardReadSpec, IdCardReader, IdCardServiceInfo};
