//! 身份证读卡服务的**应答解析**（纯函数，不碰网络）。
//!
//! # 为什么单独一个模块
//!
//! 厂商服务的报文格式不稳定：同一台机器上不同版本可能返回 `|` 分隔文本，
//! 也可能返回 JSON；字段名有英文、拼音缩写、中文三种风格。把解析从
//! [`crate::idcard_ws`] 里拆出来，才能用真实样本做单元测试而不需要读卡器。
//!
//! # 实测事实（2026-11-13，本机 `CVR100UWebSocker.exe`）
//!
//! | 场景 | 实际响应 |
//! |---|---|
//! | 读卡器上没放证件 | 文本 `读卡失败`（4 字，无分隔符） |
//! | 连接后不发命令 | 服务**不主动推送**；连接一直挂着，需要客户端超时兜底 |
//! | 未知命令字（如 `01`） | 无响应 |
//!
//! 第一条推翻了「解析不出字段就当心跳一直等」的天真实现：`读卡失败` 必须
//! **立刻**当成失败报出去，否则用户要白等满 10 秒才知道没读到。

use serde_json::Value;

use crate::idcard::IdCardData;

/// 一次应答的归类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdCardResponse {
    /// 读到了身份信息（至少姓名或证件号）。
    Identity(IdCardData),
    /// 「空壳」：服务回了完整报文但没有证件内容（多半只带设备型号）。
    ///
    /// 照常返回给调用方，由 [`IdCardData::has_identity`] 判断要不要采用。
    Shell(IdCardData),
    /// 服务明确报告失败，例如 `读卡失败`。
    ServiceFailure(String),
    /// 无法识别的内容，按心跳处理，继续等。
    Heartbeat,
}

/// 判定「服务明确报错」的关键词。
///
/// 只收明确表达失败的词：`请放卡`、`请将证件放上读卡器` 这类提示**不在此列**，
/// 它们会被当成心跳继续等。这是刻意的——把提示误判成失败会让发卡流程中断。
const FAILURE_MARKERS: [&str; 6] = ["失败", "错误", "异常", "未连接", "未就绪", "故障"];

/// 把一次应答归类。
///
/// # 判定顺序
///
/// 1. 空 / 纯空白 → [`IdCardResponse::Heartbeat`]
/// 2. 含 `|` 且段数够 → 按位置解析
/// 3. 以 `{` 或 `[` 开头 → 按 JSON 解析（含剥壳与别名归一）
/// 4. 含失败关键词 → [`IdCardResponse::ServiceFailure`]
/// 5. 其它 → 当心跳
#[must_use]
pub fn classify(text: &str) -> IdCardResponse {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return IdCardResponse::Heartbeat;
    }

    if trimmed.contains('|') {
        if let Some(data) = parse_pipe(trimmed) {
            return if data.has_identity() {
                IdCardResponse::Identity(data)
            } else {
                IdCardResponse::Shell(data)
            };
        }
    }

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Some(data) = parse_json(trimmed) {
            return if data.has_identity() {
                IdCardResponse::Identity(data)
            } else {
                IdCardResponse::Shell(data)
            };
        }
    }

    if FAILURE_MARKERS.iter().any(|m| trimmed.contains(m)) {
        return IdCardResponse::ServiceFailure(trimmed.to_string());
    }

    IdCardResponse::Heartbeat
}

/// 解析 `|` 分隔文本。
///
/// 段数不足 3 时返回 `None`：两段以内不可能是完整报文，多半是别的东西。
#[must_use]
pub fn parse_pipe(text: &str) -> Option<IdCardData> {
    let parts: Vec<&str> = text.split('|').collect();
    if parts.len() < 3 {
        return None;
    }
    // 只取前 12 段；头像（18+）连读都不读
    let get = |i: usize| -> Option<String> { parts.get(i).and_then(|s| non_empty(s)) };

    Some(IdCardData {
        device_model: get(0),
        device_serial: get(1),
        // 下标 2 是证件类型，契约里没有对应字段，跳过
        name: get(3),
        gender: get(4).map(|s| normalize_gender(&s)),
        nation: get(5),
        birth_date: get(6).map(|s| normalize_date(&s)),
        address: get(7),
        id_number: get(8),
        id_authority: get(9),
        valid_from: get(10),
        valid_to: get(11),
    })
}

