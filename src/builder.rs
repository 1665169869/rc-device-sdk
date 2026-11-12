//! 卡字节构造：复刻原 C# `CardBuilder` 的全部构包逻辑。
//!
//! 本模块是**纯函数**：给定入参返回 4 字节页1 + 4 字节页2，不碰硬件、不碰数据库、
//! 不含任何业务规则。所有范围检查都只是**协议位宽检查**（例如 12 位序号上限 4095），
//! 不涉及"这间房能发几张卡"这类业务判断。
//!
//! # 卡内布局
//!
//! ```text
//! 页1 = 5B EC | B2                        | B3
//!              高4bit = room_index 低4位    room_index >> 4
//!              低4bit = lost_flag
//! 页2 = 时长码 | 卡号低 | 卡号高 | 年月
//! ```
//!
//! `5B EC` 是包识别魔数，恒定不变，不参与序号运算。序号的位打包统一走
//! [`crate::card::encode_room_index`]，**不要在本模块里手写位运算**。

use crate::card::{
    encode_room_index, CardDuration, CardIssuedAt, CheckoutHourSpec, GuestDaySpec, GuestHourSpec,
    InstallSpec, MasterSpec, PlainCardSpec, TimeSyncSpec, CARD_PREFIX, ROOM_INDEX_MAX,
};
use crate::error::{DeviceError, DeviceResult};

/// 原厂固定写密码（TEMIC 卡写保护密码）。
///
/// 写入时以**十六进制 ASCII 字符串**传给 `te_setuppwm_asc` / `te_writepwm_asc`，
/// 即"12000206" 这 8 个字符。
pub const WRITE_PASSWORD: &str = "12000206";

/// 安装卡页2 固定值。安装卡不含作业流水号。
pub const INSTALL_PAGE2: [u8; 4] = [0x46, 0xF2, 0x00, 0x01];

/// 安装卡页1 低 4 位的固定值（不是挂失标志）。
///
/// 原 SDK `BuildInstall` 里硬编码 `0x0C`。安装卡没有"挂失"概念。
pub const INSTALL_LOW_NIBBLE: u8 = 0x0C;

/// 复位卡页1 后两字节。
pub const RESET_PAGE1_TAIL: [u8; 2] = [0x0A, 0x00];

/// 清除卡页1 后两字节。
pub const CLEAR_PAGE1_TAIL: [u8; 2] = [0x9B, 0xE5];

/// 清除卡页2 首字节。
pub const CLEAR_PAGE2_HEAD: u8 = 0xC7;

/// 总控卡页1 后两字节（= 12 位序号全 1，即 4095）。
pub const MASTER_PAGE1_TAIL: [u8; 2] = [0xF0, 0xFF];

/// 退房钟点设置卡页1 第 3 字节（固定标记）。
pub const CHECKOUT_HOUR_MARKER: u8 = 0x7B;

/// 退房钟点设置卡页2 首字节。
pub const CHECKOUT_HOUR_PAGE2_HEAD: u8 = 0xFF;

/// 校时卡页2 首字节（明文，IDA: `LOBYTE(v14[2]) = 6`）。
pub const TIMESYNC_PAGE2_HEAD: u8 = 0x06;

/// 校时卡旧卡首字节（加扰残留，读卡时要能认）。
pub const TIMESYNC_PAGE2_HEAD_LEGACY: u8 = 0x14;

// ---------------------------------------------------------------- 年月字节

/// 计算页2[3] 的「年 + 月」字节。
///
/// ```text
/// 高 4 位 = (year - 2000) & 0x0F
/// 低 4 位 = month & 0x0F
/// ```
///
/// **16 年一循环**：2026-11 = `0xAB`、2027-11 = `0xBB`、2042-11 又是 `0xAB`。
pub fn year_month_byte(at: &CardIssuedAt) -> u8 {
    // 用取模而不是直接 & 0x0F，避免 year < 2000 时出现负数补码的意外值
    let year_part = (((at.year - 2000) % 16 + 16) % 16) as u8;
    let month_part = at.month & 0x0F;
    (year_part << 4) | month_part
}

/// 从页2[3] 还原完整年份（读卡侧用，需要业务层提供"当前年"作锚点）。
///
/// ```text
/// year = 2000 + 16 × ⌊(当前年 − 2000) / 16⌋ + 高4位
/// ```
///
/// 这是 16 年循环的还原规则（原厂 `sub_1000260C`）。业务层必须传入**当前年**，
/// 因为卡里只有 4 位年份，单靠卡本身无法区分 2026 与 2042。
pub fn restore_year(year_month_byte: u8, current_year: i32) -> i32 {
    let high = (year_month_byte >> 4) as i32;
    2000 + 16 * ((current_year - 2000) / 16) + high
}

// ---------------------------------------------------------------- 时长码

