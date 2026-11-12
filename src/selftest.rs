//! 离线自检：46 项黄金用例，逐字节比对原厂实测样本。
//!
//! # 为什么需要它
//!
//! 卡字节构造是从 C# 重新移植到 Rust 的。原 SDK 的 `selftest` 验的是 **C# 自己的
//! 构造器**，验不到 Rust 的实现。所以必须把 46 项向量搬到 Rust 这边，
//! 让「字节与旧软件一致」这件事在 `cargo test` 里被机械保证。
//!
//! # 构成
//!
//! | 类别 | 项数 | 内容 |
//! |---|---|---|
//! | 字节向量 | 33 | 覆盖 8 种卡型，逐字节比对页1/页2 |
//! | 作业流水号 | 7 | seq 0/1/62、解码、第 64 张进位、第 65 张不倒退、跨日 |
//! | 时长码 | 6 | 63/64/90/150/360/1095 天 |
//!
//! # 固定时间基准
//!
//! 向量统一用 `2026-11-08 14:00:00`，让年月字节恒为 `0xAB`。
//! 若跟随真实时间，2027 年会变成 `0xBB`，黄金向量就漂移了。
//!
//! # 纯离线
//!
//! 不加载 DLL、不开串口、不需要插卡。随时可跑，可在 CI 里跑。

use crate::builder::{
    build_checkout_hour, build_clear, build_guest_day, build_guest_hour, build_install,
    build_master, build_reset, build_timesync, duration_days_code, next_stamp, timestamp_card_no,
    to_raw28,
};
use crate::card::{
    CardDuration, CardIssuedAt, CardKind, CheckoutHourSpec, GuestDaySpec, GuestHourSpec,
    InstallSpec, MasterSpec, PlainCardSpec, SelfTestCase, SelfTestReport, TimeSyncSpec,
};
use crate::error::DeviceResult;
use crate::parser::parse;

/// 黄金基准时刻：让年月字节恒为 `0xAB`。
fn golden_at() -> CardIssuedAt {
    CardIssuedAt {
        year: 2026,
        month: 11,
        day: 8,
        hour: 14,
    }
}

/// 一条字节向量。
struct Vector {
    name: &'static str,
    kind: CardKind,
    spec: VectorSpec,
    /// 期望的页1 十六进制。
    p1: &'static str,
    /// 期望的页2 十六进制。
    p2: &'static str,
}

/// 向量各自的构造入参。
enum VectorSpec {
    GuestDay {
        room_index: u16,
        days: u16,
        card_no: u16,
    },
    GuestHour {
        room_index: u16,
        hours: u16,
        card_no: u16,
    },
    Master {
        duration: CardDuration,
        card_no: u16,
    },
    Reset {
        card_no: u16,
    },
    Clear {
        card_no: u16,
    },
    Install {
        room_index: u16,
    },
    CheckoutHour {
        hour: u8,
        card_no: u16,
    },
    TimeSync {
        hour: u8,
        minute: u8,
        card_no: u16,
    },
}

