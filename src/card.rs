//! 门锁卡硬件层的数据类型：卡型、时长、卡号、入参规格、返回快照、自检与端口。
//!
//! 这些类型的字段**全部来自硬件协议**，没有一个字段承载业务含义。
//! 特别注意：[`CardSnapshot`] **没有房号字段**——卡内只存 `room_index: u16`（12 bit，0–4095），
//! 房号文本的真值在前端 SQLite，由业务层拿 `room_index` 回查。

use serde::{Deserialize, Serialize};

use crate::error::{DeviceError, DeviceResult, DriverArch};

/// 卡数据的有效字节数。卡内只有 8 字节有效数据（页1 + 页2），
/// 但读卡器一次返回 28 字节，页 3–7 恒为 0。
pub const CARD_BYTES: usize = 28;

/// 单页字节数（页1 / 页2 各 4 字节）。
pub const PAGE_BYTES: usize = 4;

/// TEMIC 卡数据在 28 字节中的前两个字节恒为 `5B EC`。
///
/// **仅用于包识别**，不参与任何序号运算。
pub const CARD_PREFIX: [u8; 2] = [0x5B, 0xEC];

/// 房间序号最大值：12 bit → `0x0FFF` = 4095。
///
/// 这是卡格式的硬上限，**不是业务规则**（一个系统能建多少房间由业务层决定）。
pub const ROOM_INDEX_MAX: u16 = 0x0FFF;

/// 总控卡占用的房间序号：12 位全 1（`0x0FFF` = 4095）。
///
/// 现场读到页1 = `5B EC F0 FF` 时，按位打包解出的正是 4095。
/// 总控卡不属于任何单一房间，读卡后业务层不应拿它回查房间表。
pub const MASTER_ROOM_INDEX: u16 = ROOM_INDEX_MAX;

// ---------------------------------------------------------------- 卡型

/// 卡型。与原 C# SDK 的 `CardKind` 一一对应，取值来源是读卡后按卡内字节判定的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CardKind {
    /// 日租客卡：页1 = `5B EC xx xx`，页2[0] = `0x80 | 天数` 或 `0xC0 | 月数`。
    GuestDay,
    /// 钟点客卡：页1 = `5B EC xx xx`，页2[0] = `0x40 | (小时 + 1)`。
    GuestHour,
    /// 总控卡：页1 = `5B EC F0 FF`，页2[0] = `0x80 | 天数` 或 `0x40 | 小时`。
    Master,
    /// 复位卡。
    Reset,
    /// 清除卡。
    Clear,
    /// 安装卡：页2 = `46 F2 00 01`（不含作业流水号），用于把房间序号绑定到门锁。
    Install,
    /// 退房钟点设置卡：页2[0] = `0xFF`，用于把"退房钟点"写进锁。
    CheckoutHour,
    /// 校时卡：页2[0] = `0x06`（明文），编码 `YYMMDDHHmm`。
    TimeSync,
    /// 空白卡（全零或全 FF）：锁不认这张卡。
    Blank,
    /// 无法识别的卡型。解析不出来**绝不猜测**成客卡。
    Unknown,
}

impl CardKind {
    /// 读卡后展示用的中文名。与原 SDK 的 `KindText` 保持一致。
    pub fn label(self) -> &'static str {
        match self {
            Self::GuestDay => "日租客卡",
            Self::GuestHour => "钟点客卡",
            Self::Master => "总控卡",
            Self::Reset => "复位卡",
            Self::Clear => "清除卡",
            Self::Install => "安装卡",
            Self::CheckoutHour => "退房钟点设置卡",
            Self::TimeSync => "校时卡",
            Self::Blank => "空白卡",
            Self::Unknown => "未知卡型",
        }
    }

    /// 是否属于管理卡。
    ///
    /// **这只是卡型分类，不是权限判断**——是否需要管理员角色由业务层决定。
    pub fn is_admin_card(self) -> bool {
        matches!(
            self,
            Self::Master
                | Self::Reset
                | Self::Clear
                | Self::Install
                | Self::CheckoutHour
                | Self::TimeSync
        )
    }

    /// 是否携带作业流水号（卡号）。
    ///
    /// 安装卡固定 `46 F2 00 01`（不含流水号），空白/未知卡自然也无需。
    /// 其余六种（客卡日租/钟点、总控、复位、清除、校时、退房钟点）都带。
    pub fn has_card_no(self) -> bool {
        !matches!(self, Self::Install | Self::Blank | Self::Unknown)
    }

    /// 卡内是否带房间序号。管理卡里只有安装卡需要序号；客卡都需要。
    pub fn carries_room_index(self) -> bool {
        matches!(self, Self::GuestDay | Self::GuestHour | Self::Install)
    }
}

