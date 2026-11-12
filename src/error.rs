//! 硬件层统一错误类型。
//!
//! # 为什么以 SDK 错误码为基准
//!
//! 原 C# SDK 的 `LockResult` 是 0–11 的**语义码**，而 `PMSLockCard.exe` 的**进程退出码**
//! 是另一套 0–7，两者在 2/3 上错位（`PMSLockCard.cs:463-476`）：
//!
//! | SDK 码 | 含义 | CLI 退出码 |
//! |---|---|---|
//! | 2 | 驱动 DLL 加载失败 | 2 |
//! | 3 | 串口打开失败 | 2 |
//! | 4 | 读卡器握手失败 | 2 |
//! | 5 | 无卡 / 卡型不符 | 3 |
//! | 6 | 读卡失败 | 3 |
//!
//! 也就是说「退出码 3」在 SDK 语义里是「串口打开失败」而不是「没读到卡」。
//! 旧实现在现场把这两者混进同一字段，导致「没插读卡器」被提示成「请把卡放上去」。
//!
//! 因此本层所有错误统一以 **SDK 语义码** 表达，见 [`DeviceError::sdk_code`]；
//! 硬件层自有、SDK 没有对应码的故障一律返回 `-1`，由业务层按文案处理。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 硬件层统一返回类型。
pub type DeviceResult<T> = Result<T, DeviceError>;

