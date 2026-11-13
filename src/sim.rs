//! 内存模拟后端：不加载驱动、不开串口，让演示与无硬件环境跑完整发卡流程。
//!
//! # 它模拟的是「设备」，不是「业务」
//!
//! [`SimCardDevice`] 实现完整的 [`CardDevice`]：写卡走同一套 [`crate::builder`]，
//! 读卡走同一个 [`crate::parser`]，因此**字节与真机逐 byte 相同**，
//! 只是把「写进卡」换成「写进内存里的卡槽」。业务层（房态、入住单、发卡上限）
//! 一行都不需要知道当前是真机还是模拟。
//!
//! # 与真机的两处刻意差异
//!
//! | 行为 | 真机 | 模拟 |
//! |---|---|---|
//! | 写卡后回读 | 驱动可能返回旧值，重试 3 次 | 立即返回刚写入的字节 |
//! | 失败注入 | 无卡 / 回读不一致 | 只有「没放卡」（[`SimCardDevice::remove_card`]） |
//!
//! 也就是说模拟**不会**制造写卡失败——想测失败分支请直接 mock [`CardDevice`]，
//! 不要指望模拟后端随机报错（那会让演示变成掷骰子）。
//!
//! # 为什么不做成 `--sim` 之外的开关
//!
//! 模拟只由进程启动参数选择（见 `bin/rc-device-sidecar.rs` 的 `--sim`），
//! 进入运行期后没有任何方法能切成真机：一次会话要么全模拟，要么全真机，
//! 避免「前半段写内存、后半段写卡」这种既查不出问题也复现不了的中间状态。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::builder;
use crate::card::{
    CheckoutHourSpec, DeviceBackend, DeviceInfo, DriverHashStatus, GuestDaySpec, GuestHourSpec,
    InstallSpec, MasterSpec, PlainCardSpec, PortProbe, RawSpec, SelfTestReport, TimeSyncSpec,
    CARD_BYTES,
};
use crate::card_device::CardDevice;
use crate::error::{DeviceError, DeviceResult};
use crate::parser;
use crate::protocol::{MAX_PORT_INDEX, SERIAL_BAUD};
use crate::{CardSnapshot, DriverArch};

/// 模拟设备上报的读卡器序列号。
pub const SIM_READER_ID: &str = "SIM-READER";

/// 内存里的门锁卡读写器。
///
/// 构造时默认插入一张**空白卡**（全零 28 字节）：模拟环境下「写卡前先读一次卡」
/// 这条硬性前置检查才能通过，否则任何发卡都会在第一步报无卡。
pub struct SimCardDevice {
    opened: AtomicBool,
    port: AtomicU8,
    /// 卡槽里当前的 28 字节。`None` = 读卡器上没放卡。
    slot: Mutex<Option<[u8; CARD_BYTES]>>,
    /// 依次写入过的卡，供测试断言「到底写没写、写了几次」。
    writes: Mutex<Vec<[u8; CARD_BYTES]>>,
}

impl SimCardDevice {
    /// 新建模拟设备，卡槽里预置一张空白卡。
    pub fn new() -> Self {
        Self {
            opened: AtomicBool::new(false),
            // 与真机默认端口保持一致（COM7 = 索引 6）
            port: AtomicU8::new(6),
            slot: Mutex::new(Some([0u8; CARD_BYTES])),
            writes: Mutex::new(Vec::new()),
        }
    }

    /// 新建模拟设备，卡槽里放指定的 28 字节。
    ///
    /// 用于「模拟一张已发出的客卡」这类演示场景。
    pub fn with_card(raw: [u8; CARD_BYTES]) -> Self {
        let dev = Self::new();
        if let Ok(mut slot) = dev.slot.lock() {
            *slot = Some(raw);
        }
        dev
    }