// ---------------------------------------------------------------- 房间序号位打包

/// 把房间序号与挂失标志打包进页1 的第 3、4 字节（`B2`、`B3`）。
///
/// # 位布局（这是唯一权威，不要自己另写一套）
///
/// ```text
/// 页1:  [0]=0x5B  [1]=0xEC  [2]=B2            [3]=B3
///                            │                  │
///                            │                  └─ 完整 8 bit = room_index 的高 8 位
///                            │                      即 (room_index >> 4) & 0xFF
///                            └─ 高 4 bit = room_index 的低 4 位（room_index & 0x0F）
///                               低 4 bit = lost_flag（挂失标志，不属于序号）
/// ```
///
/// 所以 `room_index` 一共 **12 bit**，值域 **0–4095**。
///
/// # 为什么不能用 `to_le_bytes` / `to_be_bytes`
///
/// `5B EC` 是**包识别魔数，恒定不变，不参与序号运算**。序号的两个字节跨在
/// `B2` 的高半字节与 `B3` 的整字节上——这是**非对齐**的位打包，既不是大端也不是小端。
/// 任何「把 `B2 B3` 拼成 u16 再解析」的写法，都会把挂失标志混进序号或让序号整体错位 4 位。
///
/// # 例（全部来自原厂实测样本）
///
/// | room_index | lost | B2 | B3 | 页1 |
/// |---|---|---|---|---|
/// | 15（220 房） | false | `0xF0` | `0x00` | `5BEC F000` |
/// | 6（209 房）  | false | `0x60` | `0x00` | `5BEC 6000` |
/// | 39（260 房） | false | `0x70` | `0x02` | `5BEC 7002` |
/// | 0（201 房）  | false | `0x00` | `0x00` | `5BEC 0000` |
/// | 3（205 房）  | false | `0x30` | `0x00` | `5BEC 3000` |
/// | 4095（总控） | false | `0xF0` | `0xFF` | `5BEC F0FF` |
///
/// 最后一行是关键：总控卡的 `B3 = 0xFF` 解出来正是 12 位全 1 = 4095，
/// 这就是「只有总控卡会把 B3 置 FF」的成因，不是特例而是位打包的自然结果。
///
/// # 适用范围
///
/// 用于**客卡**（日租、钟点）。安装卡页1[2] 的低 4 位是固定值 `0x0C`
/// （见原 SDK `BuildInstall`），不承载挂失语义，由写入实现直接构造，不走本函数。
///
/// # 错误
///
/// `room_index > 4095` 返回 [`DeviceError::Param`]（纯位宽检查，不是业务规则）。
pub fn encode_room_index(room_index: u16, lost: bool) -> DeviceResult<[u8; 2]> {
    if room_index > ROOM_INDEX_MAX {
        return Err(DeviceError::Param(format!(
            "房间序号 {room_index} 超出 12 位上限 {ROOM_INDEX_MAX}"
        )));
    }
    let low_nibble: u8 = if lost { 1 } else { 0 };
    let b2 = (((room_index & 0x0F) as u8) << 4) | low_nibble;
    let b3 = ((room_index >> 4) & 0xFF) as u8;
    Ok([b2, b3])
}