/// 硬件层可能发生的全部故障。
///
/// 分两类：
/// 1. **SDK 语义故障**（`sdk_code() >= 0`）—— 与原 C# SDK 的 `LockResult` 一一对应，
///    业务层可以按数字码做分支，表现与旧软件一致；
/// 2. **硬件层自有故障**（`sdk_code() == -1`）—— DLL 加载、超时、协议解析等，
///    SDK 里没有这些概念，只能按文案提示。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DeviceError {
    // ---------------- SDK 语义码 1–11 ----------------
    /// SDK 码 1：参数非法。
    ///
    /// 触发条件：`room_index > 4095`（超出 12 位为止）、`days` 不满足 1–1095、`hours` 不满足 1–63、
    /// `card_no == 0`、`hour > 23`、`minute > 59` 等入参越界。
    /// **纯数值范围检查，不含任何业务规则**（不校验房间上限、不判断房态）。
    #[error("参数非法：{0}")]
    Param(String),

    /// SDK 码 2：驱动 DLL 缺失、加载失败或缺少必需导出。
    ///
    /// 触发条件：`LoadLibrary("LC32RFRW.dll")` 返回空句柄，或任一必需导出取不到地址。
    /// 见 [`DeviceError::DriverFileMissing`] / [`DeviceError::DriverArchMismatch`] /
    /// [`DeviceError::DriverExportMissing`] 三个更精确的细分变体（`sdk_code()` 同为 2）。
    #[error("驱动 DLL 加载失败（检查驱动目录）：{0}")]
    DriverNotLoaded(String),

    /// SDK 码 3：串口打开失败。
    ///
    /// 触发条件：`mif_selecom(port, baud) != 0`。
    /// 现场最常见原因是原厂门锁软件（`NacreHis.exe`）占着同一个串口。
    #[error("串口打开失败：COM{port}（可能被其它门锁软件占用）")]
    PortOpenFailed {
        /// 串口索引，0 表示 COM1。
        port: u8,
    },

    /// SDK 码 4：读卡器握手失败。
    ///
    /// 触发条件：`mif_shake()` 首次失败，且 `config_parameter(0)` 后重试仍失败。
    #[error("读卡器握手失败（检查 USB 连接与供电）")]
    HandshakeFailed,

    /// SDK 码 5：没有检测到卡片，或卡型不符。
    ///
    /// 触发条件：`mif_selecard(5)`（5 = TEMIC）返回非 0；即卡没放上去，或放的不是 125 kHz TEMIC 卡。
    #[error("未检测到卡片，或卡片不是 125 kHz TEMIC 卡")]
    NoCard,

    /// SDK 码 6：读卡失败。
    ///
    /// 触发条件：`te_read_asc` 返回非 0，或读回的 HEX 串不足 56 个字符（28 字节）。
    #[error("读卡失败：{0}")]
    ReadFailed(String),

    /// SDK 码 7：写页失败。
    ///
    /// 触发条件：`te_setuppwm_asc` 或任一 `te_writepwm_asc` 返回非 0。
    #[error("写卡失败：{0}")]
    WriteFailed(String),

    /// SDK 码 8：写后回读校验失败。
    ///
    /// 触发条件：写完后的 3 次回读（间隔 200 ms）里，前 8 字节始终与期望值不一致。
    /// 卡可能没放稳、已损坏，或被其它系统加过密。
    #[error("写后回读校验失败（卡可能没放稳或已损坏）")]
    VerifyFailed,

    /// SDK 码 9：卡号无效。
    ///
    /// 触发条件：`card_no == 0` 或超出 `0xFFFF`。
    /// **注意**：卡号由业务层生成后传入，本层只做数值合法性检查，不生成、不自增、不持久化。
    #[error("作业流水号无效：{0}")]
    InvalidCardNo(u16),

    /// SDK 码 10：该卡型不支持。
    ///
    /// 触发条件：读出的卡字节无法归入任何已知卡型，或调用了当前设备不支持的写入方式。
    #[error("该卡型当前不支持：{0}")]
    Unsupported(String),

    /// SDK 码 11：卡号状态文件读写失败。**已废弃，当前实现绝不会产生它。**
    ///
    /// # 为什么废弃
    ///
    /// 直接加载 DLL 的方案**不使用任何状态文件**。卡号水位由业务层数据库维护：
    /// 硬件层只接收业务层算好的 `card_no: u16` 并校验范围，既不读也不写任何
    /// 水位文件。旧版 `PMSLockCard.exe` 用 `RoomIndex.ini` 一类文件记卡号，
    /// 那套做法在本项目里已被彻底移除。
    ///
    /// # 为什么还留着这个变体
    ///
    /// 只为**与原 SDK 的 `LockResult` 错误码表保持一一对应**。分析旧系统日志或
    /// 诊断包时，码 11 需要能翻译回「卡号状态文件错误」。删掉变体会让码表出现
    /// 空洞，对照关系就此失传。
    ///
    /// # 给后续维护者
    ///
    /// 不要构造这个变体。若将来真的需要文件状态，应当新开一个语义明确的变体
    /// （状态文件具体存什么？为何不能进数据库？），而不是复用这个。
    #[deprecated(
        note = "新版直接加载驱动，不使用任何状态文件；此变体仅为兼容旧 SDK 错误码 11 而保留，不要构造"
    )]
    #[error("卡号状态文件读写失败：{0}")]
    StateFile(String),

    // ---------------- 硬件层自有故障（sdk_code() == -1） ----------------
    /// 驱动文件不存在。
    ///
    /// 触发条件：驱动目录下找不到 `LC32RFRW.dll`。
    #[error("找不到驱动文件：{0}")]
    DriverFileMissing(String),

    /// 驱动位数不匹配。
    ///
    /// 触发条件：`LC32RFRW.dll` 是 32 位 PE，而宿主进程是 64 位（或反之）。
    /// 该驱动只有 x86 版本，因此宿主必须按 32 位目标构建。
    #[error("驱动位数不匹配：DLL 为 {dll}，宿主为 {host}（该驱动只有 x86 版本）")]
    DriverArchMismatch {
        /// DLL 实际的 PE 架构，例如 `"x86"`。
        dll: DriverArch,
        /// 宿主进程的架构，例如 `"x86_64"`。
        host: DriverArch,
    },

    /// 驱动缺少必需的导出函数。
    ///
    /// 触发条件：`GetProcAddress` 取不到某个必需函数名。
    /// 通常意味着驱动被换成了不兼容的版本。
    #[error("驱动缺少导出函数：{0}（驱动版本可能与已验证版本不一致）")]
    DriverExportMissing(String),

    /// 设备尚未打开就调用了需要已打开状态的操作。
    ///
    /// 触发条件：未调用 `open` 或 `open` 失败后继续调用读/写方法。
    #[error("设备尚未打开")]
    NotOpen,

    /// 操作超时。
    ///
    /// 触发条件：驱动调用在规定时间内没有返回。驱动是同步阻塞调用，
    /// 实现侧需用超时机制兜底，避免界面永久卡死。
    #[error("设备操作超时（{0} 秒）")]
    Timeout(u64),

    /// 设备输出无法解析。
    ///
    /// 触发条件：读回的字节长度不对、HEX 串非法、或卡数据不满足任何已知格式。
    /// **解析不出来一定是错误，绝不"猜测成功"**。
    #[error("设备输出无法解析：{0}")]
    Protocol(String),

    /// 底层 I/O 故障。
    ///
    /// 触发条件：驱动文件读取失败、句柄无效等无法归入上述分类的系统级错误。
    #[error("设备 I/O 故障：{0}")]
    Io(String),

    /// 身份证读卡服务不可用或返回异常。
    ///
    /// 触发条件：连不上 `ws://127.0.0.1:9001`、服务主动断开、读卡超时、
    /// 返回内容不符合 `|` 分隔协议或 JSON 协议。
    /// 仅 [`crate::IdCardReader`] 会产生此变体。
    #[error("身份证读卡服务异常：{0}")]
    IdCardService(String),
}

