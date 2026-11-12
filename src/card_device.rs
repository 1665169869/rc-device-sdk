//! 门锁卡读写器（125 kHz TEMIC）的接口契约。
//!
//! # 设计说明
//!
//! ## 一、这个 trait 只描述硬件能力
//!
//! 每一个方法都对应驱动 `LC32RFRW.dll` 上一组确定的调用序列，除此之外不做任何事：
//!
//! | 方法 | 底层调用序列 |
//! |---|---|
//! | [`CardDevice::open`] | `mif_selecom` → `mif_shake` → `config_parameter(0)` → `mif_selecard(5)` |
//! | [`CardDevice::read_card`] | `te_read_asc` → 取前 28 字节 → 按卡内字节判定卡型 |
//! | `write_*` | `te_setuppwm_asc` → `te_writepwm_asc(页1)` → `te_writepwm_asc(页2)` → 回读校验 |
//! | [`CardDevice::erase_card`] | 同上，两页均写全零 |
//! | [`CardDevice::probe_ports`] | 对 0–7 逐个尝试 `open`，成功的留下 |
//! | [`CardDevice::selftest`] | 纯离线：只跑字节构造与编码用例，**不碰硬件** |
//!
//! 这里**没有**、也不会有这些东西：
//!
//! - ❌ 房号字符串（只有 `room_index: u16`，12 bit，0–4095）
//! - ❌ 发卡数量上限、房态、入住单、退房/换房
//! - ❌ 卡号生成与自增（卡号由业务层生成后传入，本层只校验数值范围）
//! - ❌ 任何数据库访问
//!
//! ## 二、入参只保留协议必需字段
//!
//! 每个 `write_*` 方法的入参是一个 `*Spec` 结构体，字段数正好等于写卡协议需要的个数。
//! 不传业务 ID、不传房号标签、不传房态——即使传了本层也用不上。
//!
//! 唯一的「身份」是 `room_index: u16`（12 bit）：
//!
//! ```text
//! 业务层（SQLite）              硬件层（本 trait）
//! room_no = "220"   ──查表──▶   room_index = 15   ──写卡──▶  卡内 0x0F
//! ```
//!
//! 反方向也一样：读卡只回 `room_index = 15`，房号 `"220"` 由业务层回查。
//! 这样硬件层永远不需要知道「房号」这个概念存在。
//!
//! ## 三、为什么 `&self` + `Send + Sync`
//!
//! 驱动是进程级串口资源，同一时刻只能有一个实例在用。方法全部取 `&self`：
//!
//! - 可以直接放进 `Arc<dyn CardDevice>` 共享给多个 Tauri 命令；
//! - 内部可变状态（已加载的 DLL 句柄、串口是否打开）由实现自己用锁保护；
//! - 测试里可以写出零状态的 `MockCardDevice`，不必为每个测试开串口。
//!
//! ## 四、会话由实现管理，但状态显式可查
//!
//! 打开串口是耗时操作（实测 `selecom` 后要等 150 ms 才握手）。实现可以按需开关，
//! 但必须如实反映在 [`CardDevice::is_open`] 上，让业务层能判断「现在能不能立刻写卡」。
//!
//! ## 五、错误一律是 [`DeviceError`]
//!
//! 不允许实现返回 `anyhow::Error` 或自造错误类型。原因见 `error.rs` 的模块注释：
//! 错误码要能映射回原 SDK 的 `LockResult` 0–11，前端表现才能与旧软件一致。
//!
//! # 实现者须知
//!
//! 1. **不要自动重试写卡**。写卡是硬件动作，重试可能把同一张卡写两遍、
//!    把卡号推进两次。失败就返回错误，让前台人工确认。
//! 2. **回读校验的重试是协议的一部分**（最多 3 次，间隔 200 ms），不是"容错"。
//!    写完立刻读可能拿到旧值，这是实测结论。
//! 3. **`config_parameter(0)` 是明文模式**，必须在写卡前调用。加扰模式下会出现
//!    「能读不能写」。
//! 4. **不要为了省事再引入配置文件**。房号→序号的映射是业务数据，
//!    已随 `PMSLockCard.exe` 一起废弃。

use crate::card::{
    CheckoutHourSpec, DeviceInfo, GuestDaySpec, GuestHourSpec, InstallSpec, MasterSpec,
    PlainCardSpec, PortProbe, SelfTestReport, TimeSyncSpec,
};
use crate::error::DeviceResult;
use crate::CardSnapshot;

/// 125 kHz TEMIC 门锁卡读写器。
///
/// 实现者：`DirectDllDevice`（动态加载 `LC32RFRW.dll`）与测试用的 `MockCardDevice`。
pub trait CardDevice: Send + Sync {
    // ------------------------------------------------------------ 会话

    /// 打开设备：加载驱动 DLL、打开串口、握手、选中 TEMIC 卡型。
    ///
    /// 幂等：已打开时直接返回 `Ok(())`，不重复握手。
    ///
    /// # 错误
    ///
    /// - [`DeviceError::DriverFileMissing`] —— 驱动目录下没有 `LC32RFRW.dll`
    /// - [`DeviceError::DriverArchMismatch`] —— DLL 与宿主位数不一致
    /// - [`DeviceError::DriverExportMissing`] —— 驱动版本不兼容，缺少必需导出
    /// - [`DeviceError::PortOpenFailed`] —— 串口被占用（关掉原厂门锁软件再试）
    /// - [`DeviceError::HandshakeFailed`] —— 读卡器没连好或没供电
    fn open(&self) -> DeviceResult<()>;

