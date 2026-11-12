//! TEMIC 卡协议的**唯一常量来源**。
//!
//! # 为什么单独一个模块
//!
//! 卡格式里充满了裸写的十六进制字面量：`0x80` 是天数编码前缀、`0xC0` 是月编码前缀、
//! `<< 11` 是卡号的日段移位、`* 30` 是天数换月数……这些数字散落在构造与解析代码里时，
//! **一个笔误不会有任何编译错误**，只会让卡写错、让门打不开。
//!
//! 所以全部收敛到这里，并按语义分组命名。改协议只有这一个地方要动。
//!
//! # 注释里的【来源】标记
//!
//! 每个常量都注明结论是哪来的，沿用 `docs` 的惯例：
//!
//! - 【实测】—— 有原厂抓包样本或真机读卡证据
//! - 【反编译】—— IDA 反汇编原厂驱动/高层导出得出
//! - 【原 SDK】—— 照抄原 C# `PMSLockCard.exe` 的行为，已用实测样本验证
//!
//! 不要把【反编译】的结论当成【实测】用。

// ------------------------------------------------------------------ 卡数据布局

/// 卡数据的有效字节数。卡内只有 8 字节有效数据（页1 + 页2），
/// 但读卡器一次返回 28 字节，页 3–7 恒为 0。
///
/// 【实测】所有样本的最后 20 字节都是 `00`。
pub const CARD_BYTES: usize = 28;

/// 单页字节数（页1 / 页2 各 4 字节）。【原 SDK】`CardBuilder` 的页数组长度。
pub const PAGE_BYTES: usize = 4;

/// TEMIC 卡数据包头的魔数，恒定不变。
///
/// **仅用于包识别，不参与任何序号运算。** 序号只由 `B2`、`B3` 两个字节决定。
///
/// 【实测】33 条黄金向量的前两字节全部为 `5B EC`。
pub const CARD_PREFIX: [u8; 2] = [0x5B, 0xEC];

/// 卡数据里「有效载荷」的字节数：页1 的 4 字节 + 页2 的 4 字节。
///
/// 空白卡判定、页切分都以它为界，页 3–7 不参与解析。
///
/// 【原 SDK】`Parse` 只读 `raw28[0..8]`，其余字节一律忽略。
pub const PAYLOAD_BYTES: usize = 2 * PAGE_BYTES;

// ------------------------------------------------------------ 房间序号位打包（12 bit）

/// 房间序号的最大值：12 bit → `0x0FFF` = 4095。
///
/// 这是**卡格式的位宽上限**，不是业务规则——一个系统能建多少房间由业务层决定。
///
/// 【实测】页1 的 `B2` 高 4 位与 `B3` 全部 8 位共同承载序号。
pub const ROOM_INDEX_MAX: u16 = 0x0FFF;

/// 总控卡占用的房间序号：12 位全 1（`0x0FFF` = 4095）。
///
/// 【实测】现场总控卡页1 = `5B EC F0 FF`，按位打包解出正是 4095。
pub const MASTER_ROOM_INDEX: u16 = ROOM_INDEX_MAX;

/// 序号低 4 位在 `B2` 中的左移位数（低 4 位存放序号、低半字节之上才是序号）。
///
/// 【原 SDK】`p1[2] = (byte)(((roomIndex & 0x0F) << 4) | low)`。
pub const ROOM_LOW_NIBBLE_SHIFT: u32 = 4;

/// 序号低 4 位的掩码。【原 SDK】`roomIndex & 0x0F`。
pub const ROOM_LOW_NIBBLE_MASK: u16 = 0x0F;

/// 序号高 8 位的右移位数：`B3` 承载 `(roomIndex >> 4) & 0xFF`。【原 SDK】。
pub const ROOM_HIGH_SHIFT: u32 = 4;

/// 单字节掩码，用于把移位结果截到 8 位。【原 SDK】`& 0xFF`。
pub const BYTE_MASK: u16 = 0xFF;

/// 挂失标志在 `B2` 低 4 位上占据的位域掩码（`0x0F`）。
///
/// 判定规则：`(B2 & LOST_FLAG_BITS) != 0` 即为挂失。
///
/// **只对客卡有意义。** 其它卡型的低 4 位各有含义（安装卡固定 `0x0C`、
/// 校时卡恒为 `9`），解析时必须按卡型强制置 `false`，
/// 否则每张校时卡都会被读成「挂失卡」。
///
/// 【实测】挂失卡页1[2] 低 4 位 = 1；普通卡 = 0。
pub const LOST_FLAG_BITS: u8 = 0x0F;

