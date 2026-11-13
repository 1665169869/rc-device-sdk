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
    InstallSpec, MasterSpec, PlainCardSpec, RawSpec, TimeSyncSpec, CARD_PREFIX, ROOM_INDEX_MAX,
};
use crate::error::{DeviceError, DeviceResult};

// 协议常量统一在 `crate::protocol` 定义，这里重新导出以保持既有调用路径不变。
pub use crate::protocol::{
    BYTE_MASK, CARD_NO_DAY_MASK, CARD_NO_DAY_SHIFT, CARD_NO_HIGH_SHIFT, CARD_NO_HOUR_MASK,
    CARD_NO_HOUR_SHIFT, CARD_NO_SEQ_MASK, CHECKOUT_HOUR_MARKER, CHECKOUT_HOUR_PAGE2_HEAD,
    CLEAR_PAGE1_TAIL, CLEAR_PAGE2_HEAD, DAYS_PER_MONTH, DAY_SIMPLE_PREFIX, DURATION_MASK,
    ERASE_PAGE, HOUR_PREFIX, INSTALL_LOW_NIBBLE, INSTALL_PAGE2, MASTER_PAGE1_TAIL,
    MAX_DAY_OF_MONTH, MAX_GUEST_HOURS, MAX_HOURS_MASTER, MAX_HOUR_OF_DAY, MAX_MINUTE, MAX_MONTH,
    MAX_SIMPLE_DAYS, MAX_TOTAL_DAYS, MAX_YEAR, MIN_DAYS, MIN_HOURS, MIN_MONTH, MIN_YEAR,
    MINUTE_HIGH_SHIFT, MINUTE_LOW_MASK, MONTH_MASK, MONTH_PREFIX, MONTHS_MAX, PAGE_BYTES,
    PAYLOAD_BYTES, RESET_PAGE1_TAIL, RESET_PAGE2_HEAD, ROOM_HIGH_SHIFT, ROOM_LOW_NIBBLE_MASK,
    ROOM_LOW_NIBBLE_SHIFT, ROUND_HALF_GAP, SEQ_MAX, TIMESYNC_HOUR_FACTOR, TIMESYNC_HOUR_MASK,
    TIMESYNC_MINUTE_LOW_FACTOR, TIMESYNC_MINUTE_LOW_OFFSET, TIMESYNC_PAGE2_HEAD,
    TIMESYNC_PAGE2_HEAD_LEGACY, WRITE_PASSWORD, YEAR_BASE, YEAR_CYCLE, YEAR_MASK, YEAR_SHIFT,
};

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
    // 用取模而不是直接按位与，避免 year < YEAR_BASE 时出现负数补码的意外值
    let year_part = (((at.year - YEAR_BASE) % YEAR_CYCLE + YEAR_CYCLE) % YEAR_CYCLE) as u8;
    let month_part = at.month & MONTH_MASK;
    (year_part << YEAR_SHIFT) | month_part
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
    let high = i32::from(year_month_byte >> YEAR_SHIFT);
    YEAR_BASE + YEAR_CYCLE * ((current_year - YEAR_BASE) / YEAR_CYCLE) + high
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
    let d = if days < MIN_DAYS { MIN_DAYS } else { days };
    if d <= MAX_SIMPLE_DAYS {
        return DAY_SIMPLE_PREFIX | (d & u16::from(DURATION_MASK)) as u8;
    }
    let months = round_half_to_even(u32::from(d));
    let months = months.clamp(1, u32::from(MONTHS_MAX));
    MONTH_PREFIX | (months & u32::from(DURATION_MASK)) as u8
}