/// 从页1 的两个字节（`raw[2]`、`raw[3]`）还原房间序号与挂失标志。
///
/// 与 [`encode_room_index`] 严格互逆：
///
/// ```text
/// room_index = ((B3 as u16) << 4) | ((B2 >> 4) as u16)
/// lost_flag  = B2 & 0x0F != 0
/// ```
///
/// # 不要用魔数判断
///
/// `raw[0] == 0x5B && raw[1] == 0xEC` 只用于**识别这是不是 TEMIC 卡数据包**，
/// 绝不能参与序号运算。序号只由 `B2`、`B3` 决定。
///
/// # 只能用于哪些卡型
///
/// 位打包**仅适用于 [`CardKind::GuestDay`]、[`CardKind::GuestHour`]、[`CardKind::Install`]**。
/// 其余卡型的页1[2] 有各自固定含义，按本函数解会得到无意义的值：
///
/// | 卡型 | 页1 | `B2` | `B3` | 按本函数误解得 |
/// |---|---|---|---|---|
/// | 退房钟点设置卡 | `5BEC 7B0C` | `0x7B`（固定） | 钟点 | 无意义 |
/// | 复位卡 | `5BEC 0A00` | `0x0A`（固定） | `0x00` | 无意义 |
/// | 清除卡 | `5BEC 9BE5` | `0x9B`（固定） | `0xE5`（固定） | 无意义 |
/// | 校时卡 | `5BEC E921` | 分钟位域 | 时/分位域 | 无意义 |
///
/// 调用方必须先用 [`CardKind::carries_room_index`] 判断，再决定是否调用本函数。
///
/// # 安装卡的特殊低 4 位
///
/// 安装卡低 4 位固定 `0x0C`（原 SDK `BuildInstall`），不是挂失标志。
/// 按本函数解出的 `lost_flag` 会是 `true`，调用方应忽略它——
/// 安装卡没有「挂失」概念。
pub fn decode_room_index(b2: u8, b3: u8) -> (u16, bool) {
    let index = ((b3 as u16) << 4) | ((b2 >> 4) as u16);
    (index, (b2 & 0x0F) != 0)
}

// ---------------------------------------------------------------- 时长

/// 卡内时长。日租与钟点二选一，卡里只存一个时长码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "unit", content = "value")]
pub enum CardDuration {
    /// 天数。1–63 走 `0x80|天数`；超过 63 走 `0xC0|月数`（30 天/月）。
    Days(u16),
    /// 小时数。客卡是 `0x40|(小时+1)`，总控卡是 `0x40|小时`（差 1，实测）。
    Hours(u16),
}

impl CardDuration {
    /// 数值部分，不带单位。
    pub fn amount(self) -> u16 {
        match self {
            Self::Days(v) | Self::Hours(v) => v,
        }
    }

    /// 是否以「天」为单位。
    pub fn is_days(self) -> bool {
        matches!(self, Self::Days(_))
    }
}

// ---------------------------------------------------------------- 发卡时刻与卡号

/// 发卡时刻，只保留卡内真正用得上的精度：年、月、日、时。
///
/// 为什么是这四个：
/// - **年 + 月** → 页2[3]（高 4 位年、低 4 位月，16 年一循环）；
/// - **日 + 时** → 卡号的第 11–15 位与第 6–10 位。
///
/// 卡内**没有分钟**，所以这里不给分钟。
///
/// 硬性要求：页2[3] 与卡号必须来自**同一个** `CardIssuedAt`，
/// 否则会出现「卡号说 11 点、年月说 12 月」的自相矛盾卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardIssuedAt {
    /// 年份（完整四位，如 2026）。页2[3] 只存 `(year - 2000) & 0x0F`。
    pub year: i32,
    /// 月份 1–12。
    pub month: u8,
    /// 日 1–31。写进卡号时需要 `& 0x1F`。
    pub day: u8,
    /// 小时 0–23。写进卡号时需要 `& 0x1F`。
    pub hour: u8,
}

/// 从卡号解出的发卡时刻片段（卡内没有年月，年月在页2[3]）。
///
/// 16 位卡号 = 日(5 位) | 时(5 位) | 小时内序号(6 位)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardStamp {
    /// 日 1–31。
    pub day: u8,
    /// 小时 0–23。
    pub hour: u8,
    /// 该小时内的序号 0–62（卡内序号 = 第 n 张 − 1）。
    pub seq: u8,
}

// ---------------------------------------------------------------- 入参规格

