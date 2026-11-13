//! sidecar 协议：64 位壳层 ⇄ 32 位设备宿主进程的 NDJSON 通信。
//!
//! # 为什么需要这一层
//!
//! `LC32RFRW.dll` 只有 x86 版本，Windows 不允许 64 位进程加载 32 位 DLL。
//! 想让 Tauri 主程序按 64 位构建，就必须把门锁卡操作放进一个 32 位子进程。
//! 本模块只定义**线上形状**，不碰进程、不碰硬件——两侧共用同一份类型，
//! 协议漂移会直接变成编译错误。
//!
//! # 线上规则
//!
//! - 一行一个 JSON 对象，UTF-8，`\n` 结尾；空行忽略。
//! - 请求与响应靠 `id` 配对，`id` 由客户端自增。
//! - stdout **只允许**出现响应行；日志一律走 stderr，避免搅乱响应流。
//! - `result` 是各命令自己的返回结构；失败时没有 `result`，只有 `error`。
//!
//! # 为什么错误用 [`DeviceErrorPayload`] 而不是 `DeviceError`
//!
//! `DeviceError` 是 `thiserror` 枚举，`kind()` 返回 `&'static str`，无法承载
//! 远端传来的任意分类。`DeviceErrorPayload` 本来就是给 IPC 用的稳定结构
//! （`kind` / `sdkCode` / `hint` / `deviceFault`），前端拿到的形状与进程内调用
//! 完全一致，不需要为 sidecar 再写一套错误处理。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::card::{
    CheckoutHourSpec, GuestDaySpec, GuestHourSpec, InstallSpec, MasterSpec, PlainCardSpec, RawSpec,
    TimeSyncSpec,
};
use crate::error::{DeviceError, DeviceErrorPayload};

/// 请求序号。客户端自增，响应原样带回。
pub type RequestId = u64;

/// 一条请求：`{"id":1,"op":"read_card"}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// 由客户端分配，响应按它配对。
    pub id: RequestId,
    /// 具体操作，`op` 字段与 [`Operation`] 的变体一一对应。
    #[serde(flatten)]
    pub body: Operation,
}

/// 门锁卡设备的全部操作，与 [`CardDevice`](crate::CardDevice) 的方法一一对应。
///
/// `Shutdown` 不是设备操作，是进程控制：收到后写回响应并退出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Operation {
    /// 打开设备（加载 DLL、开串口、握手）。
    #[serde(rename = "open")]
    Open,
    /// 关闭设备。
    #[serde(rename = "close")]
    Close,
    /// 查询是否已打开。
    #[serde(rename = "is_open")]
    IsOpen,
    /// 读一张卡。
    #[serde(rename = "read_card")]
    ReadCard,
    /// 擦卡。
    #[serde(rename = "erase_card")]
    EraseCard,
    /// 原始直写：把 8 字节原样写进卡（破坏性，仅供逆向/排障）。
    ///
    /// 与其它写卡操作不同，这里没有卡型语义——`spec` 就是最终写入的字节。
    #[serde(rename = "write_raw")]
    WriteRaw {
        /// 页1/页2 的原始字节。
        spec: RawSpec,
    },
    /// 写日租客卡。
    #[serde(rename = "write_guest_day")]
    WriteGuestDay {
        /// 写卡参数。
        spec: GuestDaySpec,
    },
    /// 写钟点客卡。
    #[serde(rename = "write_guest_hour")]
    WriteGuestHour {
        /// 写卡参数。
        spec: GuestHourSpec,
    },
    /// 写总控卡。
    #[serde(rename = "write_master")]
    WriteMaster {
        /// 写卡参数。
        spec: MasterSpec,
    },
    /// 写安装卡。
    #[serde(rename = "write_install")]
    WriteInstall {
        /// 写卡参数。
        spec: InstallSpec,
    },
    /// 写复位卡。
    #[serde(rename = "write_reset")]
    WriteReset {
        /// 写卡参数。
        spec: PlainCardSpec,
    },
    /// 写清除卡。
    #[serde(rename = "write_clear")]
    WriteClear {
        /// 写卡参数。
        spec: PlainCardSpec,
    },
    /// 写退房钟点设置卡。
    #[serde(rename = "write_checkout_hour")]
    WriteCheckoutHour {
        /// 写卡参数。
        spec: CheckoutHourSpec,
    },
    /// 写校时卡。
    #[serde(rename = "write_timesync")]
    WriteTimesync {
        /// 写卡参数。
        spec: TimeSyncSpec,
    },
    /// 探测串口 0–7。
    #[serde(rename = "probe_ports")]
    ProbePorts,
    /// 切换串口。
    #[serde(rename = "set_port")]
    SetPort {
        /// 串口索引 0–7。
        port: u8,
    },
    /// 跑离线自检。
    #[serde(rename = "selftest")]
    Selftest,
    /// 查设备与环境信息。
    #[serde(rename = "device_info")]
    DeviceInfo,
    /// 让 sidecar 退出。收到后先回响应再退出。
    #[serde(rename = "shutdown")]
    Shutdown,
}