/// 33 条黄金向量，取自原厂抓包样本。
///
/// **顺序与原 SDK 的 `Vectors` 数组一致**，便于逐条对照。
const VECTORS: &[Vector] = &[
    // ---- 日租客卡 7 条 ----
    Vector {
        name: "220/5天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 15, days: 5, card_no: 0x2288 },
        p1: "5BECF000",
        p2: "858822AB",
    },
    Vector {
        name: "209/1天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 6, days: 1, card_no: 0x2250 },
        p1: "5BEC6000",
        p2: "815022AB",
    },
    Vector {
        name: "260/1天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 39, days: 1, card_no: 0x2281 },
        p1: "5BEC7002",
        p2: "818122AB",
    },
    Vector {
        // 序号 0 的实测样本：旧 CLI 因 --room-index 限制根本发不出来
        name: "201/1天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 0, days: 1, card_no: 0x224A },
        p1: "5BEC0000",
        p2: "814A22AB",
    },
    Vector {
        name: "205/3天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 3, days: 3, card_no: 0x2249 },
        p1: "5BEC3000",
        p2: "834922AB",
    },
    Vector {
        name: "205/4天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 3, days: 4, card_no: 0x2249 },
        p1: "5BEC3000",
        p2: "844922AB",
    },
    Vector {
        name: "220/1天",
        kind: CardKind::GuestDay,
        spec: VectorSpec::GuestDay { room_index: 15, days: 1, card_no: 0x2403 },
        p1: "5BECF000",
        p2: "810324AB",
    },
    // ---- 钟点客卡 5 条 ----
    Vector {
        name: "220/1小时",
        kind: CardKind::GuestHour,
        spec: VectorSpec::GuestHour { room_index: 15, hours: 1, card_no: 0x228A },
        p1: "5BECF000",
        p2: "428A22AB",
    },
    Vector {
        name: "220/3小时",
        kind: CardKind::GuestHour,
        spec: VectorSpec::GuestHour { room_index: 15, hours: 3, card_no: 0x2289 },
        p1: "5BECF000",
        p2: "448922AB",
    },
    Vector {
        name: "220/6小时",
        kind: CardKind::GuestHour,
        spec: VectorSpec::GuestHour { room_index: 15, hours: 6, card_no: 0x228B },
        p1: "5BECF000",
        p2: "478B22AB",
    },
    Vector {
        name: "220/62小时",
        kind: CardKind::GuestHour,
        spec: VectorSpec::GuestHour { room_index: 15, hours: 62, card_no: 0x228C },
        p1: "5BECF000",
        p2: "7F8C22AB",
    },
    Vector {
        name: "220/1小时#2",
        kind: CardKind::GuestHour,
        spec: VectorSpec::GuestHour { room_index: 15, hours: 1, card_no: 0x2404 },
        p1: "5BECF000",
        p2: "420424AB",
    },
    // ---- 总控卡 10 条 ----
    Vector {
        name: "总控卡1天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(1), card_no: 0x2248 },
        p1: "5BECF0FF",
        p2: "814822AB",
    },
    Vector {
        name: "总控2小时",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Hours(2), card_no: 0x2BC2 },
        p1: "5BECF0FF",
        p2: "42C22BAB",
    },
    Vector {
        name: "总控3小时",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Hours(3), card_no: 0x2BC3 },
        p1: "5BECF0FF",
        p2: "43C32BAB",
    },
    Vector {
        name: "总控2天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(2), card_no: 0x2BC4 },
        p1: "5BECF0FF",
        p2: "82C42BAB",
    },
    Vector {
        name: "总控63天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(63), card_no: 0x2BC7 },
        p1: "5BECF0FF",
        p2: "BFC72BAB",
    },
    Vector {
        name: "总控90天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(90), card_no: 0x2BC8 },
        p1: "5BECF0FF",
        p2: "C3C82BAB",
    },
    Vector {
        name: "总控120天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(120), card_no: 0x2BC9 },
        p1: "5BECF0FF",
        p2: "C4C92BAB",
    },
    Vector {
        name: "总控360天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(360), card_no: 0x2BCB },
        p1: "5BECF0FF",
        p2: "CCCB2BAB",
    },
    Vector {
        name: "总控1095天",
        kind: CardKind::Master,
        spec: VectorSpec::Master { duration: CardDuration::Days(1095), card_no: 0x2BCD },
        p1: "5BECF0FF",
        // 0xE4 = 0xC0|36：银行家舍入，不是 37
        p2: "E4CD2BAB",
    },
    // ---- 校时卡 6 条 ----
    Vector {
        name: "校时08:30",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 8, minute: 30, card_no: 0x4200 },
        p1: "5BECE921",
        p2: "060042AB",
    },
    Vector {
        name: "校时12:00",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 12, minute: 0, card_no: 0x4300 },
        p1: "5BEC0930",
        p2: "060043AB",
    },
    Vector {
        name: "校时19:47",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 19, minute: 47, card_no: 0x44C0 },
        p1: "5BECF94E",
        p2: "06C044AB",
    },
    Vector {
        name: "校时22:00",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 22, minute: 0, card_no: 0x4580 },
        p1: "5BEC0958",
        p2: "068045AB",
    },
    Vector {
        name: "校时11日19:47",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 19, minute: 47, card_no: 0x5CC0 },
        p1: "5BECF94E",
        p2: "06C05CAB",
    },
    Vector {
        name: "校时16:35",
        kind: CardKind::TimeSync,
        spec: VectorSpec::TimeSync { hour: 16, minute: 35, card_no: 0x4C01 },
        p1: "5BEC3942",
        p2: "06014CAB",
    },
    // ---- 复位 / 清除 / 安装 / 钟点 ----
    Vector {
        name: "复位卡",
        kind: CardKind::Reset,
        spec: VectorSpec::Reset { card_no: 0x2441 },
        p1: "5BEC0A00",
        p2: "004124AB",
    },
    Vector {
        name: "清除卡",
        kind: CardKind::Clear,
        spec: VectorSpec::Clear { card_no: 0x2442 },
        p1: "5BEC9BE5",
        p2: "C74224AB",
    },
    Vector {
        name: "安装卡220",
        kind: CardKind::Install,
        spec: VectorSpec::Install { room_index: 15 },
        p1: "5BECFC00",
        p2: "46F20001",
    },
    Vector {
        name: "安装卡201",
        kind: CardKind::Install,
        spec: VectorSpec::Install { room_index: 0 },
        p1: "5BEC0C00",
        p2: "46F20001",
    },
    Vector {
        name: "钟点卡12点",
        kind: CardKind::CheckoutHour,
        spec: VectorSpec::CheckoutHour { hour: 12, card_no: 0x228F },
        p1: "5BEC7B0C",
        p2: "FF8F22AB",
    },
    Vector {
        name: "钟点卡14点",
        kind: CardKind::CheckoutHour,
        spec: VectorSpec::CheckoutHour { hour: 14, card_no: 0x2290 },
        p1: "5BEC7B0E",
        p2: "FF9022AB",
    },
];