/// 日租客卡入参。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestDaySpec {
    /// 房间序号 0–4095（12 bit）。**不是房号**，硬件层不认房号。
    ///
    /// 写入页1 时按位打包：`p[2] = ((idx & 0x0F) << 4) | lost_flag`、`p[3] = idx >> 4`。
    pub room_index: u16,
    /// 天数 1–1095。>63 天按 30 天/月换算成月数写入卡内。
    pub days: u16,
    /// 作业流水号 1–0xFFFF，由业务层生成。
    pub card_no: u16,
    /// 挂失卡：页1[2] 低 4 位写 1，让该房间在用卡失效。
    pub lost: bool,
    /// 发卡时刻，决定页2[3] 的年月。
    pub issued_at: CardIssuedAt,
}

/// 钟点客卡入参。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestHourSpec {
    /// 房间序号 0–4095（12 bit）。位打包规则同 [`GuestDaySpec::room_index`]。
    pub room_index: u16,
    /// 小时数 1–62（客卡时长码是 `0x40|(小时+1)`，占了 63 这一格）。
    pub hours: u16,
    /// 作业流水号 1–0xFFFF。
    pub card_no: u16,
    /// 挂失卡。
    pub lost: bool,
    /// 发卡时刻。
    pub issued_at: CardIssuedAt,
}

/// 总控卡入参。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterSpec {
    /// 时长。天数 1–63 / 小时 1–63（总控卡小时不加 1，与客卡不同）。
    pub duration: CardDuration,
    /// 作业流水号 1–0xFFFF。
    pub card_no: u16,
    /// 发卡时刻。
    pub issued_at: CardIssuedAt,
}

/// 安装卡入参。安装卡**不占卡号**，页2 固定 `46 F2 00 01`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSpec {
    /// 房间序号 0–4095（12 bit），写进页1 让门锁认下这个房间。
    pub room_index: u16,
}

/// 不携带房间序号的卡：复位卡、清除卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlainCardSpec {
    /// 作业流水号 1–0xFFFF。
    pub card_no: u16,
    /// 发卡时刻。
    pub issued_at: CardIssuedAt,
}

/// 退房钟点设置卡入参。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutHourSpec {
    /// 退房钟点 0–23。
    pub hour: u8,
    /// 作业流水号 1–0xFFFF。
    pub card_no: u16,
    /// 发卡时刻。
    pub issued_at: CardIssuedAt,
}

/// 校时卡入参。卡内编码的是 `YYMMDDHHmm`，但没有「日」——日是当天的日。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSyncSpec {
    /// 小时 0–23，写进页1 位域。
    pub hour: u8,
    /// 分钟 0–59。
    pub minute: u8,
    /// 作业流水号 1–0xFFFF。
    pub card_no: u16,
    /// 发卡时刻。决定页2[3] 的年月与卡号里的日/时。
    pub issued_at: CardIssuedAt,
}

// ---------------------------------------------------------------- 返回快照

/// 一张卡的硬件层快照：读卡与写卡成功后的统一返回形态。
///
/// **没有房号字段**。卡内只有 [`CardSnapshot::room_index`]，
/// 业务层拿它回查自己的房间表得到房号文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardSnapshot {
    /// 判定出的卡型。
    pub kind: CardKind,
    /// 卡内房间序号 0–4095（12 bit）。只有客卡与安装卡带序号；
    /// 总控/复位/清除/校时/空白为 `None`。
    ///
    /// 已由位打包函数还原好，业务层直接拿去回查房间表，**不要**再解析页1 字节。
    pub room_index: Option<u16>,
    /// 卡内时长。只有日租客卡、钟点客卡、总控卡有时长。
    pub duration: Option<CardDuration>,
    /// 退房钟点（0–23）。只有退房钟点设置卡有。
    pub checkout_hour: Option<u8>,
    /// 作业流水号。安装卡、空白卡、未知卡型为 `None`。
    pub card_no: Option<u16>,
    /// 挂失标志（页1[2] 低 4 位为 1）。
    pub lost: bool,
    /// 从卡号解出的发卡日/时/序号。无卡号时为 `None`。
    pub stamp: Option<CardStamp>,
    /// 页2[3]，高 4 位是 `(year - 2000) & 0x0F`、低 4 位是月份。
    ///
    /// **只给原始字节**：年份的还原（16 年一循环）需要"当前年"这个业务侧上下文，
    /// 由业务层按 `2000 + 16 × ⌊(当前年 − 2000) / 16⌋ + 高4位` 还原。
    pub year_month_byte: u8,
    /// 28 字节原始卡数据。页 3–7 恒为 0；诊断与逐字节比对都靠它。
    pub raw: [u8; CARD_BYTES],
}