impl Operation {
    /// 线上 `op` 字符串。与 serde 的 rename 保持一致，供日志使用。
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::IsOpen => "is_open",
            Self::ReadCard => "read_card",
            Self::EraseCard => "erase_card",
            Self::WriteRaw { .. } => "write_raw",
            Self::WriteGuestDay { .. } => "write_guest_day",
            Self::WriteGuestHour { .. } => "write_guest_hour",
            Self::WriteMaster { .. } => "write_master",
            Self::WriteInstall { .. } => "write_install",
            Self::WriteReset { .. } => "write_reset",
            Self::WriteClear { .. } => "write_clear",
            Self::WriteCheckoutHour { .. } => "write_checkout_hour",
            Self::WriteTimesync { .. } => "write_timesync",
            Self::ProbePorts => "probe_ports",
            Self::SetPort { .. } => "set_port",
            Self::Selftest => "selftest",
            Self::DeviceInfo => "device_info",
            Self::Shutdown => "shutdown",
        }
    }
}

impl Request {
    /// 组装一条请求。
    #[must_use]
    pub fn new(id: RequestId, body: Operation) -> Self {
        Self { id, body }
    }

    /// 编码成一行（不含行尾换行）。
    ///
    /// # 错误
    ///
    /// 只有序列化本身失败才会返回 `Err`；本类型的字段都能序列化，
    /// 实际不会发生。
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 从一行文本解析。
    ///
    /// # 错误
    ///
    /// JSON 非法、缺 `id`、`op` 不认识或参数形状不对，都返回 `Err`。
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// 一条响应：`{"id":1,"ok":true,"result":null}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// 对应请求的 `id`。
    pub id: RequestId,
    /// 是否成功。
    pub ok: bool,
    /// 成功时的返回值；无返回值的命令为 `null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 失败时的结构化错误。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<DeviceErrorPayload>,
}

