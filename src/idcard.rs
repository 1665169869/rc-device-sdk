//! 身份证读卡器的接口契约。
//!
//! # 为什么和 [`crate::CardDevice`] 分开
//!
//! 这是**两个完全不同的物理设备**：
//!
//! | | 门锁卡读写器 | 身份证读卡器 |
//! |---|---|---|
//! | 硬件 | 125 kHz TEMIC 读写器 | 华视（或兼容）身份证阅读器 |
//! | 接入方式 | `LC32RFRW.dll` 动态加载 | 本机 `ws://127.0.0.1:9001` 服务 |
//! | 失败影响 | 发不了卡 | 只是不能自动填表，手填照样能登记 |
//! | 使用场景 | 发卡、擦卡、读卡校验 | 入住登记时读取旅客证件 |
//!
//! 合成一个 trait 的后果是：只想 mock 身份证读卡的测试，也得顺手实现 17 个门锁卡方法。
//! 所以拆开。两者互不引用、互不依赖。
//!
//! # 一个刻意的设计：不读头像
//!
//! 厂商服务返回的 `|` 分隔文本里，最后一项是证件照的 base64。
//! 本层**解析时直接丢弃**，不进返回结构、不进日志、不落库。
//! 原因：头像体积大（几十 KB base64）、无可用的业务场景、且属于敏感个人信息。
//!
//! # 协议
//!
//! 服务不会主动推送，必须由客户端发起读卡命令（`04`，不拿起模式）。
//! 各版本厂商服务的字段名不一致（英文 / 拼音缩写 / 中文），实现在解析层做宽容归一；
//! 归一出任何字段就当成心跳忽略，直到超时——读卡器没放卡时不能让表单卡死。
//!
//! 具体报文与字段顺序在实现条目 `plan/td-007.md` 里，本文件只定契约。

use serde::{Deserialize, Serialize};

use crate::error::DeviceResult;

/// 身份证读卡请求参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdCardReadSpec {
    /// 超时毫秒数。超时说明卡没放上去或服务无响应。
    ///
    /// 默认建议 8000–10000 ms：使用者放证 + 服务识别需要时间，
    /// 太短会被误判成"读不到"。
    pub timeout_ms: u64,
}

impl Default for IdCardReadSpec {
    fn default() -> Self {
        Self { timeout_ms: 10_000 }
    }
}

/// 探测身份证读卡服务的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdCardProbeSpec {
    /// 连接超时毫秒数。探测只是看服务在不在，不需要等证件。
    pub timeout_ms: u64,
}

impl Default for IdCardProbeSpec {
    fn default() -> Self {
        Self { timeout_ms: 2_000 }
    }
}

/// 读出的一张身份证信息。
///
/// 字段全部为 `Option`：不同厂商/版本的服务能给出的字段不一样，缺哪个就 `None`，
/// 不能因为少一个字段就判整次读卡失败。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdCardData {
    /// 姓名。
    pub name: Option<String>,
    /// 性别，已归一成 `"男"` / `"女"`（原始值可能是 `M/F`、`1/2`）。
    pub gender: Option<String>,
    /// 民族，例如 `"汉"`。
    pub nation: Option<String>,
    /// 出生日期，已归一成 `YYYY-MM-DD`。
    pub birth_date: Option<String>,
    /// 住址。
    pub address: Option<String>,
    /// 公民身份号码（18 位）。
    pub id_number: Option<String>,
    /// 签发机关。
    pub id_authority: Option<String>,
    /// 证件有效期起始，原样保留厂商给出的格式（多为 `YYYYMMDD`）。
    pub valid_from: Option<String>,
    /// 证件有效期截止，原样保留。
    pub valid_to: Option<String>,
    /// 读卡器设备型号。
    pub device_model: Option<String>,
    /// 读卡器设备序列号。
    pub device_serial: Option<String>,
}