/// 解析 JSON 应答，含剥壳与别名归一。
#[must_use]
pub fn parse_json(text: &str) -> Option<IdCardData> {
    let root: Value = serde_json::from_str(text).ok()?;
    let obj = unwrap_shell(&root)?;

    Some(IdCardData {
        device_model: pick(obj, &["devicemodel", "model", "设备型号", "sbxh"]),
        device_serial: pick(obj, &["deviceserial", "serial", "设备序列", "sbxlh"]),
        name: pick(obj, &["name", "姓名", "xm"]),
        gender: pick(obj, &["sex", "gender", "性别", "xb"]).map(|s| normalize_gender(&s)),
        nation: pick(obj, &["nation", "民族", "mz"]),
        birth_date: pick(obj, &["birth", "birthday", "出生", "csrq"]).map(|s| normalize_date(&s)),
        address: pick(obj, &["address", "住址", "dz"]),
        id_number: pick(obj, &["idno", "idnumber", "cardno", "身份证号", "gmsfhm"]),
        id_authority: pick(obj, &["idauthority", "签发机关", "qfjg"]),
        valid_from: pick(obj, &["validfrom", "有效期起", "yxqq"]),
        valid_to: pick(obj, &["validto", "有效期止", "yxqz"]),
    })
}

/// 剥掉 `data` / `result` / `info` 外壳，返回真正装字段的对象。
///
/// 有些版本把结果包一层；也有的直接平铺。两种都要能处理。
fn unwrap_shell(root: &Value) -> Option<&serde_json::Map<String, Value>> {
    let obj = root.as_object()?;

    // 被包在 data / result / info 里（大小写不敏感）
    for (k, v) in obj {
        let kl = k.to_lowercase();
        if matches!(kl.as_str(), "data" | "result" | "info") {
            if let Some(inner) = v.as_object() {
                return Some(inner);
            }
        }
    }
    Some(obj)
}

/// 按别名列表取值。别名比较**大小写不敏感**，兼容 `Name` / `NAME` / `name`。
fn pick(obj: &serde_json::Map<String, Value>, aliases: &[&str]) -> Option<String> {
    for (key, value) in obj {
        let key_lower = key.to_lowercase();
        if !aliases.iter().any(|a| a.to_lowercase() == key_lower) {
            continue;
        }
        let as_text = match value {
            Value::String(s) => non_empty(s),
            // 有些版本把数值型字段（如性别 1/2）直接给成数字
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        if as_text.is_some() {
            return as_text;
        }
    }
    None
}

/// 去空白；空串视为「没有这个字段」。
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 性别归一：`男/M/m/1` → `男`，`女/F/f/2` → `女`。
///
/// 认不出的原样返回——把厂商的原值留着，比编一个"未知"更有诊断价值。
#[must_use]
pub fn normalize_gender(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "男" | "m" | "male" | "1" => "男".to_string(),
        "女" | "f" | "female" | "2" => "女".to_string(),
        _ => raw.trim().to_string(),
    }
}

/// 出生日期归一成 `YYYY-MM-DD`。
///
/// 支持 `19900101`、`1990-01-01`、`1990/1/1`、`1990.1.1`。
/// 认不出就原样返回，不猜。
#[must_use]
pub fn normalize_date(raw: &str) -> String {
    let t = raw.trim();
    if t.chars().any(|c| c.is_ascii_alphabetic()) {
        return t.to_string();
    }
    // 按分隔符拆成三段；`1990/1/1` 这种不补零的写法去分隔符后只有 7 位数字，
    // 所以不能先剔分隔符再数位数——必须按结构拆。
    let parts: Vec<&str> = t.split(['-', '/', '.']).filter(|s| !s.is_empty()).collect();

    let (year, month, day) = match parts.as_slice() {
        // 带分隔符的三段式
        [y, m, d] => (*y, *m, *d),
        // 8 位无分隔：19900101
        [compact] if compact.len() == 8 && compact.chars().all(|c| c.is_ascii_digit()) => {
            (&compact[0..4], &compact[4..6], &compact[6..8])
        }
        _ => return t.to_string(),
    };

    let (Ok(y), Ok(m), Ok(d)) = (
        year.parse::<u16>(),
        month.parse::<u8>(),
        day.parse::<u8>(),
    ) else {
        return t.to_string();
    };
    // 月/日越界说明这不是日期，原样返回而不是拼一个假日期
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1900 {
        return t.to_string();
    }
    format!("{y:04}-{m:02}-{d:02}")
}

