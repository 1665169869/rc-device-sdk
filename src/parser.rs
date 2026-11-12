//! 卡数据反解析：把读卡器返回的 28 字节还原成 [`CardSnapshot`]。
//!
//! # 判定顺序（严格按原 SDK `CardBuilder.Parse`）
//!
//! 顺序本身承载语义，**不能调整**：
//!
//! 1. 前 8 字节全 0 → **空白卡**
//! 2. 前两字节不是 `5B EC` → **未知卡型**
//! 3. 页2 = `12 12 12 12` → **空白卡**（已退房标记）
//! 4. 页1[2..4] = `0A 00` → 复位卡
//! 5. 页1[2..4] = `9B E5` → 清除卡
//! 6. 页1[2..4] = `F0 FF` → 总控卡（= 序号 4095）
//! 7. 页2[0] = `46` 且页2[3] = `01` → 安装卡
//! 8. 页1[2] = `7B` → 退房钟点设置卡
//! 9. 页2[0] = `06` 或 `14` → 校时卡（`14` 是旧卡的加扰残留）
//! 10. 页2[0] 高 2 位 = `11` → 日租客卡（月编码）
//! 11. 页2[0] 高 2 位 = `01` → 钟点客卡
//! 12. 页2[0] 高 1 位 = `1` → 日租客卡（天数编码）
//! 13. 其余 → 未知
//!
//! **为什么第 6 步必须在第 10 步之前**：总控卡页2[0] 也可能是 `0xC_`（月编码）
//! 或 `0x8_`（天数编码），若先按客卡判就会把总控卡读成「序号 4095 的客卡」。
//!
//! # 挂失位只对客卡有意义
//!
//! 页1[2] 低 4 位在**客卡**上是挂失标志，但在其它卡型上各有含义：
//!
//! - 校时卡：`16 × (分 & 0x0F) + 9`，恒为奇数（低 4 位恒为 9）
//! - 安装卡：固定 `0x0C`
//! - 复位/清除/总控/钟点卡：固定值的一部分
//!
//! 所以解析完后必须把非客卡的 `lost` 强制置 `false`，否则每张校时卡都会被读成"挂失卡"。

use crate::builder::{
    decode_timestamp_card_no, duration_days_code, duration_hours_code,
    CHECKOUT_HOUR_MARKER, CLEAR_PAGE1_TAIL, INSTALL_PAGE2, MASTER_PAGE1_TAIL,
    RESET_PAGE1_TAIL, TIMESYNC_PAGE2_HEAD, TIMESYNC_PAGE2_HEAD_LEGACY,
};
use crate::card::{
    decode_room_index, CardDuration, CardKind, CardSnapshot, CARD_BYTES, CARD_PREFIX,
};
use crate::error::{DeviceError, DeviceResult};

/// 已退房标记：页2 = `12 12 12 12`。
pub const CHECKED_OUT_MARKER: [u8; 4] = [0x12, 0x12, 0x12, 0x12];