fn hex4(b: [u8; 4]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(crate::card::HEX_UPPER[(x >> 4) as usize] as char);
        s.push(crate::card::HEX_UPPER[(x & 0x0F) as usize] as char);
    }
    s
}

/// 按向量描述构造页1/页2。
fn build_vector(v: &Vector) -> DeviceResult<([u8; 4], [u8; 4])> {
    let at = golden_at();
    match v.spec {
        VectorSpec::GuestDay {
            room_index,
            days,
            card_no,
        } => build_guest_day(&GuestDaySpec {
            room_index,
            days,
            card_no,
            lost: false,
            issued_at: at,
        }),
        VectorSpec::GuestHour {
            room_index,
            hours,
            card_no,
        } => build_guest_hour(&GuestHourSpec {
            room_index,
            hours,
            card_no,
            lost: false,
            issued_at: at,
        }),
        VectorSpec::Master { duration, card_no } => build_master(&MasterSpec {
            duration,
            card_no,
            issued_at: at,
        }),
        VectorSpec::Reset { card_no } => build_reset(&PlainCardSpec {
            card_no,
            issued_at: at,
        }),
        VectorSpec::Clear { card_no } => build_clear(&PlainCardSpec {
            card_no,
            issued_at: at,
        }),
        VectorSpec::Install { room_index } => build_install(&InstallSpec { room_index }),
        VectorSpec::CheckoutHour { hour, card_no } => build_checkout_hour(&CheckoutHourSpec {
            hour,
            card_no,
            issued_at: at,
        }),
        VectorSpec::TimeSync {
            hour,
            minute,
            card_no,
        } => build_timesync(&TimeSyncSpec {
            hour,
            minute,
            card_no,
            issued_at: at,
        }),
    }
}