impl IdCardData {
    /// 是否读到了有实际内容的信息（至少姓名或身份证号之一）。
    ///
    /// 只有设备型号之类的「空壳结果」应当当作什么都没读到，不要落一条空白住客。
    pub fn has_identity(&self) -> bool {
        self.name.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self
                .id_number
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }
}

/// 身份证读卡服务的探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdCardServiceInfo {
    /// 服务地址，固定为 `ws://127.0.0.1:9001`（若实现支持配置，则回显实际使用的地址）。
    pub endpoint: String,
    /// 是否连通。
    pub reachable: bool,
    /// 服务版本标识。
    ///
    /// 各版本华视服务的字段名与报文格式有差异，实现需要识别版本并在解析时走对应策略。
    /// 识别不出来时保留原始响应特征（如字段个数），不要瞎猜成某个已知版本。
    pub service_version: Option<String>,
    /// 探测失败原因；成功时为 `None`。
    pub failure: Option<String>,
}

/// 身份证读卡器。
///
/// 实现者：`HsWsIdCardReader`（本机 9001 WebSocket 客户端）与测试用的 mock。
/// 未来若改为直接调厂商 DLL，只需另写一个实现，业务层与 Tauri 命令层都不必改。
pub trait IdCardReader: Send + Sync {
    /// 发起一次读卡，等待证件放上读卡器并返回解析结果。
    ///
    /// # 错误
    ///
    /// - [`crate::DeviceError::IdCardService`] —— 连不上服务、服务主动断开、超时、
    ///   或返回内容不符合已知协议
    ///
    /// # 成功但无内容
    ///
    /// 读到「空壳」结果时**不要**报错，正常返回 [`IdCardData`]，
    /// 由调用方用 [`IdCardData::has_identity`] 判断要不要采用。
    fn read(&self, spec: &IdCardReadSpec) -> DeviceResult<IdCardData>;

    /// 探测服务是否可用，以及它的版本。
    ///
    /// 用于「设置 → 设备」页与诊断包，不读证件。
    fn probe(&self, spec: &IdCardProbeSpec) -> DeviceResult<IdCardServiceInfo>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有设备型号不算读到人——避免落一条空白住客。
    #[test]
    fn shell_result_has_no_identity() {
        let shell = IdCardData {
            device_model: Some("HS-2000".into()),
            device_serial: Some("123456".into()),
            ..Default::default()
        };
        assert!(!shell.has_identity());
    }

    /// 姓名或身份证号任一存在即为有效读取。
    #[test]
    fn name_or_id_number_counts_as_identity() {
        let by_name = IdCardData {
            name: Some("张三".into()),
            ..Default::default()
        };
        assert!(by_name.has_identity());

        let by_id = IdCardData {
            id_number: Some("110101199001011234".into()),
            ..Default::default()
        };
        assert!(by_id.has_identity());
    }

    /// 纯空白字符串不算内容。
    #[test]
    fn whitespace_only_is_not_identity() {
        let blank = IdCardData {
            name: Some("   ".into()),
            id_number: Some("".into()),
            ..Default::default()
        };
        assert!(!blank.has_identity());
    }

    /// 默认超时足够长：放证 + 识别需要时间，太短会误判成读不到。
    #[test]
    fn default_timeouts_are_reasonable() {
        assert!(IdCardReadSpec::default().timeout_ms >= 5_000);
        assert!(IdCardProbeSpec::default().timeout_ms <= 3_000);
    }

    /// 返回结构要能跨 IPC，且不包含头像这类大字段。
    #[test]
    fn id_card_data_serializes_without_portrait() {
        let data = IdCardData {
            name: Some("张三".into()),
            id_number: Some("110101199001011234".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&data).expect("必须可序列化");
        assert!(json.contains("\"idNumber\""));
        assert!(!json.contains("portrait"), "契约里不该有头像字段");
        assert!(!json.contains("base64"), "契约里不该有头像字段");
    }
}