/// 天数 → 时长码。
///
/// | 天数 | 编码 |
/// |---|---|
/// | 1–63 | `0x80 \| days` |
/// | > 63 | `0xC0 \| round(days / 30)`，月数上限 63 |
///
/// # 为什么是 30 天/月，且必须银行家舍入
///
/// 锁按**日历月**执行。原 SDK 用 `Math.Round(days / 30.0)`，而 C# 的 `Math.Round`
/// 默认是 **MidpointRounding.ToEven（银行家舍入）**，不是四舍五入：
///
/// - `1095 / 30 = 36.5` → **36**（偶数）→ `0xE4`
/// - 若按四舍五入会得到 37 → `0xE5`，与实测样本不符
///
/// 所以这里用整数算术实现银行家舍入，既避开了浮点精度问题，也精确复刻原行为。
pub fn duration_days_code(days: u16) -> u8 {
    let d = if days < 1 { 1 } else { days };
    if d <= 63 {
        return 0x80 | (d & 0x3F) as u8;
    }
    let months = round_half_to_even(d as u32);
    let months = months.clamp(1, 63);
    0xC0 | (months & 0x3F) as u8
}

/// 整数版银行家舍入：`round_half_to_even(n / 30)`。
///
/// - 余数 < 15 → 舍
/// - 余数 > 15 → 入
/// - 余数 = 15 → 取偶数（`.5` 时向偶数靠）
fn round_half_to_even(n: u32) -> u32 {
    let q = n / 30;
    let r = n % 30;
    match r.cmp(&15) {
        std::cmp::Ordering::Less => q,
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Equal => {
            if q % 2 == 0 {
                q
            } else {
                q + 1
            }
        }
    }
}

/// 小时 → 时长码。
///
/// - 客卡：`0x40 | (hours + 1)`（上限 62）
/// - 总控卡：`0x40 | hours`（上限 63）
///
/// 客卡比总控卡少 1，是因为客卡要把小时数加 1 后编码，占掉了 63 这一格。
/// 这是实测差异，不要"顺手统一"。
pub fn duration_hours_code(hours: u16, guest: bool) -> u8 {
    let h = if hours < 1 { 1 } else { hours };
    let v = if guest { h.saturating_add(1) } else { h };
    let v = v.min(63);
    0x40 | (v as u8 & 0x3F)
}

// ---------------------------------------------------------------- 作业流水号

/// 由发卡时刻与「本小时第几张」生成 16 位作业流水号。
///
/// ```text
/// card_no = (day & 0x1F) << 11 | (hour & 0x1F) << 6 | seq & 0x3F
/// ```
///
/// `seq` 取 **0–62**（卡内序号 = 本小时第 n 张 − 1）。
pub fn timestamp_card_no(at: &CardIssuedAt, seq: u8) -> DeviceResult<u16> {
    if seq > 62 {
        return Err(DeviceError::Param(format!(
            "小时内序号必须在 0–62，实际 {seq}"
        )));
    }
    if !(1..=31).contains(&at.day) {
        return Err(DeviceError::Param(format!("日必须是 1–31，实际 {}", at.day)));
    }
    if at.hour > 23 {
        return Err(DeviceError::Param(format!(
            "小时必须是 0–23，实际 {}",
            at.hour
        )));
    }
    Ok((((at.day & 0x1F) as u16) << 11)
        | (((at.hour & 0x1F) as u16) << 6)
        | (seq & 0x3F) as u16)
}

/// 从作业流水号还原发卡日 / 时 / 小时内序号。
///
/// 卡内**没有年月**——年月在页2[3]，需要配合 [`restore_year`] 使用。
pub fn decode_timestamp_card_no(card_no: u16) -> crate::card::CardStamp {
    crate::card::CardStamp {
        day: ((card_no >> 11) & 0x1F) as u8,
        hour: ((card_no >> 6) & 0x1F) as u8,
        seq: (card_no & 0x3F) as u8,
    }
}

/// 按原厂规则推进「发卡时刻 + 本小时第 n 张」。
///
/// 规则（来自原厂文档与本项目实测）：
///
/// - 同一「日」且卡号小时 **≥** 当前小时：沿用卡号的小时作基准，`nth = 上次 seq + 2`
/// - `nth > 63` → 小时 +1、`nth` 归 1
/// - **进位产生的「未来小时」是下一次的基准，不会退回当前小时**
/// - 跨小时 / 跨日：以当前时刻为基准，`nth` 从 1 开始
///
/// 返回 `(发卡时刻, nth)`，`nth` 取 **1–63**，卡内序号 = `nth - 1`。
///
/// 注意「卡号小时 ≥ 当前小时」这个条件：它保证上一张卡已经进位到未来小时时，
/// 后续发卡接着往后走，而不是回头用当前钟点。少了这个判断，
/// 第 65 张会退回 14 点，把已被占用的卡号重新发一遍。
pub fn next_stamp(current_card_no: u16, now: &CardIssuedAt) -> (CardIssuedAt, u8) {
    let stamp = decode_timestamp_card_no(current_card_no);

    if stamp.day == now.day && stamp.hour >= now.hour {
        let mut at = CardIssuedAt {
            hour: stamp.hour,
            ..*now
        };
        let mut nth = stamp.seq as u16 + 2;
        if nth > 63 {
            nth = 1;
            at = add_one_hour(&at);
        }
        return (at, nth as u8);
    }

    (
        CardIssuedAt {
            hour: now.hour,
            ..*now
        },
        1,
    )
}