impl CardSnapshot {
    /// 页1 的 4 字节（`raw[0..4]`）。
    pub fn page1(&self) -> [u8; PAGE_BYTES] {
        [self.raw[0], self.raw[1], self.raw[2], self.raw[3]]
    }

    /// 页2 的 4 字节（`raw[4..8]`）。
    pub fn page2(&self) -> [u8; PAGE_BYTES] {
        [self.raw[4], self.raw[5], self.raw[6], self.raw[7]]
    }

    /// 28 字节转大写十六进制（56 个字符），与原 SDK 的 `RawHex` 对应。
    pub fn raw_hex(&self) -> String {
        self.raw.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// 卡数据是否以 `5B EC` 开头且长度正确。
    ///
    /// 空白卡（全零 / 全 FF）不是 TEMIC 卡，会返回 `false`。
    pub fn has_temic_prefix(&self) -> bool {
        self.raw[0] == CARD_PREFIX[0] && self.raw[1] == CARD_PREFIX[1]
    }

    /// 是否空白卡。业务层据此提示"这张卡不能用于开锁"。
    pub fn is_blank(&self) -> bool {
        self.kind == CardKind::Blank
    }
}

// ---------------------------------------------------------------- 自检

/// 自检中单个用例的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestCase {
    /// 用例名，例如 `"220/5天"`、`"卡号 seq=62"`、`"DurationDays(1095)"`。
    pub name: String,
    /// 是否通过。
    pub ok: bool,
    /// 期望值。批量用例（如卡号编码）可能为空。
    pub expected: String,
    /// 实际值。通过的字节向量用例会带上实际构造出的 `页1/页2`。
    pub actual: String,
}

/// 自检报告。共 46 项：33 条字节向量 + 7 条作业流水号编码 + 6 条时长码。
///
/// 这些用例来自原厂实测样本，是「卡字节与旧软件一致」的唯一黄金基准。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfTestReport {
    /// 通过数。
    pub passed: u32,
    /// 失败数。
    pub failed: u32,
    /// 逐项明细。失败项必须能看出「期望 vs 实际」。
    pub cases: Vec<SelfTestCase>,
}

impl SelfTestReport {
    /// 是否全通过（且确实跑了用例）。
    pub fn is_all_green(&self) -> bool {
        self.failed == 0 && self.passed > 0
    }

    /// 用例总数。
    pub fn total(&self) -> u32 {
        self.passed + self.failed
    }
}

// ---------------------------------------------------------------- 端口与设备信息

/// 一次串口探测的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortProbe {
    /// 串口索引 0–7（0 = COM1）。驱动内部按 index + 1 打开。
    pub port: u8,
    /// 串口名，例如 `"COM7"`。
    pub com_name: String,
    /// 是否可用（打开 + 握手 + 选中 TEMIC 卡全部成功）。
    pub available: bool,
    /// 读卡器序列号。可用但取不到时为 `None`。
    pub reader_id: Option<String>,
    /// 不可用时的失败说明；可用时为 `None`。
    pub failure: Option<String>,
}

/// 当前使用的设备后端类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceBackend {
    /// 直接动态加载 `LC32RFRW.dll`（生产实现）。
    DirectDll,
    /// 内存中的模拟设备（测试与无硬件演示）。**不写任何真卡**。
    Mock,
}