/// 挂失标志在 `B2` 低 4 位的取值。【实测】挂失卡页1[2] 低 4 位 = 1。
pub const LOST_FLAG_SET: u8 = LOST_FLAG_BITS & 0x01;

/// 挂失标志未置位时的取值。【实测】普通卡页1[2] 低 4 位 = 0。
pub const LOST_FLAG_CLEAR: u8 = 0x00;

/// 安装卡在 `B2` 低 4 位的固定值。
///
/// 这一位**不是挂失标志**——安装卡没有挂失概念。按通用位打包解出来的
/// `lost = true` 是无意义的，解析时必须按卡型强制置 `false`。
///
/// 【原 SDK】`BuildInstall` 里硬编码 `| 0x0C`。
pub const INSTALL_LOW_NIBBLE: u8 = 0x0C;

// ------------------------------------------------------------------ 时长码

/// 天数简单段前缀：`0x80 | 天数`（天数 1–63）。
///
/// 【实测】`0x85` = 5 天、`0x81` = 1 天。
pub const DAY_SIMPLE_PREFIX: u8 = 0x80;

/// 月编码段前缀：`0xC0 | 月数`（天数 > 63 时使用）。
///
/// 【实测】90 天 → `0xC3`、1095 天 → `0xE4`。
pub const MONTH_PREFIX: u8 = 0xC0;

/// 小时编码段前缀：`0x40 | 值`。
///
/// 客卡编码 `小时 + 1`，总控卡编码 `小时` 本身。
///
/// 【实测】客卡 1 小时 → `0x42`，总控 1 小时 → `0x41`，两者差一位。
pub const HOUR_PREFIX: u8 = 0x40;

/// 时长码的 2 位段前缀掩码（`0xC0`），用于判断落在哪一段。
///
/// 【原 SDK】`Parse` 里用 `p2[0] & 0xC0` 区分月编码/小时/天数三段。
pub const DURATION_SEGMENT_MASK: u8 = 0xC0;

/// 时长码的数值掩码（`0x3F`），去掉段前缀后的天数 / 小时数 / 月数。
///
/// 【原 SDK】`p2[0] & 0x3F`。
pub const DURATION_MASK: u8 = 0x3F;

/// 天数换算成月数时的除数：**30 天/月**。
///
/// 锁按**日历月**执行。不能用 31 天：90 天会算成 2.9→3 月没错，但 1095 天会算成
/// 35 月而不是实测的 36 月，少约一个月。
///
/// 【实测】90→3 月、1095→36 月。
pub const DAYS_PER_MONTH: u16 = 30;

/// 银行家舍入的中值间隙：余数恰好等于它时说明命中了 `.5`，需要向偶数靠。
///
/// 等于 `DAYS_PER_MONTH / 2`，单独命名是因为它承载的语义（`.5` 判定）比数值更重要。
///
/// 【原 SDK】C# 的 `Math.Round(days / 30.0)` 默认 `MidpointRounding.ToEven`，
/// 即 `.5` 时向偶数靠：`1095/30 = 36.5` → **36**（`0xE4`），不是 37。
pub const ROUND_HALF_GAP: u16 = DAYS_PER_MONTH / 2;

/// 小时简单段的上限（总控卡）。
///
/// 总控卡编码 `0x40 | 小时`，63 是 6 位能表示的最大值。
///
/// 【实测】总控 63 小时 → `0x7F`，与客卡上限共用同一个编码值。
pub const MAX_HOURS_MASTER: u16 = 63;

/// 小时简单段的上限（客卡）。
///
/// 客卡编码 `0x40 | (小时 + 1)`，加 1 后占掉了 63 这一格，所以上限是 62。
///
/// 【实测】62 小时 → `0x7F`，63 小时被拒。
pub const MAX_GUEST_HOURS: u16 = 62;

/// 天数简单段的上限：63（`0x80 | 63` = `0xBF`）。【实测】。
pub const MAX_SIMPLE_DAYS: u16 = 63;

/// 天数的最小值：1。0 天不是合法时长，编码时按 1 天兜底。【原 SDK】。
pub const MIN_DAYS: u16 = 1;

/// 小时数的最小值：1。0 小时不是合法时长，编码时按 1 小时兜底。【原 SDK】。
pub const MIN_HOURS: u16 = 1;