/// 解析「服务主动断开/无响应」类错误文案时用的一句话。
///
/// 放在这里而不是 `error.rs`：它只在身份证通路里用，且要带端口号等现场信息。
#[must_use]
pub fn no_card_hint() -> &'static str {
    "确认证件已放上读卡器（带照片一面朝下），并确认读卡器指示灯已亮。"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 本机实测的真实样本（`|` 分隔，12 段有效字段 + 头像）。
    ///
    /// 头像段刻意用短的占位串：**测试里也不需要真实头像**，
    /// 契约层面它必须被丢弃，长度无关紧要。
    const REAL_SAMPLE: &str = "CVR100U|4E57520492008A7F|I|张三|男|汉|19900101|\
                               北京市朝阳区某路1号|110101199001011234|\
                               北京市公安局朝阳分局|20150301|20350301|||||||aW1hZ2U=";

    #[test]
    fn pipe_sample_parses_all_fields() {
        let data = parse_pipe(REAL_SAMPLE).expect("真实样本应能解析");
        assert_eq!(data.device_model.as_deref(), Some("CVR100U"));
        assert_eq!(data.device_serial.as_deref(), Some("4E57520492008A7F"));
        assert_eq!(data.name.as_deref(), Some("张三"));
        assert_eq!(data.gender.as_deref(), Some("男"));
        assert_eq!(data.nation.as_deref(), Some("汉"));
        assert_eq!(data.birth_date.as_deref(), Some("1990-01-01"), "出生应归成 YYYY-MM-DD");
        assert_eq!(data.address.as_deref(), Some("北京市朝阳区某路1号"));
        assert_eq!(data.id_number.as_deref(), Some("110101199001011234"));
        assert_eq!(data.id_authority.as_deref(), Some("北京市公安局朝阳分局"));
        assert_eq!(data.valid_from.as_deref(), Some("20150301"));
        assert_eq!(data.valid_to.as_deref(), Some("20350301"));
    }

    /// 头像绝不能进返回结构——契约、日志、数据库都不该有它。
    #[test]
    fn pipe_sample_drops_portrait() {
        let data = parse_pipe(REAL_SAMPLE).expect("应能解析");
        let json = serde_json::to_string(&data).expect("必须可序列化");
        assert!(!json.contains("aW1hZ2U="), "头像内容出现在返回结构里");
        assert!(!json.contains("base64"));
        assert!(!json.contains("portrait"));
        // 结构体本身只有 11 个字段，没有任何一段留给头像
        assert_eq!(json.matches(':').count(), 11, "字段数应与契约一致");
    }

    /// 段数不够就不是完整报文，不能硬解。
    #[test]
    fn pipe_with_too_few_fields_is_rejected() {
        assert!(parse_pipe("CVR100U|12345").is_none());
        assert!(parse_pipe("只有一段").is_none());
    }

    /// 空壳：只有设备型号与序列号，没有证件内容。
    #[test]
    fn pipe_shell_has_no_identity() {
        let shell = "CVR100U|4E57520492008A7F|I||||||||||";
        let data = parse_pipe(shell).expect("空壳也应能解析");
        assert_eq!(data.device_model.as_deref(), Some("CVR100U"));
        assert!(!data.has_identity(), "只有设备信息不算读到人");
        assert_eq!(classify(shell), IdCardResponse::Shell(data));
    }

    /// **本机实测**：没放证件时返回纯文本 `读卡失败`，必须立刻报失败。
    ///
    /// 这条是最重要的：若按「解析不出就继续等」，用户要白等满 10 秒。
    #[test]
    fn real_failure_text_is_recognized_immediately() {
        assert_eq!(
            classify("读卡失败"),
            IdCardResponse::ServiceFailure("读卡失败".to_string())
        );
    }

    /// 「请放卡」这类提示**不能**当成失败——那是服务在提醒用户，要接着等。
    #[test]
    fn prompt_text_is_treated_as_heartbeat_not_failure() {
        assert_eq!(classify("请将证件放上读卡器"), IdCardResponse::Heartbeat);
        assert_eq!(classify(""), IdCardResponse::Heartbeat);
        assert_eq!(classify("   \n "), IdCardResponse::Heartbeat);
    }

    /// 三种键名风格都要能认出来。
    #[test]
    fn json_aliases_cover_three_styles() {
        let english = r#"{"name":"李四","sex":"F","nation":"满","birth":"19851231",
                          "address":"上海市","idno":"310101198512310001",
                          "idAuthority":"上海市公安局","validFrom":"20100101","validTo":"20300101"}"#;
        let pinyin = r#"{"xm":"李四","xb":"2","mz":"满","csrq":"1985-12-31",
                         "dz":"上海市","gmsfhm":"310101198512310001"}"#;
        let chinese = r#"{"姓名":"李四","性别":"女","民族":"满","出生":"1985/12/31",
                          "住址":"上海市","身份证号":"310101198512310001"}"#;

        for (label, text) in [
            ("english", english),
            ("pinyin", pinyin),
            ("chinese", chinese),
        ] {
            let data = parse_json(text).unwrap_or_else(|| panic!("{label} 风格应能解析"));
            assert_eq!(data.name.as_deref(), Some("李四"), "{label}");
            assert_eq!(data.gender.as_deref(), Some("女"), "{label} 性别应归一");
            assert_eq!(data.nation.as_deref(), Some("满"), "{label}");
            assert_eq!(data.birth_date.as_deref(), Some("1985-12-31"), "{label}");
            assert_eq!(data.address.as_deref(), Some("上海市"), "{label}");
            assert_eq!(
                data.id_number.as_deref(),
                Some("310101198512310001"),
                "{label}"
            );
        }
    }

    /// 结果被包在 `data` / `result` / `info` 里时要能剥壳。
    #[test]
    fn json_shell_is_unwrapped() {
        for key in ["data", "result", "info", "DATA"] {
            let text = format!(r#"{{"{key}":{{"name":"王五","idno":"110101199001011234"}}}}"#);
            let data = parse_json(&text).unwrap_or_else(|| panic!("{key} 外壳应能剥"));
            assert_eq!(data.name.as_deref(), Some("王五"), "{key}");
            assert_eq!(data.id_number.as_deref(), Some("110101199001011234"), "{key}");
        }
    }

    /// 性别归一覆盖契约要求的全部写法。
    #[test]
    fn gender_normalization_covers_all_forms() {
        for male in ["男", "M", "m", "male", "Male", "1", " 男 "] {
            assert_eq!(normalize_gender(male), "男", "{male}");
        }
        for female in ["女", "F", "f", "female", "Female", "2", " 女 "] {
            assert_eq!(normalize_gender(female), "女", "{female}");
        }
        // 认不出就原样保留，不编造
        assert_eq!(normalize_gender("X"), "X");
    }

    /// 出生日期三种格式都归一，非法值原样保留。
    #[test]
    fn date_normalization_covers_all_forms() {
        for input in ["19900101", "1990-01-01", "1990/1/1", "1990.01.01", " 1990-1-1 "] {
            assert_eq!(normalize_date(input), "1990-01-01", "{input}");
        }
        // 不是完整日期就不要强行拼
        assert_eq!(normalize_date("1990"), "1990");
        assert_eq!(normalize_date(""), "");
        assert_eq!(normalize_date("19901301"), "19901301", "13 月不是日期");
        assert_eq!(normalize_date("19900132"), "19900132", "32 日不是日期");
        assert_eq!(normalize_date("ABCD0101"), "ABCD0101");
    }

    /// 完整报文走 classify 应判为 Identity。
    #[test]
    fn full_pipe_response_is_identity() {
        match classify(REAL_SAMPLE) {
            IdCardResponse::Identity(data) => assert!(data.has_identity()),
            other => panic!("应判为 Identity，实际 {other:?}"),
        }
    }

    /// 非法 JSON 不能 panic，应退回心跳。
    #[test]
    fn malformed_json_falls_back_to_heartbeat() {
        assert_eq!(classify("{这不是合法 JSON"), IdCardResponse::Heartbeat);
        assert!(parse_json("{坏").is_none());
    }
}