/// 跑一遍完整自检，返回逐项明细。
///
/// 共 **46 项**：33 条字节向量 + 7 条作业流水号 + 6 条时长码。
pub fn run() -> SelfTestReport {
    let mut cases = Vec::with_capacity(46);

    // ---- 33 条字节向量 ----
    for v in VECTORS {
        let (ok, expected, actual) = match build_vector(v) {
            Ok((p1, p2)) => {
                let g1 = hex4(p1);
                let g2 = hex4(p2);
                let bytes_ok = g1 == v.p1 && g2 == v.p2;
                // 反向校验：构造出来的字节必须能被解析回原卡型
                // （安装卡与总控卡不做参数级反向校验，见下方说明）
                let parse_ok = if bytes_ok {
                    reverse_check(v, p1, p2)
                } else {
                    false
                };
                (
                    bytes_ok && parse_ok,
                    format!("{}/{}", v.p1, v.p2),
                    format!("{g1}/{g2}"),
                )
            }
            Err(e) => (false, format!("{}/{}", v.p1, v.p2), format!("构造失败：{e}")),
        };
        cases.push(SelfTestCase {
            name: v.name.to_string(),
            ok,
            expected,
            actual,
        });
    }

    // ---- 7 条作业流水号 ----
    let t0 = golden_at();

    // seq 0 / 1 / 62
    for (seq, want) in [(0u8, 0x4380u16), (1, 0x4381), (62, 0x43BE)] {
        let (ok, actual) = match timestamp_card_no(&t0, seq) {
            Ok(got) => (got == want, format!("0x{got:04X}")),
            Err(e) => (false, format!("错误：{e}")),
        };
        cases.push(SelfTestCase {
            name: format!("卡号 seq={seq}"),
            ok,
            expected: format!("0x{want:04X}"),
            actual,
        });
    }

    // 解码实测样本 0x5B34 = 11 日 12 点 seq 52
    {
        let s = crate::builder::decode_timestamp_card_no(0x5B34);
        let ok = s.day == 11 && s.hour == 12 && s.seq == 52;
        cases.push(SelfTestCase {
            name: "解码 0x5B34".to_string(),
            ok,
            expected: "11日12点seq52".to_string(),
            actual: format!("{}日{}点seq{}", s.day, s.hour, s.seq),
        });
    }

    // 第 64 张：满 63 张进位到下一小时、n 归 1
    {
        let (at, nth) = next_stamp(0x43BE, &t0);
        let got = timestamp_card_no(&at, nth - 1).unwrap_or(0);
        // 期望：nth=1、卡号 0x43C0（15 点）
        let ok = nth == 1 && got == 0x43C0;
        cases.push(SelfTestCase {
            name: "第64张进位".to_string(),
            ok,
            expected: "nth=1/0x43C0".to_string(),
            actual: format!("nth={nth}/0x{got:04X}"),
        });
    }

    // 第 65 张（时钟仍是 14:00）：必须仍在 15 点，不能退回 14 点
    {
        let (at, nth) = next_stamp(0x43C0, &t0);
        let got = timestamp_card_no(&at, nth - 1).unwrap_or(0);
        let ok = nth == 2 && got == 0x43C1;
        cases.push(SelfTestCase {
            name: "第65张不倒退".to_string(),
            ok,
            expected: "nth=2/0x43C1".to_string(),
            actual: format!("nth={nth}/0x{got:04X}"),
        });
    }

    // 跨日进位：23 点发满 → 次日 0 点第 1 张
    {
        let t1 = CardIssuedAt {
            hour: 23,
            ..t0
        };
        let last_of_day = timestamp_card_no(&t1, 62).unwrap_or(0);
        let (at, nth) = next_stamp(last_of_day, &t1);
        let got = timestamp_card_no(&at, nth - 1).unwrap_or(0);
        let want = 9u16 << 11;
        let ok = nth == 1 && got == want;
        cases.push(SelfTestCase {
            name: "跨日进位".to_string(),
            ok,
            expected: format!("nth=1/0x{want:04X}"),
            actual: format!("nth={nth}/0x{got:04X}"),
        });
    }

    // ---- 6 条时长码 ----
    for (days, want) in [
        (63u16, 0xBFu8),
        (64, 0xC2),
        (90, 0xC3),
        (150, 0xC5),
        (360, 0xCC),
        (1095, 0xE4),
    ] {
        let got = duration_days_code(days);
        cases.push(SelfTestCase {
            name: format!("DurationDays({days})"),
            ok: got == want,
            expected: format!("0x{want:02X}"),
            actual: format!("0x{got:02X}"),
        });
    }

    let passed = cases.iter().filter(|c| c.ok).count() as u32;
    let failed = cases.len() as u32 - passed;
    SelfTestReport {
        passed,
        failed,
        cases,
    }
}