/// PE 架构标识，用于报告驱动与宿主的位数是否匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DriverArch {
    /// 32 位（`LC32RFRW.dll` 的实际架构）。
    X86,
    /// 64 位。
    X86_64,
    /// 无法判定（文件读不出来，或 PE 头不识别）。
    Unknown,
}

impl std::fmt::Display for DriverArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::X86 => f.write_str("x86"),
            Self::X86_64 => f.write_str("x86_64"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

impl DeviceError {
    /// 映射回原 C# SDK 的 `LockResult` 语义码（0–11）。
    ///
    /// 返回 `-1` 表示 SDK 里没有对应概念（DLL 缺失、超时、协议错误等），
    /// 业务层应改用 [`DeviceError::hint`] 的文案提示用户。
    ///
    /// 多个硬件层自有变体映射到同一个 SDK 码是**有意为之**：前端对
    /// 「驱动加载失败」只需一种表现，细分信息保留在本层供诊断包使用。
    ///
    /// # 关于 `StateFile` 的 `allow(deprecated)`
    ///
    /// [`DeviceError::StateFile`] 已废弃（新版不碰任何状态文件），但它仍要在这张
    /// 码表里占住 11——否则旧系统日志里的码 11 就翻译不回来了。这里是**唯一**
    /// 允许引用该变体的地方，`allow` 是有意为之，不是遗漏。
    #[allow(deprecated)]
    pub fn sdk_code(&self) -> i32 {
        match self {
            Self::Param(_) => 1,
            Self::DriverNotLoaded(_)
            | Self::DriverFileMissing(_)
            | Self::DriverArchMismatch { .. }
            | Self::DriverExportMissing(_) => 2,
            // 未打开也按"串口打不开"上报：与原 SDK 的 IsOpen 检查行为一致（ErrPort = 3）
            Self::PortOpenFailed { .. } | Self::NotOpen => 3,
            Self::HandshakeFailed => 4,
            Self::NoCard => 5,
            Self::ReadFailed(_) => 6,
            Self::WriteFailed(_) => 7,
            Self::VerifyFailed => 8,
            Self::InvalidCardNo(_) => 9,
            Self::Unsupported(_) => 10,
            Self::StateFile(_) => 11,
            Self::Timeout(_) | Self::Protocol(_) | Self::Io(_) | Self::IdCardService(_) => -1,
        }
    }

    /// 稳定的故障分类标识（`snake_case`），供业务层做埋点与诊断包分组。
    ///
    /// 与 Rust 变体名解耦：变体名可以随重构变化，这个字符串不能变。
    ///
    /// `allow(deprecated)` 的理由同 [`Self::sdk_code`]：`"state_file"` 这个字符串
    /// 已经是对外契约的一部分，即使变体废弃也不能把字符串从 match 里删掉。
    #[allow(deprecated)]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Param(_) => "param",
            Self::DriverNotLoaded(_) => "driver_not_loaded",
            Self::PortOpenFailed { .. } => "port_open_failed",
            Self::HandshakeFailed => "handshake_failed",
            Self::NoCard => "no_card",
            Self::ReadFailed(_) => "read_failed",
            Self::WriteFailed(_) => "write_failed",
            Self::VerifyFailed => "verify_failed",
            Self::InvalidCardNo(_) => "invalid_card_no",
            Self::Unsupported(_) => "unsupported",
            Self::StateFile(_) => "state_file",
            Self::DriverFileMissing(_) => "driver_file_missing",
            Self::DriverArchMismatch { .. } => "driver_arch_mismatch",
            Self::DriverExportMissing(_) => "driver_export_missing",
            Self::NotOpen => "not_open",
            Self::Timeout(_) => "timeout",
            Self::Protocol(_) => "protocol",
            Self::Io(_) => "io",
            Self::IdCardService(_) => "id_card_service",
        }
    }

    /// 给现场人员看的处理建议。业务层直接展示这段文本，不再自行拼文案。
    ///
    /// `allow(deprecated)`：`StateFile` 虽然不会产生，但 `payload()` 对它仍要
    /// 给出可读文案——万一将来有人从旧数据里还原出这个错误，界面不该显示空白。
    #[allow(deprecated)]
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Param(_) => "检查传入的房间序号、时长、卡号是否在允许范围内。",
            Self::DriverNotLoaded(_)
            | Self::DriverFileMissing(_)
            | Self::DriverExportMissing(_) => {
                "检查运行时目录下是否存在 IOServer/LC2000/LC32RFRW.dll，且未被安全软件隔离。"
            }
            Self::DriverArchMismatch { .. } => {
                "该驱动只有 32 位版本，程序必须按 32 位目标构建（i686）。"
            }
            Self::PortOpenFailed { .. } => {
                "串口打不开。多半是原厂门锁软件占着串口，先退出它再试；也可以换一个串口。"
            }
            Self::HandshakeFailed => "读卡器握手失败。检查 USB 线是否插好、读卡器供电是否正常。",
            Self::NoCard => "请把卡片放到读卡器上，并确认它是一张 125 kHz TEMIC 卡。",
            Self::ReadFailed(_) => "读卡失败。把卡在读卡器上放稳后重试。",
            Self::WriteFailed(_) => "写卡失败。换一张卡试试，或检查卡是否被其它系统加过密。",
            Self::VerifyFailed => "写完回读不一致。把卡放稳重试；多次失败说明卡已损坏，换一张。",
            Self::InvalidCardNo(_) => "作业流水号无效，请刷新后重新发卡。",
            Self::Unsupported(_) => "当前设备不支持这张卡的写入方式，请导出诊断包反馈。",
            Self::StateFile(_) => "卡号状态文件读写失败（新版直接加载驱动，不应出现此错误）。",
            Self::NotOpen => "设备尚未就绪，请先打开读卡器再重试。",
            Self::Timeout(_) => "设备无响应。检查读卡器连接后重试；反复出现请重启程序。",
            Self::Protocol(_) => "读卡器返回了无法识别的内容。请导出诊断包反馈。",
            Self::Io(_) => "与设备通信失败，请重试或重启程序。",
            Self::IdCardService(_) => {
                "身份证读卡服务不可用。请确认华视读卡服务已启动并占用本机 9001 端口。"
            }
        }
    }

    /// 是否属于「读卡器本身有问题」这一类（业务层据此把设备状态条标红）。
    ///
    /// 无卡、读卡失败只是操作层面的问题，不算设备故障。
    pub fn is_device_fault(&self) -> bool {
        matches!(
            self,
            Self::DriverNotLoaded(_)
                | Self::DriverFileMissing(_)
                | Self::DriverArchMismatch { .. }
                | Self::DriverExportMissing(_)
                | Self::PortOpenFailed { .. }
                | Self::HandshakeFailed
                | Self::NotOpen
                | Self::Timeout(_)
                | Self::Io(_)
        )
    }

    /// 转成可跨 Tauri IPC 传递的稳定结构。
    ///
    /// 不直接给枚举加 `Serialize`：那会把 Rust 变体名变成前端契约的一部分，
    /// 以后重命名变体就成了破坏性变更。
    pub fn payload(&self) -> DeviceErrorPayload {
        DeviceErrorPayload {
            kind: self.kind().to_string(),
            sdk_code: self.sdk_code(),
            message: self.to_string(),
            hint: self.hint().to_string(),
            device_fault: self.is_device_fault(),
        }
    }
}