/// 设备与运行时环境信息，用于状态条、诊断包与排障。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// 当前后端。
    pub backend: DeviceBackend,
    /// 驱动 DLL 的绝对路径。
    pub driver_path: String,
    /// 驱动文件的 SHA-256（小写十六进制）。用于比对已验证版本。
    ///
    /// 结论只对 SHA-256 `f2f483e0…bccbc0` 的 `LC32RFRW.dll` 成立，换版本需重新评估。
    pub driver_sha256: Option<String>,
    /// 驱动 PE 架构。该驱动只有 x86 版本。
    pub driver_arch: DriverArch,
    /// 当前串口索引 0–7。
    pub port: u8,
    /// 波特率（原厂固定 9600）。
    pub baud: u32,
    /// 当前是否已打开。
    pub opened: bool,
    /// 读卡器序列号，未打开或取不到时为 `None`。
    pub reader_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(kind: CardKind) -> CardSnapshot {
        let mut raw = [0u8; CARD_BYTES];
        raw[0] = 0x5B;
        raw[1] = 0xEC;
        CardSnapshot {
            kind,
            room_index: None,
            duration: None,
            checkout_hour: None,
            card_no: None,
            lost: false,
            stamp: None,
            year_month_byte: 0,
            raw,
        }
    }

    /// 卡型中文名必须与 SDK 的 `KindText` 完全一致——读卡页直接展示这些字。
    #[test]
    fn kind_labels_match_sdk() {
        assert_eq!(CardKind::GuestDay.label(), "日租客卡");
        assert_eq!(CardKind::GuestHour.label(), "钟点客卡");
        assert_eq!(CardKind::Master.label(), "总控卡");
        assert_eq!(CardKind::Reset.label(), "复位卡");
        assert_eq!(CardKind::Clear.label(), "清除卡");
        assert_eq!(CardKind::Install.label(), "安装卡");
        assert_eq!(CardKind::CheckoutHour.label(), "退房钟点设置卡");
        assert_eq!(CardKind::TimeSync.label(), "校时卡");
        assert_eq!(CardKind::Blank.label(), "空白卡");
    }

    /// 管理卡分类是卡型属性，不是权限——六种管理卡全覆盖。
    #[test]
    fn admin_card_classification() {
        for k in [
            CardKind::Master,
            CardKind::Reset,
            CardKind::Clear,
            CardKind::Install,
            CardKind::CheckoutHour,
            CardKind::TimeSync,
        ] {
            assert!(k.is_admin_card(), "{k:?} 应算管理卡");
        }
        for k in [CardKind::GuestDay, CardKind::GuestHour, CardKind::Blank] {
            assert!(!k.is_admin_card(), "{k:?} 不应算管理卡");
        }
    }

    /// 安装卡不占作业流水号：页2 固定 `46 F2 00 01`。
    #[test]
    fn install_card_has_no_card_no() {
        assert!(!CardKind::Install.has_card_no());
        assert!(CardKind::GuestDay.has_card_no());
        assert!(CardKind::CheckoutHour.has_card_no());
        assert!(!CardKind::Blank.has_card_no());
    }

    /// 只有客卡与安装卡带房间序号。
    #[test]
    fn only_room_cards_carry_room_index() {
        assert!(CardKind::GuestDay.carries_room_index());
        assert!(CardKind::GuestHour.carries_room_index());
        assert!(CardKind::Install.carries_room_index());
        assert!(!CardKind::Master.carries_room_index());
        assert!(!CardKind::Reset.carries_room_index());
        assert!(!CardKind::TimeSync.carries_room_index());
    }

    /// 页1/页2 切分必须与 28 字节布局一致。
    #[test]
    fn page_split_matches_raw_layout() {
        let mut s = snapshot(CardKind::GuestDay);
        s.raw[0] = 0x5B;
        s.raw[1] = 0xEC;
        s.raw[2] = 0xF0;
        s.raw[3] = 0x00;
        s.raw[4] = 0x85;
        s.raw[5] = 0x88;
        s.raw[6] = 0x22;
        s.raw[7] = 0xAB;
        assert_eq!(s.page1(), [0x5B, 0xEC, 0xF0, 0x00]);
        assert_eq!(s.page2(), [0x85, 0x88, 0x22, 0xAB]);
        assert_eq!(&s.raw_hex()[..8], "5BECF000");
        assert!(s.has_temic_prefix());
    }

    /// raw_hex 必须 56 个字符、全大写，与 SDK 输出可直接比对。
    #[test]
    fn raw_hex_is_uppercase_and_56_chars() {
        let s = snapshot(CardKind::GuestDay);
        let hex = s.raw_hex();
        assert_eq!(hex.len(), 56);
        assert!(hex.starts_with("5BEC"));
        assert_eq!(hex, hex.to_uppercase());
    }

    /// 空白卡不是 TEMIC 卡，前缀检查要能识别。
    #[test]
    fn blank_card_has_no_temic_prefix() {
        let mut s = snapshot(CardKind::Blank);
        s.raw = [0u8; CARD_BYTES];
        assert!(!s.has_temic_prefix());
        assert!(s.is_blank());
    }

    /// 时长单位语义清晰。
    #[test]
    fn duration_unit_helpers() {
        assert_eq!(CardDuration::Days(5).amount(), 5);
        assert_eq!(CardDuration::Hours(3).amount(), 3);
        assert!(CardDuration::Days(1).is_days());
        assert!(!CardDuration::Hours(1).is_days());
    }

    /// 自检报告的全绿判定不能把「一项都没跑」当成通过。
    #[test]
    fn empty_report_is_not_green() {
        let empty = SelfTestReport {
            passed: 0,
            failed: 0,
            cases: vec![],
        };
        assert!(!empty.is_all_green(), "没跑用例不能算通过");

        let green = SelfTestReport {
            passed: 46,
            failed: 0,
            cases: vec![],
        };
        assert!(green.is_all_green());
        assert_eq!(green.total(), 46);
    }

    /// 快照要能跨 IPC 传输，且字段名是 camelCase。
    #[test]
    fn snapshot_serializes_to_camel_case() {
        let mut s = snapshot(CardKind::GuestDay);
        s.room_index = Some(15);
        s.duration = Some(CardDuration::Days(5));
        s.card_no = Some(0x2288);
        let json = serde_json::to_string(&s).expect("必须可序列化");
        assert!(json.contains("\"roomIndex\":15"), "实际: {json}");
        assert!(json.contains("\"cardNo\""), "实际: {json}");
        assert!(!json.contains("room\""), "快照里不许出现房号字段");
    }

    /// 发卡时刻必须能完整表达页2[3] 与卡号所需的信息。
    #[test]
    fn issued_at_carries_year_month_day_hour() {
        let at = CardIssuedAt {
            year: 2026,
            month: 11,
            day: 8,
            hour: 14,
        };
        assert_eq!(at.year, 2026);
        assert_eq!(at.month, 11);
        assert_eq!(at.day, 8);
        assert_eq!(at.hour, 14);
    }

    // ------------------------------------------------ 房间序号位打包（12 bit）

    /// 33 条黄金向量里出现过的全部 room_index，逐一比对页1 的 B2/B3。
    ///
    /// 这是「12 位位打包」与实测样本一致性的核心证据。
    #[test]
    fn encode_matches_every_recorded_sample() {
        // (room_index, 页1 后两字节 as [B2, B3], 说明)
        let samples: &[(u16, [u8; 2], &str)] = &[
            (15, [0xF0, 0x00], "220 房"),
            (6, [0x60, 0x00], "209 房"),
            (39, [0x70, 0x02], "260 房"),
            (0, [0x00, 0x00], "201 房（序号 0 真实存在）"),
            (3, [0x30, 0x00], "205 房"),
        ];
        for &(idx, expected, name) in samples {
            let got = encode_room_index(idx, false).expect("合法序号应当编码成功");
            assert_eq!(got, expected, "{name}（序号 {idx}）编码不符");
        }
    }

    /// 总控卡的 `B3 = 0xFF` 不是特例，而是 12 位全 1 的自然结果。
    #[test]
    fn master_card_index_is_all_ones() {
        let got = encode_room_index(MASTER_ROOM_INDEX, false).expect("4095 应当编码成功");
        assert_eq!(got, [0xF0, 0xFF], "总控卡页1 应为 5BEC F0FF");
        assert_eq!(MASTER_ROOM_INDEX, 4095);
    }

    /// 编解码必须严格互逆，且覆盖 12 位全值域边界。
    #[test]
    fn encode_decode_roundtrip_across_12_bits() {
        // 覆盖每个半字节边界与上下限
        for idx in [
            0u16, 1, 14, 15, 16, 17, 255, 256, 257, 1000, 2047, 2048, 4094, 4095,
        ] {
            for lost in [false, true] {
                let [b2, b3] = encode_room_index(idx, lost).expect("合法序号应当编码成功");
                let (back, back_lost) = decode_room_index(b2, b3);
                assert_eq!(back, idx, "序号 {idx} 往返失真（lost={lost}）");
                assert_eq!(back_lost, lost, "序号 {idx} 的挂失标志往返失真");
            }
        }
    }

    /// 遍历全部 4096 个序号值，确保位打包无遗漏。
    #[test]
    fn all_4096_indices_roundtrip() {
        for idx in 0..=ROOM_INDEX_MAX {
            let [b2, b3] = encode_room_index(idx, false).expect("0–4095 全部合法");
            let (back, lost) = decode_room_index(b2, b3);
            assert_eq!(back, idx, "序号 {idx} 往返失真");
            assert!(!lost, "未置挂失时低 4 位必须为 0");

            // 挂失标志只占低 4 位，绝不能影响序号本身
            let [lb2, lb3] = encode_room_index(idx, true).expect("0–4095 全部合法");
            assert_eq!(lb2 >> 4, b2 >> 4, "挂失标志不许污染序号低 4 位");
            assert_eq!(lb3, b3, "挂失标志不许污染 B3");
            assert_eq!(decode_room_index(lb2, lb3), (idx, true));
        }
    }

    /// `B2 B3` 不是 u16 的大端也不是小端——必须用位打包函数。
    ///
    /// 这个用例专门防止有人"顺手"写成 `u16::from_le_bytes([b2, b3])`。
    #[test]
    fn room_index_is_not_a_naive_u16() {
        let idx = 39u16; // 260 房，B2=0x70, B3=0x02
        let [b2, b3] = encode_room_index(idx, false).expect("39 合法");

        assert_ne!(u16::from_le_bytes([b2, b3]), idx, "按小端解析会错");
        assert_ne!(u16::from_be_bytes([b2, b3]), idx, "按大端解析会错");

        // 正确解法：B3 是高位、B2 的高半字节是低位
        assert_eq!(((b3 as u16) << 4) | ((b2 >> 4) as u16), idx);
    }

    /// 超出 12 位的序号必须是参数错误，而不是静默截断。
    #[test]
    fn out_of_range_index_is_param_error() {
        let err = encode_room_index(4096, false).expect_err("4096 应被拒绝");
        assert_eq!(err.sdk_code(), 1);
        assert!(err.to_string().contains("12 位"));

        // 4095 是上限，必须通过
        assert!(encode_room_index(4095, false).is_ok());
    }

    /// 手机号段：序号 1000–4095 这些旧方案发不出的值，现在必须能正确打包。
    ///
    /// 旧 CLI 只允许 1–254，序号 0 的实测样本（201 房）根本发不出来。
    #[test]
    fn high_indices_beyond_old_255_limit_are_supported() {
        let [b2, b3] = encode_room_index(300, false).expect("300 应合法");
        assert_eq!([b2, b3], [0xC0, 0x12], "300 = 0x12C → B2=0xC0, B3=0x12");
        assert_eq!(decode_room_index(b2, b3).0, 300);
    }

    /// 魔数只做包识别，不参与序号运算——即使魔数变了，序号也不该变。
    #[test]
    fn magic_prefix_is_independent_of_index() {
        let [b2, b3] = encode_room_index(15, false).expect("15 合法");
        // 页1 完整形态
        assert_eq!([CARD_PREFIX[0], CARD_PREFIX[1], b2, b3], [0x5B, 0xEC, 0xF0, 0x00]);
        // 序号解码完全不看前两字节
        assert_eq!(decode_room_index(b2, b3), (15, false));
    }
}