/// 时刻 +1 小时，含跨日 / 跨月 / 跨年进位。
fn add_one_hour(at: &CardIssuedAt) -> CardIssuedAt {
    if at.hour < 23 {
        return CardIssuedAt {
            hour: at.hour + 1,
            ..*at
        };
    }
    let (year, month, day) = next_day(at.year, at.month, at.day);
    CardIssuedAt {
        year,
        month,
        day,
        hour: 0,
    }
}

/// 日进位，处理月份长度与闰年。
fn next_day(year: i32, month: u8, day: u8) -> (i32, u8, u8) {
    if day < days_in_month(year, month) {
        return (year, month, day + 1);
    }
    if month < 12 {
        return (year, month + 1, 1);
    }
    (year + 1, 1, 1)
}

/// 某年某月的天数。
fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

// ---------------------------------------------------------------- 入参校验

/// 校验发卡时刻的取值范围。纯位宽/日历检查，不含业务规则。
fn check_issued_at(at: &CardIssuedAt) -> DeviceResult<()> {
    if !(2000..=2099).contains(&at.year) {
        return Err(DeviceError::Param(format!(
            "年份必须在 2000–2099，实际 {}",
            at.year
        )));
    }
    if !(1..=12).contains(&at.month) {
        return Err(DeviceError::Param(format!("月份必须是 1–12，实际 {}", at.month)));
    }
    if !(1..=31).contains(&at.day) {
        return Err(DeviceError::Param(format!("日必须是 1–31，实际 {}", at.day)));
    }
    if at.hour > 23 {
        return Err(DeviceError::Param(format!(
            "小时必须是 0–23，实际 {}",
            at.hour
        )));
    }
    Ok(())
}

/// 校验作业流水号：1–0xFFFF。`0` 不是合法卡号。
fn check_card_no(card_no: u16) -> DeviceResult<u16> {
    if card_no == 0 {
        return Err(DeviceError::InvalidCardNo(card_no));
    }
    Ok(card_no)
}