    /// 往卡槽里放一张卡（模拟用户把卡放到读卡器上）。
    pub fn insert_card(&self, raw: [u8; CARD_BYTES]) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(raw);
        }
    }

    /// 把卡从读卡器上拿走，之后的读卡返回 [`DeviceError::NoCard`]。
    pub fn remove_card(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }

    /// 卡槽里当前的 28 字节；没放卡时为 `None`。
    pub fn card(&self) -> Option<[u8; CARD_BYTES]> {
        self.slot.lock().ok().and_then(|slot| *slot)
    }

    /// 到目前为止所有写入过的卡（按时间顺序）。
    pub fn writes(&self) -> Vec<[u8; CARD_BYTES]> {
        self.writes.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// 写卡核心：把两页拼成 28 字节、放进卡槽、记录一次，再按真机的路径解析回来。
    ///
    /// 解析失败**不回滚卡槽**：真机上卡已经被写坏了，模拟也必须表现成「卡是坏的」，
    /// 否则「原始直写写废卡」这条最需要被看见的路径在模拟里反而看不见。
    fn write_pages(&self, p1: [u8; 4], p2: [u8; 4]) -> DeviceResult<CardSnapshot> {
        self.ensure_open()?;
        let raw = builder::to_raw28(&p1, &p2);
        {
            let mut slot = self.lock_slot()?;
            *slot = Some(raw);
        }
        {
            let mut writes = self
                .writes
                .lock()
                .map_err(|_| DeviceError::Io("模拟写卡记录锁被污染".to_string()))?;
            writes.push(raw);
        }
        parser::parse(&raw)
    }

    fn lock_slot(&self) -> DeviceResult<MutexGuard<'_, Option<[u8; CARD_BYTES]>>> {
        self.slot
            .lock()
            .map_err(|_| DeviceError::Io("模拟卡槽锁被污染".to_string()))
    }

    /// 模拟设备没有串口，但写卡/读卡前仍要求「已打开」，
    /// 与真机的 `ensure_open` 语义对齐（未打开时自动打开，而不是报错）。
    fn ensure_open(&self) -> DeviceResult<()> {
        if !self.is_open() {
            self.open()?;
        }
        Ok(())
    }
}