/// 解析 28 字节卡数据。
///
/// # 错误
///
/// 只在长度不足 28 字节时返回 [`DeviceError::Protocol`]。
/// 卡型识别不出来**不算错误**——返回 [`CardKind::Unknown`]，由业务层提示用户。
pub fn parse(raw: &[u8]) -> DeviceResult<CardSnapshot> {
    if raw.len() < CARD_BYTES {
        return Err(DeviceError::Protocol(format!(
            "卡数据只有 {} 字节，至少需要 {CARD_BYTES} 字节",
            raw.len()
        )));
    }

    let mut out_raw = [0u8; CARD_BYTES];
    out_raw.copy_from_slice(&raw[..CARD_BYTES]);
    let p1 = [out_raw[0], out_raw[1], out_raw[2], out_raw[3]];
    let p2 = [out_raw[4], out_raw[5], out_raw[6], out_raw[7]];

    // 1) 前 8 字节全 0 → 空白卡
    if out_raw[..8].iter().all(|b| *b == 0) {
        return Ok(snapshot(CardKind::Blank, out_raw));
    }

    // 2) 魔数不符 → 未知
    if p1[0] != CARD_PREFIX[0] || p1[1] != CARD_PREFIX[1] {
        return Ok(snapshot(CardKind::Unknown, out_raw));
    }

    // 3) 已退房标记 → 空白卡
    if p2 == CHECKED_OUT_MARKER {
        return Ok(snapshot(CardKind::Blank, out_raw));
    }

    // 4-9) 管理卡按页1 / 页2 的固定标记识别（顺序不可调整）
    if p1[2] == RESET_PAGE1_TAIL[0] && p1[3] == RESET_PAGE1_TAIL[1] {
        return Ok(with_card_no(CardKind::Reset, out_raw));
    }
    if p1[2] == CLEAR_PAGE1_TAIL[0] && p1[3] == CLEAR_PAGE1_TAIL[1] {
        return Ok(with_card_no(CardKind::Clear, out_raw));
    }
    if p1[2] == MASTER_PAGE1_TAIL[0] && p1[3] == MASTER_PAGE1_TAIL[1] {
        // 总控卡：页2[3] 是年月（不带秒），页2[0] 是时长码
        let mut snap = with_card_no(CardKind::Master, out_raw);
        snap.duration = decode_duration(p2[0]);
        return Ok(snap);
    }
    if p2[0] == INSTALL_PAGE2[0] && p2[3] == INSTALL_PAGE2[3] {
        let (index, _) = decode_room_index(p1[2], p1[3]);
        let mut snap = snapshot(CardKind::Install, out_raw);
        // 安装卡低 4 位固定 0x0C，不承载挂失语义
        snap.room_index = Some(index);
        snap.lost = false;
        return Ok(snap);
    }
    if p1[2] == CHECKOUT_HOUR_MARKER {
        let mut snap = with_card_no(CardKind::CheckoutHour, out_raw);
        snap.checkout_hour = Some(p1[3]);
        return Ok(snap);
    }
    if p2[0] == TIMESYNC_PAGE2_HEAD || p2[0] == TIMESYNC_PAGE2_HEAD_LEGACY {
        return Ok(with_card_no(CardKind::TimeSync, out_raw));
    }

    // 10-12) 客卡按时长码的高位判定
    let (index, lost) = decode_room_index(p1[2], p1[3]);
    if let Some(duration) = decode_duration(p2[0]) {
        let (kind, duration) = match duration {
            CardDuration::Days(_) => (CardKind::GuestDay, duration),
            CardDuration::Hours(h) => (
                CardKind::GuestHour,
                // 客卡小时编码是 0x40|(小时+1)，解出要减回 1
                CardDuration::Hours(h.saturating_sub(1).max(1)),
            ),
        };
        let mut snap = with_card_no(kind, out_raw);
        snap.room_index = Some(index);
        snap.lost = lost;
        snap.duration = Some(duration);
        return Ok(snap);
    }

    Ok(snapshot(CardKind::Unknown, out_raw))
}

/// 按时长码首字节解出时长。
///
/// - `0xC_` → 月编码（`(b & 0x3F) × 30` 天）
/// - `0x4_` → 小时编码（客卡要再减 1，见调用方）
/// - `0x8_` → 天数编码（`b & 0x3F` 天）
/// - 其它 → `None`（不属于客卡/总控卡的时长段）
fn decode_duration(byte: u8) -> Option<CardDuration> {
    match byte & 0xC0 {
        0xC0 => Some(CardDuration::Days((byte & 0x3F) as u16 * 30)),
        0x40 => Some(CardDuration::Hours((byte & 0x3F) as u16)),
        0x80 => Some(CardDuration::Days((byte & 0x3F) as u16)),
        _ => None,
    }
}

/// 构造一个只带卡型与原始字节的快照。
fn snapshot(kind: CardKind, raw: [u8; CARD_BYTES]) -> CardSnapshot {
    CardSnapshot {
        kind,
        room_index: None,
        duration: None,
        checkout_hour: None,
        card_no: None,
        lost: false,
        stamp: None,
        year_month_byte: raw[7],
        raw,
    }
}

/// 在 [`snapshot`] 基础上尝试解出作业流水号。
///
/// 判据：页2[3] 的低 4 位是月份，落在 1–12 才认为这张卡带卡号。
/// 空白卡与安装卡不带卡号。
fn with_card_no(kind: CardKind, raw: [u8; CARD_BYTES]) -> CardSnapshot {
    let mut snap = snapshot(kind, raw);
    let ym = raw[7];
    let month = ym & 0x0F;
    if (1..=12).contains(&month) {
        let card_no = ((raw[6] as u16) << 8) | raw[5] as u16;
        if card_no != 0 {
            snap.card_no = Some(card_no);
            snap.stamp = Some(decode_timestamp_card_no(card_no));
        }
    }
    snap
}