/// 校验房间序号：0–4095（12 位）。
fn check_room_index(room_index: u16) -> DeviceResult<()> {
    if room_index > ROOM_INDEX_MAX {
        return Err(DeviceError::Param(format!(
            "房间序号 {room_index} 超出 12 位上限 {ROOM_INDEX_MAX}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- 构包

/// 构包结果：`(页1, 页2)`，各 4 字节。
pub type Pages = ([u8; 4], [u8; 4]);

/// 组装页2（时长码 / 卡号低 / 卡号高 / 年月）。
fn page2(duration_code: u8, card_no: u16, at: &CardIssuedAt) -> [u8; 4] {
    [
        duration_code,
        (card_no & 0xFF) as u8,
        ((card_no >> 8) & 0xFF) as u8,
        year_month_byte(at),
    ]
}

/// 日租客卡。
pub fn build_guest_day(spec: &GuestDaySpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    check_issued_at(&spec.issued_at)?;
    if spec.days < 1 || spec.days > 1095 {
        return Err(DeviceError::Param(format!(
            "天数必须在 1–1095，实际 {}",
            spec.days
        )));
    }
    let card_no = check_card_no(spec.card_no)?;
    let [b2, b3] = encode_room_index(spec.room_index, spec.lost)?;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    let p2 = page2(duration_days_code(spec.days), card_no, &spec.issued_at);
    Ok((p1, p2))
}

/// 钟点客卡。
pub fn build_guest_hour(spec: &GuestHourSpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    check_issued_at(&spec.issued_at)?;
    if spec.hours < 1 || spec.hours > 62 {
        return Err(DeviceError::Param(format!(
            "钟点客卡小时数必须在 1–62，实际 {}",
            spec.hours
        )));
    }
    let card_no = check_card_no(spec.card_no)?;
    let [b2, b3] = encode_room_index(spec.room_index, spec.lost)?;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    let p2 = page2(
        duration_hours_code(spec.hours, true),
        card_no,
        &spec.issued_at,
    );
    Ok((p1, p2))
}

/// 总控卡。页1 固定 `5B EC F0 FF`（= 序号 4095）。小时编码不加 1。
pub fn build_master(spec: &MasterSpec) -> DeviceResult<Pages> {
    check_issued_at(&spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let duration_code = match spec.duration {
        CardDuration::Days(d) => {
            if !(1..=1095).contains(&d) {
                return Err(DeviceError::Param(format!(
                    "总控卡天数必须在 1–1095，实际 {d}"
                )));
            }
            duration_days_code(d)
        }
        CardDuration::Hours(h) => {
            if !(1..=63).contains(&h) {
                return Err(DeviceError::Param(format!(
                    "总控卡小时数必须在 1–63，实际 {h}"
                )));
            }
            duration_hours_code(h, false)
        }
    };
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        MASTER_PAGE1_TAIL[0],
        MASTER_PAGE1_TAIL[1],
    ];
    Ok((p1, page2(duration_code, card_no, &spec.issued_at)))
}

/// 安装卡。页2 固定 `46 F2 00 01`，**不占作业流水号**。
///
/// 页1 低 4 位固定 `0x0C`，不是挂失标志——安装卡没有挂失概念。
pub fn build_install(spec: &InstallSpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    let b2 = (((spec.room_index & 0x0F) as u8) << 4) | INSTALL_LOW_NIBBLE;
    let b3 = ((spec.room_index >> 4) & 0xFF) as u8;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    Ok((p1, INSTALL_PAGE2))
}

/// 复位卡。页1 固定 `5B EC 0A 00`，页2 首字节 `0x00`。
pub fn build_reset(spec: &PlainCardSpec) -> DeviceResult<Pages> {
    check_issued_at(&spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        RESET_PAGE1_TAIL[0],
        RESET_PAGE1_TAIL[1],
    ];
    Ok((p1, page2(0x00, card_no, &spec.issued_at)))
}

/// 清除卡。页1 固定 `5B EC 9B E5`，页2 首字节 `0xC7`。
pub fn build_clear(spec: &PlainCardSpec) -> DeviceResult<Pages> {
    check_issued_at(&spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        CLEAR_PAGE1_TAIL[0],
        CLEAR_PAGE1_TAIL[1],
    ];
    Ok((p1, page2(CLEAR_PAGE2_HEAD, card_no, &spec.issued_at)))
}

/// 退房钟点设置卡。页1 = `5B EC 7B <hour>`，页2 首字节 `0xFF`。
pub fn build_checkout_hour(spec: &CheckoutHourSpec) -> DeviceResult<Pages> {
    check_issued_at(&spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    if spec.hour > 23 {
        return Err(DeviceError::Param(format!(
            "退房钟点必须是 0–23，实际 {}",
            spec.hour
        )));
    }
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        CHECKOUT_HOUR_MARKER,
        spec.hour,
    ];
    Ok((p1, page2(CHECKOUT_HOUR_PAGE2_HEAD, card_no, &spec.issued_at)))
}

/// 校时卡。页2 首字节固定 `0x06`（明文）。
///
/// 页1 位域（IDA `tem_writetimercard_sdk @ 0x7154`，7 组实测样本验证）：
///
/// ```text
/// 页1[2] = 16 × (分 & 0x0F) + 9
/// 页1[3] = (分 >> 4) + ((4 × 时) & 0x7C)
/// ```
///
/// 页1[2] 必须满足 `& 0x09 == 0x09`，否则读卡会返回错误 4。
/// 早期按「16 × 当日分钟数 + 953」拟合的公式是错的，已废弃。
pub fn build_timesync(spec: &TimeSyncSpec) -> DeviceResult<Pages> {
    check_issued_at(&spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    if spec.hour > 23 {
        return Err(DeviceError::Param(format!(
            "小时必须是 0–23，实际 {}",
            spec.hour
        )));
    }
    if spec.minute > 59 {
        return Err(DeviceError::Param(format!(
            "分钟必须是 0–59，实际 {}",
            spec.minute
        )));
    }
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        16 * (spec.minute & 0x0F) + 9,
        (spec.minute >> 4) + ((4 * spec.hour) & 0x7C),
    ];
    Ok((p1, page2(TIMESYNC_PAGE2_HEAD, card_no, &spec.issued_at)))
}

/// 擦卡用的全零页。
///
/// 写零后 28 字节全零 = 空白卡，锁不再认它。
/// **注意**：这不会让门锁里已有的授权消失。
pub const ERASE_PAGES: Pages = ([0x00; 4], [0x00; 4]);

/// 把页1/页2 拼成 28 字节卡数据（页3–7 恒为 0）。
pub fn to_raw28(p1: &[u8; 4], p2: &[u8; 4]) -> [u8; crate::card::CARD_BYTES] {
    let mut raw = [0u8; crate::card::CARD_BYTES];
    raw[0..4].copy_from_slice(p1);
    raw[4..8].copy_from_slice(p2);
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_2026_11_08() -> CardIssuedAt {
        CardIssuedAt {
            year: 2026,
            month: 11,
            day: 8,
            hour: 14,
        }
    }

    fn hex4(b: [u8; 4]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    // ------------------------------------------------ 年月字节

    /// 年月字节是 16 年循环，2026-11 与 2027-11 差 0x10。
    #[test]
    fn year_month_byte_is_16_year_cycle() {
        let mk = |y, m| year_month_byte(&CardIssuedAt {
            year: y,
            month: m,
            day: 1,
            hour: 0,
        });
        assert_eq!(mk(2026, 11), 0xAB, "2026-11 应为 0xAB（实测）");
        assert_eq!(mk(2027, 11), 0xBB, "2027-11 应为 0xBB（实测）");
        assert_eq!(mk(2028, 1), 0xC1);
        // 16 年后回到同一个月
        assert_eq!(mk(2042, 11), 0xAB, "2042-11 应回到 0xAB");
        assert_eq!(mk(2000, 1), 0x01);
    }

    /// 还原年份需要业务层提供"当前年"，卡本身只有 4 位年份。
    #[test]
    fn restore_year_uses_current_year_anchor() {
        assert_eq!(restore_year(0xAB, 2026), 2026);
        assert_eq!(restore_year(0xBB, 2027), 2027);
        // 同一个月字节在不同锚点下还原出不同年份（16 年循环的本质）
        assert_eq!(restore_year(0xAB, 2042), 2042);
        assert_eq!(restore_year(0xAB, 2058), 2058);
    }

    // ------------------------------------------------ 时长码

    /// 6 条时长码边界，全部来自实测。
    #[test]
    fn duration_days_matches_golden_cases() {
        let cases = [
            (63u16, 0xBFu8),  // 0x80|63，不改月
            (64, 0xC2),       // 64/30≈2.1 → 2 月
            (90, 0xC3),       // 3 月
            (150, 0xC5),      // 5 月
            (360, 0xCC),      // 12 月
            (1095, 0xE4),     // 36 月（银行家舍入，不是 37）
        ];
        for (days, expected) in cases {
            assert_eq!(duration_days_code(days), expected, "{days} 天编码错误");
        }
    }

    /// 1095 天是银行家舍入的关键用例：36.5 → 36（偶数），不是 37。
    #[test]
    fn days_1095_uses_bankers_rounding() {
        assert_eq!(1095 / 30, 36);
        assert_eq!(1095 % 30, 15, "1095/30 恰好余 15，命中 .5");
        assert_eq!(duration_days_code(1095), 0xC0 | 36, "必须是 36 月");
        assert_ne!(duration_days_code(1095), 0xC0 | 37, "四舍五入会得到 37，是错的");
    }

    /// 天数 ≤63 走简单段，边界 1 与 63。
    #[test]
    fn simple_day_segment_boundaries() {
        assert_eq!(duration_days_code(1), 0x81);
        assert_eq!(duration_days_code(62), 0xBE);
        assert_eq!(duration_days_code(63), 0xBF);
        // 0 天按 1 天处理（防御，正常入参不会到这一步）
        assert_eq!(duration_days_code(0), 0x81);
    }

    /// 客卡小时加 1、总控卡不加，上限差 1。
    #[test]
    fn hour_codes_differ_between_guest_and_master() {
        assert_eq!(duration_hours_code(1, true), 0x42, "客卡 1 小时 → 0x40|2");
        assert_eq!(duration_hours_code(1, false), 0x41, "总控 1 小时 → 0x40|1");
        assert_eq!(duration_hours_code(62, true), 0x7F, "客卡上限 62 → 0x40|63");
        assert_eq!(duration_hours_code(63, false), 0x7F, "总控上限 63 → 0x40|63");
        assert_eq!(duration_hours_code(3, true), 0x44);
        assert_eq!(duration_hours_code(6, true), 0x47);
    }

    /// 银行家舍入的整数实现要覆盖 .5 上下与奇偶两侧。
    #[test]
    fn round_half_to_even_cases() {
        // 余数 < 15：舍
        assert_eq!(round_half_to_even(64), 2); // 2 余 4
        assert_eq!(round_half_to_even(89), 3); // 2 余 29 → 3
                                               // 余数 = 15：取偶数
        assert_eq!(round_half_to_even(45), 2); // 1 余 15，q=1 奇 → 2
        assert_eq!(round_half_to_even(75), 2); // 2 余 15，q=2 偶 → 2
        assert_eq!(round_half_to_even(105), 4); // 3 余 15，q=3 奇 → 4
        assert_eq!(round_half_to_even(1095), 36); // 36 余 15，q=36 偶 → 36
    }

    // ------------------------------------------------ 卡号

    /// 7 条卡号用例。
    #[test]
    fn card_no_matches_golden_cases() {
        let t0 = at_2026_11_08();
        assert_eq!(timestamp_card_no(&t0, 0).unwrap(), 0x4380);
        assert_eq!(timestamp_card_no(&t0, 1).unwrap(), 0x4381);
        assert_eq!(timestamp_card_no(&t0, 62).unwrap(), 0x43BE);
    }

    /// 实测样本 0x5B34 = 11 日 12 点，该小时第 53 张（seq=52）。
    #[test]
    fn decode_recorded_sample_0x5b34() {
        let s = decode_timestamp_card_no(0x5B34);
        assert_eq!(s.day, 11);
        assert_eq!(s.hour, 12);
        assert_eq!(s.seq, 52);
    }

    /// 第 64 张：满 63 张后进位到下一小时、n 归 1。
    #[test]
    fn 第64张进位() {
        let t0 = at_2026_11_08(); // 11-08 14:00
        let (at, nth) = next_stamp(0x43BE, &t0); // 上次 = 14 点 seq62 = 第 63 张
        assert_eq!(nth, 1);
        assert_eq!(at.hour, 15, "应进位到 15 点");
        assert_eq!(timestamp_card_no(&at, nth - 1).unwrap(), 0x43C0);
    }

    /// 第 65 张（时钟仍是 14:00）：必须仍在 15 点，不能退回 14 点。
    #[test]
    fn 第65张不倒退() {
        let t0 = at_2026_11_08();
        let (at, nth) = next_stamp(0x43C0, &t0);
        assert_eq!(nth, 2);
        assert_eq!(at.hour, 15, "未来小时是基准，不能退回 14 点");
        assert_eq!(timestamp_card_no(&at, nth - 1).unwrap(), 0x43C1);
    }

    /// 跨日进位：23 点发满 → 次日 0 点第 1 张。
    #[test]
    fn 跨日进位() {
        let t1 = CardIssuedAt {
            year: 2026,
            month: 11,
            day: 8,
            hour: 23,
        };
        let last_of_day = timestamp_card_no(&t1, 62).unwrap();
        let (at, nth) = next_stamp(last_of_day, &t1);
        assert_eq!(nth, 1);
        assert_eq!(at.day, 9, "应跨到 9 日");
        assert_eq!(at.hour, 0);
        assert_eq!(timestamp_card_no(&at, nth - 1).unwrap(), 9 << 11);
    }

    /// 跨月进位：月末 23 点发满 → 次月 1 日 0 点。
    #[test]
    fn 跨月进位() {
        let t = CardIssuedAt {
            year: 2026,
            month: 11,
            day: 30,
            hour: 23,
        };
        let last = timestamp_card_no(&t, 62).unwrap();
        let (at, _) = next_stamp(last, &t);
        assert_eq!((at.month, at.day, at.hour), (12, 1, 0));
    }

    /// 跨年进位：12 月 31 日 23 点发满 → 次年 1 月 1 日 0 点。
    #[test]
    fn 跨年进位() {
        let t = CardIssuedAt {
            year: 2026,
            month: 12,
            day: 31,
            hour: 23,
        };
        let last = timestamp_card_no(&t, 62).unwrap();
        let (at, _) = next_stamp(last, &t);
        assert_eq!((at.year, at.month, at.day, at.hour), (2027, 1, 1, 0));
    }

    /// 闰年 2 月 29 日 23 点进位到 3 月 1 日。
    #[test]
    fn 闰年进位() {
        let t = CardIssuedAt {
            year: 2028,
            month: 2,
            day: 29,
            hour: 23,
        };
        let last = timestamp_card_no(&t, 62).unwrap();
        let (at, _) = next_stamp(last, &t);
        assert_eq!((at.month, at.day), (3, 1));
    }

    /// seq > 62 必须是参数错误，不能静默截断。
    #[test]
    fn seq_out_of_range_is_param_error() {
        let t0 = at_2026_11_08();
        assert!(timestamp_card_no(&t0, 62).is_ok());
        let err = timestamp_card_no(&t0, 63).unwrap_err();
        assert_eq!(err.sdk_code(), 1);
    }

    /// 卡号 0 非法——它是"没有卡号"，不是有效流水号。
    #[test]
    fn zero_card_no_is_invalid() {
        let spec = PlainCardSpec {
            card_no: 0,
            issued_at: at_2026_11_08(),
        };
        let err = build_reset(&spec).unwrap_err();
        assert_eq!(err.sdk_code(), 9);
    }

    // ------------------------------------------------ 构包：客卡

    /// 220 房 5 天：`5BECF000` / `858822AB`。
    #[test]
    fn build_guest_day_220_5days() {
        let spec = GuestDaySpec {
            room_index: 15,
            days: 5,
            card_no: 0x2288,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_guest_day(&spec).unwrap();
        assert_eq!(hex4(p1), "5BECF000");
        assert_eq!(hex4(p2), "858822AB");
    }

    /// 201 房序号 0——旧 CLI 根本发不出来的那个。
    #[test]
    fn build_guest_day_index_zero_is_supported() {
        let spec = GuestDaySpec {
            room_index: 0,
            days: 1,
            card_no: 0x224A,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_guest_day(&spec).unwrap();
        assert_eq!(hex4(p1), "5BEC0000");
        assert_eq!(hex4(p2), "814A22AB");
    }

    /// 260 房序号 39：`B3 = 0x02`，验证 12 位序号的高 8 位写入。
    #[test]
    fn build_guest_day_index_39_uses_b3() {
        let spec = GuestDaySpec {
            room_index: 39,
            days: 1,
            card_no: 0x2281,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_guest_day(&spec).unwrap();
        assert_eq!(hex4(p1), "5BEC7002");
        assert_eq!(hex4(p2), "818122AB");
    }

    /// 挂失卡：页1[2] 低 4 位 = 1，且不影响序号。
    #[test]
    fn lost_flag_only_touches_low_nibble() {
        let normal = GuestDaySpec {
            room_index: 15,
            days: 1,
            card_no: 0x2403,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let lost = GuestDaySpec {
            lost: true,
            ..normal
        };
        let (p1n, _) = build_guest_day(&normal).unwrap();
        let (p1l, p2l) = build_guest_day(&lost).unwrap();
        assert_eq!(p1n[2], 0xF0);
        assert_eq!(p1l[2], 0xF1, "挂失位只改低 4 位");
        assert_eq!(p1n[3], p1l[3], "挂失不许影响 B3");
        assert_eq!(hex4(p2l), "810324AB", "页2 不受挂失影响");
    }

    /// 钟点客卡 62 小时是上限。
    #[test]
    fn build_guest_hour_boundaries() {
        let mk = |hours: u16, card_no: u16| GuestHourSpec {
            room_index: 15,
            hours,
            card_no,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let (_, p2) = build_guest_hour(&mk(62, 0x228C)).unwrap();
        assert_eq!(hex4(p2), "7F8C22AB");

        let err = build_guest_hour(&mk(63, 0x228C)).unwrap_err();
        assert_eq!(err.sdk_code(), 1, "63 小时超出客卡上限");
    }

    /// 序号 4095 必须能构造（旧 SDK 的 0xFF 限制是错的）。
    #[test]
    fn room_index_full_12_bits_is_writable() {
        let spec = GuestDaySpec {
            room_index: 4095,
            days: 1,
            card_no: 0x1234,
            lost: false,
            issued_at: at_2026_11_08(),
        };
        let (p1, _) = build_guest_day(&spec).unwrap();
        assert_eq!((p1[2], p1[3]), (0xF0, 0xFF));

        let over = GuestDaySpec {
            room_index: 4096,
            ..spec
        };
        let err = build_guest_day(&over).unwrap_err();
        assert_eq!(err.sdk_code(), 1);
    }

    // ------------------------------------------------ 构包：管理卡

    /// 总控卡页1 固定 `5BECF0FF`，小时编码不加 1。
    #[test]
    fn build_master_fixed_page1() {
        let day = MasterSpec {
            duration: CardDuration::Days(1),
            card_no: 0x2248,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_master(&day).unwrap();
        assert_eq!(hex4(p1), "5BECF0FF");
        assert_eq!(hex4(p2), "814822AB");

        let hour = MasterSpec {
            duration: CardDuration::Hours(2),
            card_no: 0x2BC2,
            issued_at: at_2026_11_08(),
        };
        let (_, p2h) = build_master(&hour).unwrap();
        assert_eq!(hex4(p2h), "42C22BAB", "总控 2 小时 → 0x40|2，不加 1");
    }

    /// 安装卡页2 固定 `46F20001`，低 4 位固定 0x0C。
    #[test]
    fn build_install_has_fixed_page2_and_low_nibble() {
        let (p1, p2) = build_install(&InstallSpec { room_index: 15 }).unwrap();
        assert_eq!(hex4(p1), "5BECFC00", "低 4 位必须是 0x0C");
        assert_eq!(hex4(p2), "46F20001");

        let (p1z, _) = build_install(&InstallSpec { room_index: 0 }).unwrap();
        assert_eq!(hex4(p1z), "5BEC0C00");
    }

    /// 安装卡的 0x0C 不是挂失标志——按位打包解会得到 lost=true，但那是无意义的。
    #[test]
    fn install_low_nibble_is_not_lost_flag() {
        let (p1, _) = build_install(&InstallSpec { room_index: 15 }).unwrap();
        let (idx, lost) = crate::card::decode_room_index(p1[2], p1[3]);
        assert_eq!(idx, 15, "序号解码正确");
        assert!(lost, "按通用规则解出来是 true —— 但安装卡不存在挂失语义");
        assert_eq!(p1[2] & 0x0F, INSTALL_LOW_NIBBLE);
    }

    /// 复位卡与清除卡。
    #[test]
    fn build_reset_and_clear() {
        let reset = PlainCardSpec {
            card_no: 0x2441,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_reset(&reset).unwrap();
        assert_eq!(hex4(p1), "5BEC0A00");
        assert_eq!(hex4(p2), "004124AB");

        let clear = PlainCardSpec {
            card_no: 0x2442,
            issued_at: at_2026_11_08(),
        };
        let (p1c, p2c) = build_clear(&clear).unwrap();
        assert_eq!(hex4(p1c), "5BEC9BE5");
        assert_eq!(hex4(p2c), "C74224AB");
    }

    /// 退房钟点设置卡。
    #[test]
    fn build_checkout_hour_cards() {
        let h12 = CheckoutHourSpec {
            hour: 12,
            card_no: 0x228F,
            issued_at: at_2026_11_08(),
        };
        let (p1, p2) = build_checkout_hour(&h12).unwrap();
        assert_eq!(hex4(p1), "5BEC7B0C");
        assert_eq!(hex4(p2), "FF8F22AB");

        // 14 点用另一张实测样本：卡号 0x2290（不是 12 点的 0x228F）
        let h14 = CheckoutHourSpec {
            hour: 14,
            card_no: 0x2290,
            issued_at: h12.issued_at,
        };
        let (p1b, p2b) = build_checkout_hour(&h14).unwrap();
        assert_eq!(hex4(p1b), "5BEC7B0E");
        assert_eq!(hex4(p2b), "FF9022AB");

        let bad = CheckoutHourSpec { hour: 24, ..h12 };
        assert_eq!(build_checkout_hour(&bad).unwrap_err().sdk_code(), 1);
    }

    /// 校时卡页1 位域——7 组实测样本。
    #[test]
    fn build_timesync_bitfields() {
        let mk = |h, m, no| TimeSyncSpec {
            hour: h,
            minute: m,
            card_no: no,
            issued_at: at_2026_11_08(),
        };
        let cases = [
            (mk(8, 30, 0x4200), "5BECE921", "060042AB"),
            (mk(12, 0, 0x4300), "5BEC0930", "060043AB"),
            (mk(19, 47, 0x44C0), "5BECF94E", "06C044AB"),
            (mk(22, 0, 0x4580), "5BEC0958", "068045AB"),
            (mk(19, 47, 0x5CC0), "5BECF94E", "06C05CAB"),
            (mk(16, 35, 0x4C01), "5BEC3942", "06014CAB"),
        ];
        for (spec, want_p1, want_p2) in cases {
            let (p1, p2) = build_timesync(&spec).unwrap();
            assert_eq!(hex4(p1), want_p1, "{}:{} 页1 错误", spec.hour, spec.minute);
            assert_eq!(hex4(p2), want_p2, "{}:{} 页2 错误", spec.hour, spec.minute);
        }
    }

    /// 校时卡页1[2] 必须满足 `& 0x09 == 0x09`，否则读卡返回错误 4。
    #[test]
    fn timesync_page1_low_nibble_constraint() {
        for minute in 0u8..60 {
            let b2 = 16 * (minute & 0x0F) + 9;
            assert_eq!(b2 & 0x09, 0x09, "分钟 {minute} 违反 & 0x09 == 0x09");
        }
    }

    /// 擦卡页全零。
    #[test]
    fn erase_pages_are_all_zero() {
        assert_eq!(ERASE_PAGES.0, [0u8; 4]);
        assert_eq!(ERASE_PAGES.1, [0u8; 4]);
        let raw = to_raw28(&ERASE_PAGES.0, &ERASE_PAGES.1);
        assert!(raw.iter().all(|b| *b == 0), "擦卡必须写出全零 28 字节");
    }

    /// 拼装 28 字节时页3–7 必须全零。
    #[test]
    fn to_raw28_pads_with_zeros() {
        let p1 = [0x5B, 0xEC, 0xF0, 0x00];
        let p2 = [0x85, 0x88, 0x22, 0xAB];
        let raw = to_raw28(&p1, &p2);
        assert_eq!(raw.len(), 28);
        assert_eq!(&raw[0..8], &[0x5B, 0xEC, 0xF0, 0x00, 0x85, 0x88, 0x22, 0xAB]);
        assert!(raw[8..].iter().all(|b| *b == 0), "页3–7 必须全零");
    }

    // ------------------------------------------------ 入参校验

    /// 发卡时刻的日历校验。
    #[test]
    fn issued_at_validation() {
        let bad_month = GuestDaySpec {
            room_index: 1,
            days: 1,
            card_no: 1,
            lost: false,
            issued_at: CardIssuedAt {
                year: 2026,
                month: 13,
                day: 1,
                hour: 0,
            },
        };
        assert_eq!(build_guest_day(&bad_month).unwrap_err().sdk_code(), 1);

        let bad_hour = GuestDaySpec {
            issued_at: CardIssuedAt {
                hour: 24,
                ..at_2026_11_08()
            },
            ..bad_month
        };
        // 传了 month=13 但这里也想验 hour，构造一个独立的
        let bad_hour = GuestDaySpec {
            issued_at: CardIssuedAt {
                year: 2026,
                month: 11,
                day: 8,
                hour: 24,
            },
            ..bad_hour
        };
        assert_eq!(build_guest_day(&bad_hour).unwrap_err().sdk_code(), 1);
    }

    /// 30 天/月换算不能出现 0 月（天数 63 走简单段，64 起最少 2 月）。
    #[test]
    fn month_code_never_zero() {
        for days in 64u16..=1095 {
            let code = duration_days_code(days);
            assert_eq!(code & 0xC0, 0xC0, "{days} 天应走月编码段");
            assert!((code & 0x3F) >= 1, "{days} 天算出 0 月");
        }
    }
}