/// 单张卡能表示的最大天数上限（业务层排期用）。【原 SDK】`BuildGuestDay` 的入参上限。
pub const MAX_TOTAL_DAYS: u16 = 1095;

/// 月数的上限：63（6 位）。超过后截断。【原 SDK】`DurationDays` 里的 clamp。
pub const MONTHS_MAX: u16 = 63;

/// 卡号「高字节」的右移位数。
///
/// 页2 的 `[1]` 存卡号低字节、`[2]` 存高字节（**小端**）。
///
/// 【实测】所有客卡样本的页2[1] 都是卡号低 8 位。
pub const CARD_NO_HIGH_SHIFT: u32 = 8;

// ------------------------------------------------------------ 作业流水号位域（16 bit）

/// 卡号里「日」段的左移位数。卡号 = 日(5 位) << 11 | 时(5 位) << 6 | 序号(6 位)。
///
/// 【实测】0x5B34 = 11 日 12 点 seq 52。
pub const CARD_NO_DAY_SHIFT: u32 = 11;

/// 卡号里「小时」段的左移位数。【实测】同上。
pub const CARD_NO_HOUR_SHIFT: u32 = 6;

/// 卡号「日」段的掩码（5 位）。【实测】。
pub const CARD_NO_DAY_MASK: u16 = 0x1F;

/// 卡号「小时」段的掩码（5 位）。【实测】。
pub const CARD_NO_HOUR_MASK: u16 = 0x1F;

/// 卡号「小时内序号」段的掩码（6 位）。【实测】。
pub const CARD_NO_SEQ_MASK: u16 = 0x3F;

/// 小时内序号的最大值（0 起算，故为 62）。【实测】第 63 张触发进位。
pub const SEQ_MAX: u8 = 62;

/// 一个小时内最多能发的卡数（1 起算，`SEQ_MAX + 1`）。【实测】满 63 张进位到下一小时。
pub const MAX_PER_HOUR: u16 = SEQ_MAX as u16 + 1;

/// 日段能表示的最大日历日（5 位）。【实测】。
pub const MAX_DAY_OF_MONTH: u8 = 31;

/// 小时段能表示的最大整点（5 位）。【实测】。
pub const MAX_HOUR_OF_DAY: u8 = 23;

/// 分钟的上限。【实测】校时卡位域。
pub const MAX_MINUTE: u8 = 59;

// ------------------------------------------------------------------ 年月字节

/// 年份基准：页2[3] 只存 `(year - 2000) & 0x0F`。【实测】2026-11 = `0xAB`。
pub const YEAR_BASE: i32 = 2000;

/// 年份循环周期：4 位年份只能表示 16 年。【实测】2026-11 与 2042-11 同为 `0xAB`。
pub const YEAR_CYCLE: i32 = 16;

/// 年份在年月字节里的左移位数（高 4 位）。【实测】。
pub const YEAR_SHIFT: u32 = 4;

/// 年份的 4 位掩码。【实测】。
pub const YEAR_MASK: i32 = 0x0F;

/// 月份的 4 位掩码（低 4 位）。【实测】。
pub const MONTH_MASK: u8 = 0x0F;

/// 月份的最小值。【实测】解析时用它判断「这张卡是否带卡号」。
pub const MIN_MONTH: u8 = 1;

/// 月份的最大值。【实测】同上。
pub const MAX_MONTH: u8 = 12;

/// 年份的合理范围下限（`CardIssuedAt` 校验用）。超出则页2[3] 无意义。
///
/// 【原 SDK】`CardBuilder.Suffix` 用 `(year - 2000) & 0x0F`，早于 2000 的年份会被截错。
pub const MIN_YEAR: i32 = 2000;

/// 年份的合理范围上限。
///
/// 【推断】取 2099：页2[3] 只有 4 位年份，跨 16 年就会重复，
/// 所以实际可用范围远小于此，这里只是挡住明显的非法入参。
pub const MAX_YEAR: i32 = 2099;

// ------------------------------------------------------------ 校时卡页1 位域

/// 页1[2] 的分钟低 4 位系数：`页1[2] = 16 × (分 & 0x0F) + 9`。
///
/// 【反编译】IDA `tem_writetimercard_sdk @ 0x7154`，7 组实测样本验证。
pub const TIMESYNC_MINUTE_LOW_FACTOR: u8 = 16;