/// 反向校验：构造出的字节能被解析回原卡型与关键参数。
///
/// # 不做反向校验的卡型
///
/// - **总控卡**：页2[0] 是时长码，与客卡同段；解析时靠页1 的 `F0 FF` 区分，
///   而 `F0 FF` 同时是「序号 4095 的客卡」。总控卡页1 与序号 4095 的客卡字节完全相同，
///   这是格式内在歧义，不是实现问题。
/// - **安装卡**：页1 低 4 位是固定 `0x0C`，按通用位打包解出的 `lost` 为 true，
///   解析器已按卡型强制置 false，这里只需要验证字节一致。
fn reverse_check(v: &Vector, p1: [u8; 4], p2: [u8; 4]) -> bool {
    if matches!(v.kind, CardKind::Master | CardKind::Install) {
        return true;
    }
    let raw = to_raw28(&p1, &p2);
    let Ok(snap) = parse(&raw) else {
        return false;
    };
    if snap.kind != v.kind {
        return false;
    }
    match v.spec {
        VectorSpec::GuestDay {
            room_index,
            days,
            card_no,
        } => {
            snap.room_index == Some(room_index)
                && snap.card_no == Some(card_no)
                // 月编码段是有损的（×30 还原），只对简单段要求精确
                && (days > 63
                    || snap.duration == Some(CardDuration::Days(days)))
        }
        VectorSpec::GuestHour {
            room_index,
            hours,
            card_no,
        } => {
            snap.room_index == Some(room_index)
                && snap.card_no == Some(card_no)
                && snap.duration == Some(CardDuration::Hours(hours))
        }
        VectorSpec::Reset { card_no } | VectorSpec::Clear { card_no } => {
            snap.card_no == Some(card_no)
        }
        VectorSpec::CheckoutHour { hour, card_no } => {
            snap.checkout_hour == Some(hour) && snap.card_no == Some(card_no)
        }
        VectorSpec::TimeSync { card_no, .. } => snap.card_no == Some(card_no),
        VectorSpec::Master { .. } | VectorSpec::Install { .. } => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 自检必须跑满 46 项且全部通过。
    ///
    /// 这是本 crate 的**核心门禁**：它同时保证了
    /// 「字节与旧软件一致」和「构造与解析互逆」两件事。
    #[test]
    fn selftest_is_all_green() {
        let report = run();
        assert_eq!(report.total(), 46, "自检项数必须是 46");
        assert_eq!(report.passed, 46, "自检未全绿：{report:?}");
        assert_eq!(report.failed, 0);
        assert!(report.is_all_green());
    }

    /// 失败项必须能看出「期望 vs 实际」，否则现场无从排查。
    #[test]
    fn every_case_carries_expected_and_actual() {
        for case in run().cases {
            assert!(!case.name.trim().is_empty(), "用例缺少名称");
            assert!(!case.expected.trim().is_empty(), "{} 缺少期望值", case.name);
            assert!(!case.actual.trim().is_empty(), "{} 缺少实际值", case.name);
        }
    }

    /// 向量数量与分类必须与文档一致：33 + 7 + 6。
    #[test]
    fn case_counts_match_documented_breakdown() {
        assert_eq!(VECTORS.len(), 33, "字节向量必须是 33 条");
        let report = run();
        let vectors = report
            .cases
            .iter()
            .filter(|c| !c.name.starts_with("DurationDays(") && !c.name.starts_with("卡号 seq=")
                && !c.name.starts_with("解码") && !c.name.contains("张") && !c.name.starts_with("跨日"))
            .count();
        assert_eq!(vectors, 33, "字节向量计数不符");
        let durations = report
            .cases
            .iter()
            .filter(|c| c.name.starts_with("DurationDays("))
            .count();
        assert_eq!(durations, 6, "时长码必须是 6 条");
    }

    /// 自检不依赖真实时间：任意时刻跑都应全绿。
    #[test]
    fn selftest_does_not_depend_on_wall_clock() {
        // golden_at() 是固定值，不读系统时钟；这里只是把这条约定写成断言
        let a = golden_at();
        assert_eq!(
            (a.year, a.month, a.day, a.hour),
            (2026, 11, 8, 14),
            "黄金基准时刻不能变"
        );
    }

    /// 自检结果必须能跨 IPC 传给前端。
    #[test]
    fn report_serializes_over_ipc() {
        let json = serde_json::to_string(&run()).expect("自检报告必须可序列化");
        assert!(json.contains("\"passed\":46"), "实际: {json}");
        assert!(json.contains("\"failed\":0"));
    }
}