/// [`DeviceError`] 的 IPC 传输形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceErrorPayload {
    /// 稳定分类标识，见 [`DeviceError::kind`]。
    pub kind: String,
    /// 原 SDK 语义码 0–11；`-1` 表示 SDK 无对应概念。
    pub sdk_code: i32,
    /// 面向用户的完整错误描述。
    pub message: String,
    /// 处理建议。
    pub hint: String,
    /// 是否为设备本身的故障（而非操作失误）。
    pub device_fault: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// SDK 码必须与原 C# `LockResult` 完全对齐，否则前端的错误表现会漂移。
    ///
    /// 含已废弃的 `StateFile`（码 11）：码表对齐关系本身不能因为变体废弃而失效。
    #[allow(deprecated)]
    #[test]
    fn sdk_codes_match_original_lock_result() {
        assert_eq!(DeviceError::Param("x".into()).sdk_code(), 1);
        assert_eq!(DeviceError::DriverNotLoaded("x".into()).sdk_code(), 2);
        assert_eq!(DeviceError::PortOpenFailed { port: 6 }.sdk_code(), 3);
        assert_eq!(DeviceError::HandshakeFailed.sdk_code(), 4);
        assert_eq!(DeviceError::NoCard.sdk_code(), 5);
        assert_eq!(DeviceError::ReadFailed("x".into()).sdk_code(), 6);
        assert_eq!(DeviceError::WriteFailed("x".into()).sdk_code(), 7);
        assert_eq!(DeviceError::VerifyFailed.sdk_code(), 8);
        assert_eq!(DeviceError::InvalidCardNo(0).sdk_code(), 9);
        assert_eq!(DeviceError::Unsupported("x".into()).sdk_code(), 10);
        assert_eq!(DeviceError::StateFile("x".into()).sdk_code(), 11);
    }

    /// 驱动类故障统一归 SDK 码 2，前端只需一种表现。
    #[test]
    fn driver_faults_share_sdk_code_two() {
        for e in [
            DeviceError::DriverNotLoaded("x".into()),
            DeviceError::DriverFileMissing("x".into()),
            DeviceError::DriverArchMismatch {
                dll: DriverArch::X86,
                host: DriverArch::X86_64,
            },
            DeviceError::DriverExportMissing("te_read_asc".into()),
        ] {
            assert_eq!(e.sdk_code(), 2, "{e:?} 应映射到 SDK 码 2");
            assert!(e.is_device_fault());
        }
    }

    /// 硬件层自有故障不冒充 SDK 码。
    #[test]
    fn non_sdk_faults_report_minus_one() {
        for e in [
            DeviceError::Timeout(15),
            DeviceError::Protocol("x".into()),
            DeviceError::Io("x".into()),
            DeviceError::IdCardService("x".into()),
        ] {
            assert_eq!(e.sdk_code(), -1, "{e:?} 不应有 SDK 码");
        }
    }

    /// 缺导出时消息必须带函数名——现场据此判断是哪个符号没了。
    ///
    /// 「驱动版本不兼容」这四个字帮不上任何忙；`te_writepwm_asc` 才是能拿去比对
    /// 导出表的信息。
    #[test]
    fn export_missing_message_carries_function_name() {
        let e = DeviceError::DriverExportMissing("te_writepwm_asc".into());
        assert!(
            e.to_string().contains("te_writepwm_asc"),
            "Display 应带函数名：{e}"
        );

        let p = e.payload();
        assert!(p.message.contains("te_writepwm_asc"), "IPC 载荷也要带函数名");
        assert_eq!(p.kind, "driver_export_missing");
        assert_eq!(p.sdk_code, 2);
        assert!(p.device_fault);
    }

    /// 错误文案里不能出现业务词汇——分层边界的最后一道防线。
    ///
    /// 硬件层不认识房号、房态、入住。一旦这些词出现在错误消息或处理建议里，
    /// 说明有人把业务判断塞进了 Rust 侧。
    ///
    /// `allow(deprecated)`：已废弃的 `StateFile` 也在检查范围内，
    /// 免得它暗中带进业务词而没人发现。
    #[allow(deprecated)]
    #[test]
    fn error_text_never_mentions_business_concepts() {
        let all = [
            DeviceError::Param("x".into()),
            DeviceError::DriverNotLoaded("x".into()),
            DeviceError::DriverFileMissing("x".into()),
            DeviceError::DriverArchMismatch {
                dll: DriverArch::X86,
                host: DriverArch::X86_64,
            },
            DeviceError::DriverExportMissing("x".into()),
            DeviceError::NotOpen,
            DeviceError::Timeout(15),
            DeviceError::PortOpenFailed { port: 2 },
            DeviceError::HandshakeFailed,
            DeviceError::NoCard,
            DeviceError::ReadFailed("x".into()),
            DeviceError::WriteFailed("x".into()),
            DeviceError::VerifyFailed,
            DeviceError::InvalidCardNo(0),
            DeviceError::Unsupported("x".into()),
            DeviceError::StateFile("x".into()),
            DeviceError::Protocol("x".into()),
            DeviceError::Io("x".into()),
            DeviceError::IdCardService("x".into()),
        ];
        // 「退房」不在禁词里：`CheckoutHour`（退房钟点卡）是硬件卡型名，属协议词汇。
        let forbidden = ["房号", "房态", "入住", "发卡上限", "住客", "客史"];
        for e in all {
            let p = e.payload();
            for w in forbidden {
                assert!(!p.message.contains(w), "{e:?} 的消息里出现了业务词「{w}」");
                assert!(!p.hint.contains(w), "{e:?} 的建议里出现了业务词「{w}」");
            }
        }
    }

    /// 无卡不是设备故障，只是操作问题——设备状态条不该因此变红。
    #[test]
    fn no_card_is_not_a_device_fault() {
        assert!(!DeviceError::NoCard.is_device_fault());
        assert!(!DeviceError::VerifyFailed.is_device_fault());
        assert!(DeviceError::HandshakeFailed.is_device_fault());
    }

    /// `kind()` 是跨版本稳定的字符串，一旦发布不能改。
    #[test]
    fn kind_strings_are_stable() {
        assert_eq!(DeviceError::NoCard.kind(), "no_card");
        assert_eq!(
            DeviceError::DriverArchMismatch {
                dll: DriverArch::X86,
                host: DriverArch::X86_64
            }
            .kind(),
            "driver_arch_mismatch"
        );
        assert_eq!(DeviceError::VerifyFailed.kind(), "verify_failed");
    }

    /// 已废弃的 `StateFile` 仍占住 SDK 码 11，且 `kind` 字符串不变。
    ///
    /// 码表是**对外契约**：旧系统的日志与诊断包里可能出现码 11，
    /// 它必须能被翻译回 `"state_file"`。变体废弃不等于契约作废。
    #[allow(deprecated)]
    #[test]
    fn deprecated_state_file_keeps_its_sdk_code() {
        let e = DeviceError::StateFile("x".into());
        assert_eq!(e.sdk_code(), 11, "码 11 不能因废弃而失效");
        assert_eq!(e.kind(), "state_file", "kind 字符串不能因废弃而改");
        assert!(!e.is_device_fault(), "状态文件问题不是设备故障");
        // payload 仍要能构造，且文案不为空（界面不该显示空白）
        let p = e.payload();
        assert_ne!(p.message.trim(), "", "消息不能为空");
        assert_ne!(p.hint.trim(), "", "处理建议不能为空");
    }

    /// 每个变体都必须有非空的处理建议。
    ///
    /// `allow(deprecated)`：已废弃的 `StateFile` 也要有文案，理由见 [`DeviceError::hint`]。
    #[allow(deprecated)]
    #[test]
    fn every_variant_has_a_hint() {
        let samples = [
            DeviceError::Param("x".into()),
            DeviceError::DriverNotLoaded("x".into()),
            DeviceError::PortOpenFailed { port: 0 },
            DeviceError::HandshakeFailed,
            DeviceError::NoCard,
            DeviceError::ReadFailed("x".into()),
            DeviceError::WriteFailed("x".into()),
            DeviceError::VerifyFailed,
            DeviceError::InvalidCardNo(0),
            DeviceError::Unsupported("x".into()),
            DeviceError::StateFile("x".into()),
            DeviceError::DriverFileMissing("x".into()),
            DeviceError::DriverArchMismatch {
                dll: DriverArch::X86,
                host: DriverArch::X86_64,
            },
            DeviceError::DriverExportMissing("x".into()),
            DeviceError::NotOpen,
            DeviceError::Timeout(1),
            DeviceError::Protocol("x".into()),
            DeviceError::Io("x".into()),
            DeviceError::IdCardService("x".into()),
        ];
        for e in samples {
            assert!(!e.hint().trim().is_empty(), "{e:?} 缺少处理建议");
        }
    }

    /// payload 是 IPC 契约：字段必须可序列化且包含全部决策信息。
    #[test]
    fn payload_carries_everything_frontend_needs() {
        let p = DeviceError::PortOpenFailed { port: 6 }.payload();
        assert_eq!(p.kind, "port_open_failed");
        assert_eq!(p.sdk_code, 3);
        assert!(p.device_fault);
        assert!(p.hint.contains("串口"));
        let json = serde_json::to_string(&p).expect("payload 必须可序列化");
        assert!(json.contains("\"sdkCode\":3"));
    }
}
