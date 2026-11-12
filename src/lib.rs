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

pub mod card;
pub mod card_device;
pub mod error;
pub mod idcard;

pub use card::{
    decode_room_index, encode_room_index, CardDuration, CardIssuedAt, CardKind, CardSnapshot,
    CardStamp, CheckoutHourSpec, DeviceBackend, DeviceInfo, GuestDaySpec, GuestHourSpec,
    InstallSpec, MasterSpec, PlainCardSpec, PortProbe, SelfTestCase, SelfTestReport, TimeSyncSpec,
    CARD_BYTES, CARD_PREFIX, MASTER_ROOM_INDEX, PAGE_BYTES, ROOM_INDEX_MAX,
};
pub use card_device::CardDevice;
pub use error::{DeviceError, DeviceErrorPayload, DeviceResult, DriverArch};
pub use idcard::{IdCardData, IdCardProbeSpec, IdCardReadSpec, IdCardReader, IdCardServiceInfo};