impl Default for SimCardDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl CardDevice for SimCardDevice {
    fn open(&self) -> DeviceResult<()> {
        self.opened.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn close(&self) -> DeviceResult<()> {
        self.opened.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened.load(Ordering::SeqCst)
    }

    fn read_card(&self) -> DeviceResult<CardSnapshot> {
        self.ensure_open()?;
        let raw = {
            let slot = self.lock_slot()?;
            *slot
        };
        match raw {
            Some(bytes) => parser::parse(&bytes),
            None => Err(DeviceError::NoCard),
        }
    }

    fn write_guest_day(&self, spec: &GuestDaySpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_guest_day(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_guest_hour(&self, spec: &GuestHourSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_guest_hour(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_master(&self, spec: &MasterSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_master(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_install(&self, spec: &InstallSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_install(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_reset(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_reset(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_clear(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_clear(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_checkout_hour(&self, spec: &CheckoutHourSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_checkout_hour(spec)?;
        self.write_pages(p1, p2)
    }

    fn write_timesync(&self, spec: &TimeSyncSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_timesync(spec)?;
        self.write_pages(p1, p2)
    }

    fn erase_card(&self) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::ERASE_PAGES;
        self.write_pages(p1, p2)
    }

    fn write_raw(&self, spec: &RawSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_raw(spec);
        self.write_pages(p1, p2)
    }

    fn probe_ports(&self) -> DeviceResult<Vec<PortProbe>> {
        let current = self.port.load(Ordering::SeqCst);
        Ok((0..=MAX_PORT_INDEX)
            .map(|port| {
                let available = port == current;
                PortProbe {
                    port,
                    com_name: format!("COM{}", u16::from(port) + 1),
                    available,
                    reader_id: available.then(|| SIM_READER_ID.to_string()),
                    failure: (!available).then(|| "模拟设备：该串口未插入读卡器".to_string()),
                }
            })
            .collect())
    }

    fn set_port(&self, port: u8) -> DeviceResult<()> {
        if port > MAX_PORT_INDEX {
            return Err(DeviceError::Param(format!(
                "串口索引必须在 0–{MAX_PORT_INDEX}，实际 {port}"
            )));
        }
        self.port.store(port, Ordering::SeqCst);
        Ok(())
    }

    fn selftest(&self) -> DeviceResult<SelfTestReport> {
        // 自检本来就是纯函数，模拟与真机跑的是同一份 46 项。
        Ok(crate::selftest::run())
    }

    fn device_info(&self) -> DeviceResult<DeviceInfo> {
        Ok(DeviceInfo {
            backend: DeviceBackend::Mock,
            driver_path: "(sim)".to_string(),
            driver_sha256: None,
            // 模拟后端没有驱动文件，谈不上与已验证版本比对
            driver_hash_status: DriverHashStatus::Unknown,
            driver_arch: DriverArch::X86,
            port: self.port.load(Ordering::SeqCst),
            baud: SERIAL_BAUD,
            opened: self.is_open(),
            reader_id: self.is_open().then(|| SIM_READER_ID.to_string()),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::card::{CardIssuedAt, CardKind};

    fn issued_at() -> CardIssuedAt {
        CardIssuedAt {
            year: 2026,
            month: 11,
            day: 8,
            hour: 14,
        }
    }

    fn guest_day(index: u16) -> GuestDaySpec {
        GuestDaySpec {
            room_index: index,
            days: 1,
            card_no: builder::timestamp_card_no(&issued_at(), 0).unwrap(),
            lost: false,
            issued_at: issued_at(),
        }
    }

    /// 默认卡槽必须是空白卡：否则「写卡前先读卡」在模拟环境里永远失败。
    #[test]
    fn new_device_holds_a_blank_card() {
        let dev = SimCardDevice::new();
        let snap = dev.read_card().unwrap();
        assert_eq!(snap.kind, CardKind::Blank);
        assert!(dev.is_open(), "读卡会顺带打开设备");
    }

    /// 写卡结果必须与 builder 构造的字节完全一致，且能原样读回。
    #[test]
    fn written_card_is_the_builder_output() {
        let dev = SimCardDevice::new();
        let spec = guest_day(15);
        let written = dev.write_guest_day(&spec).unwrap();
        let (p1, p2) = builder::build_guest_day(&spec).unwrap();

        assert_eq!(written.raw, builder::to_raw28(&p1, &p2));
        assert_eq!(written.kind, CardKind::GuestDay);
        assert_eq!(written.room_index, Some(15));

        let back = dev.read_card().unwrap();
        assert_eq!(back.raw, written.raw);
        assert_eq!(dev.writes().len(), 1);
    }

    /// 擦卡写全零，卡槽里留下的就是空白卡。
    #[test]
    fn erase_leaves_a_blank_card() {
        let dev = SimCardDevice::new();
        dev.write_guest_day(&guest_day(1)).unwrap();
        let erased = dev.erase_card().unwrap();
        assert_eq!(erased.kind, CardKind::Blank);
        assert!(erased.raw.iter().all(|b| *b == 0));
    }

    /// 没放卡就是 `NoCard`，而不是「空白卡」——两者在前端的提示完全不同。
    #[test]
    fn removed_card_reports_no_card() {
        let dev = SimCardDevice::new();
        dev.remove_card();
        let err = dev.read_card().unwrap_err();
        assert!(matches!(err, DeviceError::NoCard));
    }

    /// 原始直写写废的卡必须「坏得像真的」：字节留下，解析报 Unknown 而不是失败。
    #[test]
    fn raw_write_keeps_garbage_bytes() {
        let dev = SimCardDevice::new();
        let spec = RawSpec {
            p1: [0x11, 0x22, 0x33, 0x44],
            p2: [0x55, 0x66, 0x77, 0x88],
        };
        let snap = dev.write_raw(&spec).unwrap();
        assert_eq!(snap.kind, CardKind::Unknown);
        assert_eq!(&snap.raw[0..8], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        // 卡槽里留下的是这批字节，不会被回滚成写之前的样子
        assert_eq!(dev.card().unwrap()[0..8], [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    }

    /// 自检是纯函数，模拟后端必须也是 46 项全绿——这是「模拟可信」的唯一依据。
    #[test]
    fn selftest_is_the_same_46_cases() {
        let dev = SimCardDevice::new();
        let report = dev.selftest().unwrap();
        assert!(report.is_all_green(), "{} 项失败", report.failed);
        assert_eq!(report.total(), 46);
    }

    /// 端口探测：当前端口可用，其余给出「未插入读卡器」。
    #[test]
    fn probe_ports_marks_only_current_port() {
        let dev = SimCardDevice::new();
        dev.set_port(3).unwrap();
        let ports = dev.probe_ports().unwrap();
        assert_eq!(ports.len(), 8);
        let usable: Vec<u8> = ports.iter().filter(|p| p.available).map(|p| p.port).collect();
        assert_eq!(usable, vec![3]);
        assert_eq!(ports[3].reader_id.as_deref(), Some(SIM_READER_ID));
    }

    /// 越界端口必须拒绝，且不能改掉当前端口。
    #[test]
    fn set_port_rejects_out_of_range() {
        let dev = SimCardDevice::new();
        let err = dev.set_port(8).unwrap_err();
        assert!(matches!(err, DeviceError::Param(_)));
        assert_eq!(dev.probe_ports().unwrap()[6].available, true, "端口不应被改动");
    }
}