/// 页1[2] 的固定偏移量：`+ 9`。
///
/// 它保证 `页1[2] & 0x09 == 0x09`，否则读卡会返回错误 4。
///
/// 【反编译】同上。
pub const TIMESYNC_MINUTE_LOW_OFFSET: u8 = 9;

/// 页1[2] 必须满足的位约束掩码（`0x09`）。【反编译】同上。
pub const TIMESYNC_PAGE1_REQUIRED_BITS: u8 = 0x09;

/// 页1[3] 的小时系数：`(4 × 时)`。
///
/// 【反编译】同上。
pub const TIMESYNC_HOUR_FACTOR: u8 = 4;

/// 页1[3] 的小时位掩码（`0x7C`），保证小时只占 `0011 1110` 这些位。【反编译】同上。
pub const TIMESYNC_HOUR_MASK: u8 = 0x7C;

/// 分钟低 4 位的掩码。【反编译】同上。
pub const MINUTE_LOW_MASK: u8 = 0x0F;

/// 分钟的高位右移位数（取高 2 位参与页1[3]）。【反编译】同上。
pub const MINUTE_HIGH_SHIFT: u32 = 4;

// ------------------------------------------------------------------ 各卡型固定标记

/// 安装卡页2 固定值 `46 F2 00 01`。安装卡**不含作业流水号**。
///
/// 【实测】安装卡 220 / 201 两条样本页2 完全相同。
pub const INSTALL_PAGE2: [u8; 4] = [0x46, 0xF2, 0x00, 0x01];

/// 复位卡页1 后两字节 `0A 00`。【实测】。
pub const RESET_PAGE1_TAIL: [u8; 2] = [0x0A, 0x00];

/// 复位卡页2 首字节 `0x00`。【实测】。
pub const RESET_PAGE2_HEAD: u8 = 0x00;

/// 清除卡页1 后两字节 `9B E5`。【实测】。
pub const CLEAR_PAGE1_TAIL: [u8; 2] = [0x9B, 0xE5];

/// 清除卡页2 首字节 `0xC7`。【实测】。
pub const CLEAR_PAGE2_HEAD: u8 = 0xC7;

/// 总控卡页1 后两字节 `F0 FF`（= 12 位序号全 1）。
///
/// 【实测】与「序号 4095 的客卡」字节完全相同，格式上无法区分。
pub const MASTER_PAGE1_TAIL: [u8; 2] = [0xF0, 0xFF];

/// 退房钟点设置卡页1 第 3 字节固定标记 `0x7B`。【实测】。
pub const CHECKOUT_HOUR_MARKER: u8 = 0x7B;

/// 退房钟点设置卡页2 首字节 `0xFF`。【实测】。
pub const CHECKOUT_HOUR_PAGE2_HEAD: u8 = 0xFF;

/// 校时卡页2 首字节 `0x06`（明文）。
///
/// 【反编译】IDA: `LOBYTE(v14[2]) = 6`。早期照抄的 `0x14` 是加扰残留。
pub const TIMESYNC_PAGE2_HEAD: u8 = 0x06;

/// 校时卡旧卡的页2 首字节 `0x14`（加扰残留）。
///
/// 读卡时必须能识别，否则现场旧校时卡会被判成未知卡型。
///
/// 【实测】现场存在此类旧卡。
pub const TIMESYNC_PAGE2_HEAD_LEGACY: u8 = 0x14;

/// 已退房标记：页2 = `12 12 12 12`。
///
/// 原 SDK 把它当作空白卡处理——锁不再认这张卡。
///
/// 【原 SDK】`Parse` 的第 3 步判定。
pub const CHECKED_OUT_MARKER: [u8; 4] = [0x12, 0x12, 0x12, 0x12];

/// 擦卡时写入的全零页。
///
/// 写零后 28 字节全零 = 空白卡。**注意**：这只擦白卡，不会让门锁里已有的授权消失。
///
/// 【实测】旧版持卡退房用的就是这个字节序列。
pub const ERASE_PAGE: [u8; 4] = [0x00; 4];

// ------------------------------------------------------------------ 驱动侧固定参数