/// 整数版银行家舍入：`round_half_to_even(n / DAYS_PER_MONTH)`。
///
/// - 余数 < `ROUND_HALF_GAP` → 舍
/// - 余数 > `ROUND_HALF_GAP` → 入
/// - 余数 = `ROUND_HALF_GAP` → 取偶数（`.5` 时向偶数靠）
fn round_half_to_even(n: u32) -> u32 {
    let divisor = u32::from(DAYS_PER_MONTH);
    let gap = u32::from(ROUND_HALF_GAP);
    let q = n / divisor;
    let r = n % divisor;
    match r.cmp(&gap) {
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
    let h = if hours < MIN_HOURS { MIN_HOURS } else { hours };
    let v = if guest { h.saturating_add(1) } else { h };
    // 截断的是**编码后的值**，不是入参小时数：客卡的 h+1 恰好可能等于 62+1=63。
    // 六位字段满值就是 DURATION_MASK（0x3F = 63）。
    let v = v.min(u16::from(DURATION_MASK));
    HOUR_PREFIX | (v as u8 & DURATION_MASK)
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
    if seq > SEQ_MAX {
        return Err(DeviceError::Param(format!(
            "小时内序号必须在 0–{SEQ_MAX}，实际 {seq}"
        )));
    }
    if !(1..=MAX_DAY_OF_MONTH).contains(&at.day) {
        return Err(DeviceError::Param(format!(
            "日必须是 1–{MAX_DAY_OF_MONTH}，实际 {}",
            at.day
        )));
    }
    if at.hour > MAX_HOUR_OF_DAY {
        return Err(DeviceError::Param(format!(
            "小时必须是 0–{MAX_HOUR_OF_DAY}，实际 {}",
            at.hour
        )));
    }
    Ok(((u16::from(at.day) & CARD_NO_DAY_MASK) << CARD_NO_DAY_SHIFT)
        | ((u16::from(at.hour) & CARD_NO_HOUR_MASK) << CARD_NO_HOUR_SHIFT)
        | (u16::from(seq) & CARD_NO_SEQ_MASK))
}

/// 从作业流水号还原发卡日 / 时 / 小时内序号。
///
/// 卡内**没有年月**——年月在页2[3]，需要配合 [`restore_year`] 使用。
pub fn decode_timestamp_card_no(card_no: u16) -> crate::card::CardStamp {
    crate::card::CardStamp {
        day: ((card_no >> CARD_NO_DAY_SHIFT) & CARD_NO_DAY_MASK) as u8,
        hour: ((card_no >> CARD_NO_HOUR_SHIFT) & CARD_NO_HOUR_MASK) as u8,
        seq: (card_no & CARD_NO_SEQ_MASK) as u8,
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
        let mut nth = u16::from(stamp.seq) + 2;
        if nth > 63 {
            nth = 1;
            at = add_one_hour(at);
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
///
/// `CardIssuedAt` 只有 8 字节，按值传递比按引用高效（避免一次间接寻址）。
fn add_one_hour(at: CardIssuedAt) -> CardIssuedAt {
    if at.hour < MAX_HOUR_OF_DAY {
        return CardIssuedAt {
            hour: at.hour + 1,
            ..at
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
///
/// 2 月按闰年规则返回 28/29；其余月份 4/6/9/11 为 30 天，其它为 31 天。
fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 31,
    }
}

// ---------------------------------------------------------------- 入参校验

/// 校验发卡时刻的取值范围。纯位宽/日历检查，不含业务规则。
///
/// `CardIssuedAt` 只有 8 字节，按值传递比按引用高效。
fn check_issued_at(at: CardIssuedAt) -> DeviceResult<()> {
    if !(MIN_YEAR..=MAX_YEAR).contains(&at.year) {
        return Err(DeviceError::Param(format!(
            "年份必须在 {MIN_YEAR}–{MAX_YEAR}，实际 {}",
            at.year
        )));
    }
    if !(MIN_MONTH..=MAX_MONTH).contains(&at.month) {
        return Err(DeviceError::Param(format!(
            "月份必须是 {MIN_MONTH}–{MAX_MONTH}，实际 {}",
            at.month
        )));
    }
    if !(1..=MAX_DAY_OF_MONTH).contains(&at.day) {
        return Err(DeviceError::Param(format!(
            "日必须是 1–{MAX_DAY_OF_MONTH}，实际 {}",
            at.day
        )));
    }
    if at.hour > MAX_HOUR_OF_DAY {
        return Err(DeviceError::Param(format!(
            "小时必须是 0–{MAX_HOUR_OF_DAY}，实际 {}",
            at.hour
        )));
    }
    Ok(())
}

/// 校验作业流水号：1–`0xFFFF`。`0` 不是合法卡号。
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
///
/// 卡号**小端**写入：`[1]` = 低字节、`[2]` = 高字节。
fn page2(duration_code: u8, card_no: u16, at: CardIssuedAt) -> [u8; PAGE_BYTES] {
    [
        duration_code,
        (card_no & BYTE_MASK) as u8,
        ((card_no >> CARD_NO_HIGH_SHIFT) & BYTE_MASK) as u8,
        year_month_byte(&at),
    ]
}

/// 日租客卡。
pub fn build_guest_day(spec: &GuestDaySpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    check_issued_at(spec.issued_at)?;
    if spec.days < MIN_DAYS || spec.days > MAX_TOTAL_DAYS {
        return Err(DeviceError::Param(format!(
            "天数必须在 {MIN_DAYS}–{MAX_TOTAL_DAYS}，实际 {}",
            spec.days
        )));
    }
    let card_no = check_card_no(spec.card_no)?;
    let [b2, b3] = encode_room_index(spec.room_index, spec.lost)?;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    let p2 = page2(duration_days_code(spec.days), card_no, spec.issued_at);
    Ok((p1, p2))
}

/// 钟点客卡。
pub fn build_guest_hour(spec: &GuestHourSpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    check_issued_at(spec.issued_at)?;
    if spec.hours < MIN_HOURS || spec.hours > MAX_GUEST_HOURS {
        return Err(DeviceError::Param(format!(
            "钟点客卡小时数必须在 {MIN_HOURS}–{MAX_GUEST_HOURS}，实际 {}",
            spec.hours
        )));
    }
    let card_no = check_card_no(spec.card_no)?;
    let [b2, b3] = encode_room_index(spec.room_index, spec.lost)?;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    let p2 = page2(
        duration_hours_code(spec.hours, true),
        card_no,
        spec.issued_at,
    );
    Ok((p1, p2))
}

/// 总控卡。页1 固定 `5B EC F0 FF`（= 序号 4095）。小时编码不加 1。
pub fn build_master(spec: &MasterSpec) -> DeviceResult<Pages> {
    check_issued_at(spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let duration_code = match spec.duration {
        CardDuration::Days(d) => {
            if !(MIN_DAYS..=MAX_TOTAL_DAYS).contains(&d) {
                return Err(DeviceError::Param(format!(
                    "总控卡天数必须在 {MIN_DAYS}–{MAX_TOTAL_DAYS}，实际 {d}"
                )));
            }
            duration_days_code(d)
        }
        CardDuration::Hours(h) => {
            if !(MIN_HOURS..=MAX_HOURS_MASTER).contains(&h) {
                return Err(DeviceError::Param(format!(
                    "总控卡小时数必须在 {MIN_HOURS}–{MAX_HOURS_MASTER}，实际 {h}"
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
    Ok((p1, page2(duration_code, card_no, spec.issued_at)))
}

/// 安装卡。页2 固定 `46 F2 00 01`，**不占作业流水号**。
///
/// 页1 低 4 位固定 `0x0C`，不是挂失标志——安装卡没有挂失概念。
pub fn build_install(spec: &InstallSpec) -> DeviceResult<Pages> {
    check_room_index(spec.room_index)?;
    // 位打包与 `encode_room_index` 同构，但低 4 位是固定的 INSTALL_LOW_NIBBLE
    // 而不是挂失标志——安装卡没有挂失概念。
    let low_nibble = ((spec.room_index & ROOM_LOW_NIBBLE_MASK) as u8) << ROOM_LOW_NIBBLE_SHIFT;
    let b2 = low_nibble | INSTALL_LOW_NIBBLE;
    let b3 = ((spec.room_index >> ROOM_HIGH_SHIFT) & BYTE_MASK) as u8;
    let p1 = [CARD_PREFIX[0], CARD_PREFIX[1], b2, b3];
    Ok((p1, INSTALL_PAGE2))
}

/// 复位卡。页1 固定 `5B EC 0A 00`，页2 首字节 `0x00`。
pub fn build_reset(spec: &PlainCardSpec) -> DeviceResult<Pages> {
    check_issued_at(spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        RESET_PAGE1_TAIL[0],
        RESET_PAGE1_TAIL[1],
    ];
    Ok((p1, page2(RESET_PAGE2_HEAD, card_no, spec.issued_at)))
}

/// 清除卡。页1 固定 `5B EC 9B E5`，页2 首字节 `0xC7`。
pub fn build_clear(spec: &PlainCardSpec) -> DeviceResult<Pages> {
    check_issued_at(spec.issued_at)?;
    let card_no = check_card_no(spec.card_no)?;
    let p1 = [
        CARD_PREFIX[0],
        CARD_PREFIX[1],
        CLEAR_PAGE1_TAIL[0],
        CLEAR_PAGE1_TAIL[1],
    ];
    Ok((p1, page2(CLEAR_PAGE2_HEAD, card_no, spec.issued_at)))
}

/// 退房钟点设置卡。页1 = `5B EC 7B <hour>`，页2 首字节 `0xFF`。
pub fn build_checkout_hour(spec: &CheckoutHourSpec) -> DeviceResult<Pages> {
    check_issued_at(spec.issued_at)?;
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
    Ok((p1, page2(CHECKOUT_HOUR_PAGE2_HEAD, card_no, spec.issued_at)))
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
    check_issued_at(spec.issued_at)?;
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
        // 页1[2] = TIMESYNC_MINUTE_LOW_FACTOR × (分 & 低4位) + OFFSET
        // 其中 OFFSET 保证结果恒满足 `& TIMESYNC_PAGE1_REQUIRED_BITS == 该掩码`
        TIMESYNC_MINUTE_LOW_FACTOR * (spec.minute & MINUTE_LOW_MASK) + TIMESYNC_MINUTE_LOW_OFFSET,
        // 页1[3] = 分的高位 + 小时经 TIMESYNC_HOUR_FACTOR 倍乘后落在 TIMESYNC_HOUR_MASK 位上
        (spec.minute >> MINUTE_HIGH_SHIFT) + ((TIMESYNC_HOUR_FACTOR * spec.hour) & TIMESYNC_HOUR_MASK),
    ];
    Ok((p1, page2(TIMESYNC_PAGE2_HEAD, card_no, spec.issued_at)))
}

/// 擦卡用的全零页。
///
/// 写零后 28 字节全零 = 空白卡，锁不再认它。
/// **注意**：这不会让门锁里已有的授权消失。
pub const ERASE_PAGES: Pages = (ERASE_PAGE, ERASE_PAGE);

/// 原始直写：把给定页原样返回。
///
/// # 为什么不做校验
///
/// 本函数**不是**某一种卡型的编码规则，而是「调用方给了什么就写什么」的逃生门
/// （逆向验证、现场排障）。入了参就是四字节页，位域含义由调用方自己负责——
/// 一旦在这里加检查，就等于替调用方猜他想写哪种卡，反而更容易写出半对半错的卡。
///
/// # 为什么不进 46 项黄金向量
///
/// 黄金向量的价值是「编码结果必须等于原厂字节」。原始直写没有编码，
/// 期望值恒等于入参，放进去只会凑数。它的正确性由 `to_raw28` 的往返测试保证。
///
/// 擦卡请用 [`ERASE_PAGES`] + [`crate::CardDevice::erase_card`]，不要用它绕过。
pub fn build_raw(spec: &RawSpec) -> Pages {
    (spec.p1, spec.p2)
}

/// 把页1/页2 拼成 28 字节卡数据（页3–7 恒为 0）。
pub fn to_raw28(p1: &[u8; 4], p2: &[u8; 4]) -> [u8; crate::card::CARD_BYTES] {
    let mut raw = [0u8; crate::card::CARD_BYTES];
    raw[0..4].copy_from_slice(p1);
    raw[4..8].copy_from_slice(p2);
    raw
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            s.push(crate::card::HEX_UPPER[(x >> 4) as usize] as char);
            s.push(crate::card::HEX_UPPER[(x & 0x0F) as usize] as char);
        }
        s
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
        assert_eq!(duration_days_code(1095), 0xC0 | 0x24, "必须是 36 月");
        assert_ne!(
            duration_days_code(1095),
            0xC0 | 0x25,
            "四舍五入会得到 37（0x25），是错的"
        );
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

    /// 原始直写不解释入参：给什么写什么，一个 bit 都不改。
    #[test]
    fn build_raw_passes_bytes_through_unchanged() {
        let spec =
            RawSpec::from_hex("5BECF000", "810424AB").expect("合法十六进制必须能构造 RawSpec");
        let (p1, p2) = build_raw(&spec);
        assert_eq!(p1, [0x5B, 0xEC, 0xF0, 0x00]);
        assert_eq!(p2, [0x81, 0x04, 0x24, 0xAB]);
        let raw = to_raw28(&p1, &p2);
        assert_eq!(&raw[0..4], &p1, "页1 必须原样落在前 4 字节");
        assert_eq!(&raw[4..8], &p2, "页2 必须原样落在 5–8 字节");
        assert!(raw[8..].iter().all(|b| *b == 0), "页3–7 恒为零");
    }

    /// 原始直写与擦卡是两条路径：`build_raw` 全零入参的结果必须与 `ERASE_PAGES` 一致，
    /// 但擦卡仍应走 `erase_card`（有独立错误语义、不耗卡号）。
    #[test]
    fn build_raw_with_zero_pages_matches_erase() {
        let spec = RawSpec {
            p1: [0u8; 4],
            p2: [0u8; 4],
        };
        assert_eq!(build_raw(&spec), ERASE_PAGES);
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