    /// 关闭设备：通知驱动关闭串口并卸载 DLL。幂等，未打开时也是 `Ok`。
    fn close(&self) -> DeviceResult<()>;

    /// 当前是否已打开（已握手且已选中 TEMIC 卡型）。
    fn is_open(&self) -> bool;

    // ------------------------------------------------------------ 读

    /// 读一张卡，返回 28 字节解析后的快照。
    ///
    /// 卡上没卡返回 [`DeviceError::NoCard`]；卡不是 TEMIC 卡也返回 `NoCard`。
    /// 空白卡（全零/全 FF）正常返回 [`CardKind::Blank`]，**不当作错误**。
    fn read_card(&self) -> DeviceResult<CardSnapshot>;

    // ------------------------------------------------------------ 写：客卡

    /// 写一张日租客卡。
    ///
    /// 天数 1–63 编码成 `0x80|天数`；超过 63 天按 **30 天/月** 编码成 `0xC0|月数`
    /// （锁按日历月执行，90 天 → 3 月、1095 天 → 36 月）。
    fn write_guest_day(&self, spec: &GuestDaySpec) -> DeviceResult<CardSnapshot>;

    /// 写一张钟点客卡。
    ///
    /// 小时 1–62 编码成 `0x40|(小时+1)`。上限是 62 而不是 63：
    /// 客卡的小时编码加 1 占掉了 63 这一格。
    fn write_guest_hour(&self, spec: &GuestHourSpec) -> DeviceResult<CardSnapshot>;

    // ------------------------------------------------------------ 写：管理卡

    /// 写一张总控卡。页1 = `5B EC F0 FF`。
    ///
    /// 与客卡的区别：小时编码是 `0x40|小时`，**不加 1**（实测差一位）。
    fn write_master(&self, spec: &MasterSpec) -> DeviceResult<CardSnapshot>;

    /// 写一张安装卡：把房间序号绑定到门锁。
    ///
    /// 安装卡页2 固定 `46 F2 00 01`，**不含作业流水号**，因此不需要 `card_no`。
    /// 写错房间需要清除卡纠正。
    fn write_install(&self, spec: &InstallSpec) -> DeviceResult<CardSnapshot>;

    /// 写一张复位卡。复位卡本身不设防：任何持卡人都能让旧卡复活，测试后不要留在现场。
    fn write_reset(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot>;

    /// 写一张清除卡。刷清除卡可能让门锁变成「未安装」状态。
    fn write_clear(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot>;

    /// 写一张退房钟点设置卡：把「几点退房」写进门锁。
    ///
    /// 之后该锁的日租卡到期时间 = 发卡日 + N 天的这个钟点。
    fn write_checkout_hour(&self, spec: &CheckoutHourSpec) -> DeviceResult<CardSnapshot>;

    /// 写一张校时卡：把当前时间校准到门锁。
    ///
    /// 卡内编码 `YYMMDDHHmm`：年月在页2[3]、日在卡号日段、时分在页1 位域；
    /// 页2 首字节固定 `0x06`（明文）。
    fn write_timesync(&self, spec: &TimeSyncSpec) -> DeviceResult<CardSnapshot>;

    /// 擦卡：把页1、页2 全部写零，锁不再认这张卡。
    ///
    /// **不消耗作业流水号**（空白卡不含卡号）。成功后返回 [`CardKind::Blank`] 快照。
    ///
    /// 注意：这只是把卡擦白，不会让门锁里已有的授权消失。
    fn erase_card(&self) -> DeviceResult<CardSnapshot>;

    // ------------------------------------------------------------ 环境

    /// 探测串口 0–7，返回每一个的可用性。
    ///
    /// 返回空 `Vec` 表示一个都没找到（不是错误——没插读卡器是常见状态）。
    /// 单项的失败原因放在 [`PortProbe::failure`] 里。
    fn probe_ports(&self) -> DeviceResult<Vec<PortProbe>>;

    /// 切换串口索引 0–7。
    ///
    /// 实现应当关闭旧串口再用新端口重新打开；若新端口打不开，必须回到可用状态
    /// 或明确置为未打开，不能留下「以为在用新端口，其实还连着旧的」的假状态。
    ///
    /// # 错误
    ///
    /// 索引 > 7 返回 [`DeviceError::Param`]；新端口打开失败按 [`CardDevice::open`] 的错误返回。
    fn set_port(&self, port: u8) -> DeviceResult<()>;

    /// 跑一遍离线自检，共 46 项：33 条字节向量 + 7 条作业流水号编码 + 6 条时长码。
    ///
    /// **不碰硬件、不需要插卡**，随时可跑。这是「卡字节与旧软件一致」的唯一黄金基准，
    /// 改任何编码逻辑后必须先让它全绿。
    fn selftest(&self) -> DeviceResult<SelfTestReport>;

    /// 当前设备与环境信息：驱动路径与哈希、位数、端口、是否已打开、读卡器序列号。
    ///
    /// 用于顶栏设备状态条与诊断包。**不打开设备也能调用**（可能显示未打开）。
    fn device_info(&self) -> DeviceResult<DeviceInfo>;
}