/// 原厂固定写保护密码（TEMIC 卡的写密码）。
///
/// 写入时以**十六进制 ASCII 字符串**传给 `te_setuppwm_asc` / `te_writepwm_asc`，
/// 即这 8 个字符本身，不是 `0x12`、`0x00` 这样的字节。
///
/// # 这不是密钥
///
/// 它是原厂在**所有**设备上固定的写保护常量，不是用户密钥、不区分客户、
/// 不加密任何数据。违反「不硬编码密钥」的规范于此**显式豁免**，
/// 理由：它是协议的一部分，与原厂软件一致才能互操作；做成可配置反而会让
/// 用户改成错误值导致「能读不能写」。
///
/// 【实测】原厂软件与 33 条样本均使用此值。
pub const WRITE_PASSWORD: &str = "12000206";

/// 串口波特率：原厂固定 9600。【原 SDK】`LockCardOptions.Baud` 默认值。
pub const SERIAL_BAUD: u32 = 9600;

/// 选卡型参数：`5` = TEMIC 卡。【原 SDK】`mif_selecard(5)`。
pub const CARD_TYPE_TEMIC: i32 = 5;

/// 配置模式：`0` = 明文。
///
/// **必须**在写卡前调用，加扰模式会导致「能读不能写」。
///
/// 【实测】原厂软件同样传 0。
pub const CONFIG_MODE_PLAIN: i32 = 0;

/// 串口索引的合法范围上限（0 = COM1，7 = COM8）。【原 SDK】`ports` 子命令探测 0–7。
pub const MAX_PORT_INDEX: u8 = 7;

/// 写卡时的重试校验次数：写完立刻回读可能拿到旧值，需要重读。【实测】。
pub const VERIFY_ATTEMPTS: u32 = 3;

// ------------------------------------------------------------ 编译期不变量

// 以下断言在**编译期**求值：常量关系一旦被改坏，编译直接失败，
// 不必等到跑测试。写在模块级而非测试里，也让 clippy 不会把
// 恒真的 assert! 判为 assertion_on_constants。

/// 载荷（页1+页2）必须小于整卡 28 字节，页3–7 恒为 0。
const _: () = assert!(PAYLOAD_BYTES < CARD_BYTES);

/// 页3–7 恰好 20 字节。
const _: () = assert!(CARD_BYTES - PAYLOAD_BYTES == 20);

/// 客卡小时上限比总控少 1（客卡编码加 1 占掉了 63 那一格）。
const _: () = assert!(MAX_GUEST_HOURS < MAX_HOURS_MASTER);

/// 简单段天数上限必须小于总上限，否则月编码段永远不会被用到。
const _: () = assert!(MAX_SIMPLE_DAYS < MAX_TOTAL_DAYS);

/// 一个小时内第 n 张的最大值必须大于卡内序号最大值（n = seq + 1）。
const _: () = assert!(MAX_PER_HOUR > SEQ_MAX as u16);

/// 序号 12 位：4 位低 + 8 位高。
const _: () = assert!(ROOM_INDEX_MAX == (1 << 12) - 1);