impl Response {
    /// 成功响应。
    #[must_use]
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// 失败响应。
    #[must_use]
    pub fn failure(id: RequestId, error: DeviceErrorPayload) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    /// 编码成一行（不含行尾换行）。
    ///
    /// # 错误
    ///
    /// 同 [`Request::to_line`]，实际不会发生。
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// 从一行文本解析。
    ///
    /// # 错误
    ///
    /// JSON 非法或缺 `id` 时返回 `Err`。
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// 取出返回值；失败时把远端错误原样交回。
    ///
    /// # 错误
    ///
    /// - 响应带 `error`：直接返回它，`kind` / `sdkCode` / `hint` 一个不丢；
    /// - 返回值形状与 `T` 不符：按 [`DeviceError::Protocol`] 报错。
    pub fn into_result<T: DeserializeOwned>(self) -> Result<T, DeviceErrorPayload> {
        if let Some(err) = self.error {
            return Err(err);
        }

        let value = self.result.unwrap_or(Value::Null);
        serde_json::from_value(value)
            .map_err(|e| DeviceError::Protocol(format!("sidecar 响应无法解析：{e}")).payload())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::card::{CardIssuedAt, CardSnapshot, PortProbe, SelfTestReport};

    fn issued_at() -> CardIssuedAt {
        CardIssuedAt {
            year: 2026,
            month: 11,
            day: 14,
            hour: 15,
        }
    }

    /// 最简请求的线上形状：只有 `id` 与 `op`。
    #[test]
    fn read_card_request_shape() {
        let req = Request::new(1, Operation::ReadCard);
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({"id": 1, "op": "read_card"})
        );
        assert_eq!(req.to_line().unwrap(), r#"{"id":1,"op":"read_card"}"#);
    }

    /// 带参数的命令必须把 `spec` 放在 `op` 同一层。
    #[test]
    fn write_guest_day_round_trips() {
        let spec = GuestDaySpec {
            room_index: 15,
            days: 1,
            card_no: 0x1234,
            lost: false,
            issued_at: issued_at(),
        };
        let req = Request::new(7, Operation::WriteGuestDay { spec });
        let line = req.to_line().unwrap();
        assert!(line.contains(r#""op":"write_guest_day""#), "{line}");
        assert!(line.contains(r#""spec":"#), "{line}");

        let back = Request::from_line(&line).unwrap();
        assert_eq!(back, req);
    }

    /// 原始直写的线上形状：`spec` 里是两段 4 字节数组，不是十六进制字符串。
    ///
    /// 这条钉住「字节就是字节」——若哪天有人把它改成 `"p1":"5BECF000"`，
    /// 壳层解析会静默错位，而这是唯一会写废卡的入口。
    #[test]
    fn write_raw_carries_byte_arrays() {
        use crate::card::RawSpec;

        let spec = RawSpec {
            p1: [0x5B, 0xEC, 0xF0, 0x00],
            p2: [0x81, 0x04, 0x24, 0xAB],
        };
        let req = Request::new(11, Operation::WriteRaw { spec });
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({
                "id": 11,
                "op": "write_raw",
                "spec": {"p1": [91, 236, 240, 0], "p2": [129, 4, 36, 171]}
            })
        );

        let back = Request::from_line(&req.to_line().unwrap()).unwrap();
        assert_eq!(back, req);
    }

    /// `set_port` 的参数是裸字段，不是 `spec`。
    #[test]
    fn set_port_carries_bare_port_field() {
        let req = Request::new(2, Operation::SetPort { port: 3 });
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({"id": 2, "op": "set_port", "port": 3})
        );
    }

    /// 未知 `op` 必须解析失败，而不是被当成空操作。
    #[test]
    fn unknown_op_is_rejected() {
        assert!(Request::from_line(r#"{"id":1,"op":"format_disk"}"#).is_err());
    }

    /// 成功响应带 `result`，失败响应只带 `error`。
    #[test]
    fn response_shapes_are_distinct() {
        let ok = Response::success(1, serde_json::json!({"passed": 46}));
        assert_eq!(
            ok.to_line().unwrap(),
            r#"{"id":1,"ok":true,"result":{"passed":46}}"#
        );

        let bad = Response::failure(2, DeviceError::NoCard.payload());
        let line = bad.to_line().unwrap();
        assert!(line.contains(r#""ok":false"#), "{line}");
        assert!(line.contains(r#""kind":"no_card""#), "{line}");
        assert!(line.contains(r#""sdkCode":5"#), "{line}");
        assert!(!line.contains("\"result\""), "失败时不该有 result：{line}");
    }

    /// 失败响应经 `into_result` 必须原样透出远端错误。
    #[test]
    fn error_passes_through_into_result() {
        let resp = Response::failure(9, DeviceError::NoCard.payload());
        let err = resp.into_result::<CardSnapshot>().unwrap_err();
        assert_eq!(err.kind, "no_card");
        assert_eq!(err.sdk_code, 5);
        assert!(!err.device_fault, "无卡属于操作问题，不是设备故障");
    }

    /// `()` 结果能从 `null` 反序列化——`open` / `close` 走这条路径。
    #[test]
    fn unit_result_decodes_from_null() {
        let resp = Response::success(3, Value::Null);
        resp.into_result::<()>().unwrap();
    }

    /// 结果形状不符时按协议错误上报，而不是 panic。
    #[test]
    fn mismatched_result_is_protocol_error() {
        let resp = Response::success(4, serde_json::json!({"unexpected": true}));
        let err = resp.into_result::<SelfTestReport>().unwrap_err();
        assert_eq!(err.kind, "protocol");
    }

    /// 每个变体的 `op` 字符串都要与 serde 输出一致——加变体忘了改名会被这条抓住。
    #[test]
    fn operation_names_match_wire_format() {
        let cases = [
            (Operation::Open, "open"),
            (Operation::Close, "close"),
            (Operation::IsOpen, "is_open"),
            (Operation::ReadCard, "read_card"),
            (Operation::EraseCard, "erase_card"),
            (Operation::ProbePorts, "probe_ports"),
            (Operation::Selftest, "selftest"),
            (Operation::DeviceInfo, "device_info"),
            (Operation::Shutdown, "shutdown"),
        ];

        for (op, expected) in cases {
            assert_eq!(op.name(), expected);
            let req = Request::new(1, op);
            let json = serde_json::to_value(&req).unwrap();
            assert_eq!(json["op"], expected);
        }
    }

    /// 探端口返回的数组必须能解回 `Vec<PortProbe>`。
    #[test]
    fn probe_ports_result_decodes() {
        let probes = vec![PortProbe {
            port: 3,
            com_name: "COM4".to_string(),
            available: true,
            reader_id: Some("reader".to_string()),
            failure: None,
        }];
        let resp = Response::success(5, serde_json::to_value(&probes).unwrap());
        let back = resp.into_result::<Vec<PortProbe>>().unwrap();
        assert_eq!(back, probes);
    }
}