/// 把时长反编码成时长码（供自检双向校验使用）。
pub fn encode_duration(duration: CardDuration, guest: bool) -> u8 {
    match duration {
        CardDuration::Days(d) => duration_days_code(d),
        CardDuration::Hours(h) => duration_hours_code(h, guest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::CLEAR_PAGE2_HEAD;

    /// 由 8 字节构造 28 字节原始数据。
    fn raw_from(p1: [u8; 4], p2: [u8; 4]) -> [u8; CARD_BYTES] {
        crate::builder::to_raw28(&p1, &p2)
    }

    fn hex4(b: [u8; 4]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    // ------------------------------------------------ 基础分支

    /// 前 8 字节全零 → 空白卡。
    #[test]
    fn all_zero_is_blank() {
        let snap = parse(&[0u8; CARD_BYTES]).unwrap();
        assert_eq!(snap.kind, CardKind::Blank);
        assert!(snap.is_blank());
        assert_eq!(snap.card_no, None);
    }

    /// 魔数不符 → 未知卡型（不是错误）。
    #[test]
    fn wrong_magic_is_unknown_not_error() {
        let mut raw = [0u8; CARD_BYTES];
        raw[0] = 0xAA;
        raw[1] = 0xBB;
        raw[2] = 0x12;
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Unknown);
    }

    /// 页2 = 12 12 12 12 → 已退房，按空白卡处理。
    #[test]
    fn checked_out_marker_is_blank() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], CHECKED_OUT_MARKER);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Blank);
    }

    /// 长度不足是协议错误。
    #[test]
    fn short_input_is_protocol_error() {
        let err = parse(&[0u8; 10]).unwrap_err();
        assert_eq!(err.sdk_code(), -1);
        assert!(matches!(err, DeviceError::Protocol(_)));
    }

    // ------------------------------------------------ 管理卡

    /// 四种固定页1 的管理卡。
    #[test]
    fn fixed_page1_admin_cards() {
        let raw = raw_from(
            [0x5B, 0xEC, RESET_PAGE1_TAIL[0], RESET_PAGE1_TAIL[1]],
            [0x00, 0x41, 0x24, 0xAB],
        );
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Reset);
        assert_eq!(snap.card_no, Some(0x2441));

        let raw = raw_from(
            [0x5B, 0xEC, CLEAR_PAGE1_TAIL[0], CLEAR_PAGE1_TAIL[1]],
            [CLEAR_PAGE2_HEAD, 0x42, 0x24, 0xAB],
        );
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Clear);
        assert_eq!(snap.card_no, Some(0x2442));
    }

    /// 总控卡必须先于客卡判定：页2 可能是 0xC_ 月编码。
    #[test]
    fn master_is_detected_before_guest() {
        // 总控卡 1095 天：页1 = 5BECF0FF，页2[0] = 0xE4（月编码段）
        let raw = raw_from(
            [0x5B, 0xEC, MASTER_PAGE1_TAIL[0], MASTER_PAGE1_TAIL[1]],
            [0xE4, 0xCD, 0x2B, 0xAB],
        );
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Master, "0xE4 的月编码不能被误判成客卡");
        assert_eq!(snap.duration, Some(CardDuration::Days(36 * 30)));
        assert_eq!(snap.card_no, Some(0x2BCD));
    }

    /// 总控卡页1 的 F0 FF 解出 12 位全 1 = 4095。
    #[test]
    fn master_page1_decodes_to_4095() {
        let (index, _) = decode_room_index(0xF0, 0xFF);
        assert_eq!(index, 4095);
    }

    /// 安装卡：页2 = 46 F2 00 01，低 4 位 0x0C 不算挂失。
    #[test]
    fn install_card_is_not_lost() {
        let raw = raw_from([0x5B, 0xEC, 0xFC, 0x00], INSTALL_PAGE2);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::Install);
        assert_eq!(snap.room_index, Some(15));
        assert!(!snap.lost, "安装卡固定 0x0C 不能被读成挂失");
        assert_eq!(snap.card_no, None, "安装卡不占作业流水号");
    }

    /// 退房钟点设置卡。
    #[test]
    fn checkout_hour_card() {
        let raw = raw_from([0x5B, 0xEC, CHECKOUT_HOUR_MARKER, 12], [0xFF, 0x8F, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::CheckoutHour);
        assert_eq!(snap.checkout_hour, Some(12));
        assert_eq!(snap.card_no, Some(0x228F));
    }

    /// 校时卡：新卡 0x06 与旧卡 0x14 都要认，且都不能被读成挂失。
    #[test]
    fn timesync_cards_are_never_lost() {
        for head in [TIMESYNC_PAGE2_HEAD, TIMESYNC_PAGE2_HEAD_LEGACY] {
            let raw = raw_from([0x5B, 0xEC, 0xE9, 0x21], [head, 0x00, 0x42, 0xAB]);
            let snap = parse(&raw).unwrap();
            assert_eq!(snap.kind, CardKind::TimeSync, "页2[0]=0x{head:02X} 应识别为校时卡");
            assert!(!snap.lost, "校时卡页1[2] 的 0x09 低半字节不是挂失位");
        }
    }

    // ------------------------------------------------ 客卡

    /// 日租客卡：天数编码段。
    #[test]
    fn guest_day_simple_segment() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::GuestDay);
        assert_eq!(snap.room_index, Some(15));
        assert_eq!(snap.duration, Some(CardDuration::Days(5)));
        assert_eq!(snap.card_no, Some(0x2288));
        assert!(!snap.lost);
    }

    /// 日租客卡：月编码段。
    #[test]
    fn guest_day_month_segment() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0xC3, 0x00, 0x42, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::GuestDay);
        assert_eq!(snap.duration, Some(CardDuration::Days(90)));
    }

    /// 钟点客卡：解出的小时数要减回 1。
    #[test]
    fn guest_hour_decodes_minus_one() {
        // 3 小时 → 编码 0x44
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x44, 0x89, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::GuestHour);
        assert_eq!(snap.duration, Some(CardDuration::Hours(3)));

        // 62 小时 → 编码 0x7F
        let raw62 = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x7F, 0x8C, 0x22, 0xAB]);
        assert_eq!(
            parse(&raw62).unwrap().duration,
            Some(CardDuration::Hours(62))
        );
    }

    /// 挂失位只在客卡上生效。
    #[test]
    fn lost_flag_only_applies_to_guest_cards() {
        // 客卡低 4 位 = 1 → 挂失
        let lost_guest = raw_from([0x5B, 0xEC, 0xF1, 0x00], [0x81, 0x03, 0x24, 0xAB]);
        let snap = parse(&lost_guest).unwrap();
        assert_eq!(snap.kind, CardKind::GuestDay);
        assert!(snap.lost, "客卡低 4 位为 1 就是挂失");

        // 序号 0 的客卡低 4 位 = 0 → 不挂失
        let normal = raw_from([0x5B, 0xEC, 0x00, 0x00], [0x81, 0x4A, 0x22, 0xAB]);
        let snap2 = parse(&normal).unwrap();
        assert_eq!(snap2.room_index, Some(0));
        assert!(!snap2.lost);
    }

    /// 12 位序号要能从页1 正确还原（含高位）。
    ///
    /// **4095 被排除**：序号 4095 的页1 = `5BEC F0FF`，与总控卡字节完全相同，
    /// 格式上无法区分。见 `room_index_4095_collides_with_master`。
    #[test]
    fn decodes_full_12_bit_room_index() {
        for index in [0u16, 15, 39, 255, 256, 1000, 4094] {
            let [b2, b3] = crate::card::encode_room_index(index, false).unwrap();
            let raw = raw_from([0x5B, 0xEC, b2, b3], [0x81, 0x01, 0x02, 0xAB]);
            let snap = parse(&raw).unwrap();
            assert_eq!(snap.kind, CardKind::GuestDay, "序号 {index} 被判成其它卡型");
            assert_eq!(snap.room_index, Some(index), "序号 {index} 还原失败");
        }
    }

    /// **格式内在歧义**：序号 4095 的客卡与总控卡页1 完全相同，无法区分。
    ///
    /// 页1 = `5B EC F0 FF` 既是「序号 4095 的客卡」，也是「总控卡」。
    /// 解析按原 SDK 的优先级判为**总控卡**（第 6 步早于第 10 步）。
    ///
    /// 后果：**业务层绝不能把 4095 分配给真实房间**，否则该房间的卡会被读成总控卡。
    #[test]
    fn room_index_4095_collides_with_master() {
        let [b2, b3] = crate::card::encode_room_index(4095, false).unwrap();
        assert_eq!([b2, b3], MASTER_PAGE1_TAIL, "4095 与总控卡页1 必须相同");

        let raw = raw_from([0x5B, 0xEC, b2, b3], [0x81, 0x01, 0x02, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(
            snap.kind,
            CardKind::Master,
            "格式无法区分，按优先级判为总控卡"
        );
    }

    /// 页2[3] 的月份不在 1–12 时不认为带卡号。
    #[test]
    fn invalid_month_means_no_card_no() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xA0]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::GuestDay);
        assert_eq!(snap.card_no, None, "月份 0 不是合法年月");
        assert_eq!(snap.stamp, None);
    }

    /// 解出的发卡日/时/序号要正确。
    #[test]
    fn stamp_is_decoded_from_card_no() {
        // 卡号 0x2288 = 日 4 / 时 10 / seq 8
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        let stamp = snap.stamp.expect("应有发卡时刻");
        assert_eq!(stamp.day, (0x2288u16 >> 11) as u8);
        assert_eq!(stamp.hour, ((0x2288u16 >> 6) & 0x1F) as u8);
        assert_eq!(stamp.seq, (0x2288u16 & 0x3F) as u8);
    }

    /// 年月光字节要落在快照里，供业务层用当前年还原。
    #[test]
    fn year_month_byte_is_preserved() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.year_month_byte, 0xAB);
        assert_eq!(crate::builder::restore_year(snap.year_month_byte, 2026), 2026);
    }

    /// 页3–7 的内容不影响解析（只读前 8 字节）。
    #[test]
    fn trailing_bytes_are_ignored() {
        let mut raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xAB]);
        raw[8..].fill(0xEE);
        let snap = parse(&raw).unwrap();
        assert_eq!(snap.kind, CardKind::GuestDay);
        assert_eq!(snap.room_index, Some(15));
    }

    /// 时长码往返：编码后解析回来必须一致。
    #[test]
    fn duration_roundtrip() {
        for days in [1u16, 2, 5, 30, 62, 63, 90, 120, 360, 1095] {
            let code = duration_days_code(days);
            let raw = raw_from([0x5B, 0xEC, 0x00, 0x00], [code, 0x01, 0x02, 0xAB]);
            let snap = parse(&raw).unwrap();
            match snap.duration {
                Some(CardDuration::Days(d)) => {
                    // 月编码段是有损的（×30 还原），只对简单段要求精确相等
                    if days <= 63 {
                        assert_eq!(d, days, "{days} 天往返失真");
                    } else {
                        assert!(d >= 30, "{days} 天还原出 {d} 天，明显不对");
                    }
                }
                other => panic!("{days} 天解析成 {other:?}"),
            }
        }
    }

    /// 钟点卡往返。
    #[test]
    fn hour_roundtrip() {
        for hours in [1u16, 2, 3, 6, 12, 62] {
            let code = duration_hours_code(hours, true);
            let raw = raw_from([0x5B, 0xEC, 0x00, 0x00], [code, 0x01, 0x02, 0xAB]);
            let snap = parse(&raw).unwrap();
            assert_eq!(
                snap.duration,
                Some(CardDuration::Hours(hours)),
                "{hours} 小时往返失真"
            );
        }
    }

    /// 打印一条真实字节的完整解析结果，便于人工核对。
    #[test]
    fn parse_real_sample_220_5days() {
        let raw = raw_from([0x5B, 0xEC, 0xF0, 0x00], [0x85, 0x88, 0x22, 0xAB]);
        let snap = parse(&raw).unwrap();
        assert_eq!(hex4(snap.page1()), "5BECF000");
        assert_eq!(hex4(snap.page2()), "858822AB");
        assert_eq!(snap.kind.label(), "日租客卡");
        assert_eq!(snap.room_index, Some(15));
        assert_eq!(snap.duration, Some(CardDuration::Days(5)));
        assert_eq!(snap.card_no, Some(0x2288));
    }
}