/// 卡号三段恰好拼满 u16：日 5 位 << 11，时 5 位 << 6，序号 6 位。
const _: () = assert!(CARD_NO_DAY_SHIFT + 5 == 16);
const _: () = assert!(CARD_NO_HOUR_SHIFT + 5 == CARD_NO_DAY_SHIFT);
const _: () = assert!(CARD_NO_SEQ_MASK as u32 + 1 == 1 << CARD_NO_HOUR_SHIFT);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 位宽自洽：掩码与移位必须能表示出上限值。
    #[test]
    fn bit_widths_are_self_consistent() {
        assert_eq!(ROOM_INDEX_MAX, (1 << 12) - 1, "12 位序号的满值");
        assert_eq!(CARD_NO_DAY_MASK, (1 << 5) - 1, "日段 5 位");
        assert_eq!(CARD_NO_HOUR_MASK, (1 << 5) - 1, "时段 5 位");
        assert_eq!(CARD_NO_SEQ_MASK, (1 << 6) - 1, "序号段 6 位");
        assert_eq!(DAYS_PER_MONTH, 30);
        assert_eq!(ROUND_HALF_GAP, 15, "中值间隙必须是 30 的一半");
    }

    /// 卡号各段的移位必须互不重叠且拼满 16 位。
    #[test]
    fn card_no_shifts_tile_the_u16() {
        assert_eq!(
            CARD_NO_DAY_SHIFT,
            CARD_NO_HOUR_SHIFT + 5,
            "日段应紧跟时段的 5 位之后"
        );
        assert_eq!(CARD_NO_HOUR_SHIFT, 6, "序号段 6 位在前");
        // 日段最高位落在第 15 位，正好用满 u16
        assert_eq!(CARD_NO_DAY_SHIFT + 5, 16);
    }

    /// 页布局常量自洽。
    ///
    /// `PAYLOAD_BYTES < CARD_BYTES` 与「页3–7 恰好 20 字节」已提到模块级
    /// 编译期断言，这里只校验数值本身。
    #[test]
    fn page_layout_is_consistent() {
        assert_eq!(PAYLOAD_BYTES, 8, "页1 + 页2 共 8 字节有效数据");
        assert_eq!(CARD_BYTES - PAYLOAD_BYTES, 20, "页3–7 共 20 字节恒为 0");
    }

    /// 时长码两段前缀不能撞车，否则解析分不清天数与月数。
    #[test]
    fn duration_prefixes_do_not_collide() {
        assert_eq!(DAY_SIMPLE_PREFIX & DURATION_SEGMENT_MASK, 0x80);
        assert_eq!(MONTH_PREFIX & DURATION_SEGMENT_MASK, 0xC0);
        assert_eq!(HOUR_PREFIX & DURATION_SEGMENT_MASK, 0x40);
        assert_ne!(
            DAY_SIMPLE_PREFIX & DURATION_SEGMENT_MASK,
            MONTH_PREFIX & DURATION_SEGMENT_MASK
        );
        assert_ne!(
            DAY_SIMPLE_PREFIX & DURATION_SEGMENT_MASK,
            HOUR_PREFIX & DURATION_SEGMENT_MASK
        );
    }

    /// 时长码前缀不能占用数值位，否则天数会被前缀污染。
    #[test]
    fn duration_prefix_does_not_overlap_value_bits() {
        assert_eq!(DAY_SIMPLE_PREFIX & DURATION_MASK, 0);
        assert_eq!(MONTH_PREFIX & DURATION_MASK, 0);
        assert_eq!(HOUR_PREFIX & DURATION_MASK, 0);
    }

    /// 上限常量的相对大小必须合理。
    ///
    /// 「客卡上限比总控少 1」「简单段上限小于总上限」已提到模块级编译期断言，
    /// 这里只补一个运行期关系：nth 上限必须大于卡内序号上限。
    #[test]
    fn limits_are_ordered() {
        assert!(MAX_PER_HOUR > u16::from(SEQ_MAX), "第 n 张上限须大于卡内序号上限");
    }

    /// 序号位打包的掩码 + 移位必须恰好覆盖 12 位。
    #[test]
    fn room_index_packing_covers_12_bits() {
        assert_eq!(ROOM_LOW_NIBBLE_MASK, 0x0F, "低 4 位");
        assert_eq!(ROOM_LOW_NIBBLE_SHIFT, 4, "低 4 位左移到高半字节");
        assert_eq!(ROOM_HIGH_SHIFT, 4, "高 8 位从第 4 位起");
        // 4 位（低）+ 8 位（高）= 12 位
        assert_eq!(4 + 8, 12);
    }

    /// 年月字节的 4+4 位布局自洽。
    #[test]
    fn year_month_packing_covers_8_bits() {
        assert_eq!(YEAR_CYCLE, 16, "4 位年份 → 16 年一循环");
        assert_eq!(YEAR_MASK, YEAR_CYCLE - 1);
        assert_eq!(YEAR_SHIFT, 4);
        assert_eq!(MONTH_MASK, 0x0F);
    }

    /// 校时卡位域必须保证 `页1[2] & 0x09 == 0x09` 恒成立。
    #[test]
    fn timesync_bitfield_satisfies_required_bits() {
        for minute_low in 0..=15u8 {
            let b2 = TIMESYNC_MINUTE_LOW_FACTOR * minute_low + TIMESYNC_MINUTE_LOW_OFFSET;
            assert_eq!(
                b2 & TIMESYNC_PAGE1_REQUIRED_BITS,
                TIMESYNC_PAGE1_REQUIRED_BITS,
                "分钟低 4 位 {minute_low} 违反位约束"
            );
        }
    }

    /// 写密码是 8 个 ASCII 字符，驱动按十六进制字符串解析。
    #[test]
    fn write_password_is_eight_ascii_chars() {
        assert_eq!(WRITE_PASSWORD.len(), 8);
        assert!(WRITE_PASSWORD.chars().all(|c| c.is_ascii_digit()));
    }
}
