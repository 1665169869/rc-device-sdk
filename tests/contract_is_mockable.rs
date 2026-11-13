//! 证明两个 trait 都可以被 mock —— 业务层测试不需要任何硬件。
//!
//! 这份文件**不是产品代码**，它存在的意义是把「契约可测」这件事固定下来：
//! 只要 `cargo test` 通过，就说明业务层写单测时能零成本替换掉真设备。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use rc_device::{
    CardDevice, CardDuration, CardIssuedAt, CardKind, CardSnapshot, CheckoutHourSpec, DeviceBackend,
    DeviceError, DeviceInfo, DeviceResult, DriverArch, DriverHashStatus, GuestDaySpec, GuestHourSpec,
    IdCardData, IdCardProbeSpec, IdCardReadSpec, IdCardReader, IdCardServiceInfo, InstallSpec,
    MasterSpec, PlainCardSpec, PortProbe, RawSpec, SelfTestReport, TimeSyncSpec, CARD_BYTES,
};

/// 发卡时刻固定值，让测试不随真实时间漂移。
fn issued_at() -> CardIssuedAt {
    CardIssuedAt {
        year: 2026,
        month: 11,
        day: 8,
        hour: 14,
    }
}

fn blank_snapshot() -> CardSnapshot {
    let mut raw = [0u8; CARD_BYTES];
    raw[0] = 0x5B;
    raw[1] = 0xEC;
    CardSnapshot {
        kind: CardKind::Blank,
        room_index: None,
        duration: None,
        checkout_hour: None,
        card_no: None,
        lost: false,
        stamp: None,
        year_month_byte: 0,
        raw,
    }
}

/// 内存里的假设备：记录调用、回放预置结果，从不碰驱动。
struct MockCardDevice {
    open: AtomicBool,
    port: AtomicU8,
    /// 下一次读卡要返回的结果。
    read_result: Mutex<DeviceResult<CardSnapshot>>,
    /// 下一次写卡要返回的结果。
    write_result: Mutex<DeviceResult<CardSnapshot>>,
    /// 依次记录收到的客卡入参，供断言「业务层传对了参数」。
    guest_day_calls: Mutex<Vec<GuestDaySpec>>,
}

impl MockCardDevice {
    fn new() -> Self {
        Self {
            open: AtomicBool::new(false),
            port: AtomicU8::new(6),
            read_result: Mutex::new(Ok(blank_snapshot())),
            write_result: Mutex::new(Ok(blank_snapshot())),
            guest_day_calls: Mutex::new(Vec::new()),
        }
    }

    fn next_write(&self) -> DeviceResult<CardSnapshot> {
        self.write_result
            .lock()
            .expect("锁被污染")
            .as_ref()
            .cloned()
            .map_err(Clone::clone)
    }
}

impl CardDevice for MockCardDevice {
    fn open(&self) -> DeviceResult<()> {
        self.open.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn close(&self) -> DeviceResult<()> {
        self.open.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    fn read_card(&self) -> DeviceResult<CardSnapshot> {
        self.read_result
            .lock()
            .expect("锁被污染")
            .clone()
    }

    fn write_guest_day(&self, spec: &GuestDaySpec) -> DeviceResult<CardSnapshot> {
        self.guest_day_calls
            .lock()
            .expect("锁被污染")
            .push(*spec);
        self.next_write()
    }

    fn write_guest_hour(&self, _spec: &GuestHourSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_master(&self, _spec: &MasterSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_install(&self, _spec: &InstallSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_reset(&self, _spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_clear(&self, _spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_checkout_hour(&self, _spec: &CheckoutHourSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_timesync(&self, _spec: &TimeSyncSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn erase_card(&self) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn write_raw(&self, _spec: &RawSpec) -> DeviceResult<CardSnapshot> {
        self.next_write()
    }

    fn probe_ports(&self) -> DeviceResult<Vec<PortProbe>> {
        Ok(vec![PortProbe {
            port: 6,
            com_name: "COM7".into(),
            available: true,
            reader_id: Some("4E57".into()),
            failure: None,
        }])
    }

    fn set_port(&self, port: u8) -> DeviceResult<()> {
        if port > 7 {
            return Err(DeviceError::Param(format!("串口索引越界：{port}")));
        }
        self.port.store(port, Ordering::SeqCst);
        Ok(())
    }

    fn selftest(&self) -> DeviceResult<SelfTestReport> {
        Ok(SelfTestReport {
            passed: 46,
            failed: 0,
            cases: vec![],
        })
    }

    fn device_info(&self) -> DeviceResult<DeviceInfo> {
        Ok(DeviceInfo {
            backend: DeviceBackend::Mock,
            driver_path: "(mock)".into(),
            driver_sha256: None,
            // 模拟后端没有真实驱动文件，谈不上与已验证版本比对
            driver_hash_status: DriverHashStatus::Unknown,
            driver_arch: DriverArch::X86,
            port: self.port.load(Ordering::SeqCst),
            baud: 9600,
            opened: self.is_open(),
            reader_id: None,
        })
    }
}

/// 内存里的假身份证读卡器。
struct MockIdCardReader {
    result: Mutex<DeviceResult<IdCardData>>,
}

impl MockIdCardReader {
    fn returning(data: IdCardData) -> Self {
        Self {
            result: Mutex::new(Ok(data)),
        }
    }

    fn failing(err: DeviceError) -> Self {
        Self {
            result: Mutex::new(Err(err)),
        }
    }
}

impl IdCardReader for MockIdCardReader {
    fn read(&self, _spec: &IdCardReadSpec) -> DeviceResult<IdCardData> {
        self.result.lock().expect("锁被污染").clone()
    }

    fn probe(&self, _spec: &IdCardProbeSpec) -> DeviceResult<IdCardServiceInfo> {
        Ok(IdCardServiceInfo {
            endpoint: "ws://127.0.0.1:9001".into(),
            reachable: true,
            service_version: Some("mock".into()),
            failure: None,
        })
    }
}

// ---------------------------------------------------------------- 用例

/// 契约的核心承诺：业务测试可以完全不碰硬件。
#[test]
fn mock_device_satisfies_contract_without_any_hardware() {
    let dev: Box<dyn CardDevice> = Box::new(MockCardDevice::new());

    assert!(!dev.is_open());
    dev.open().expect("mock 打开应当成功");
    assert!(dev.is_open());

    let card = dev.read_card().expect("mock 读卡应当成功");
    assert_eq!(card.kind, CardKind::Blank);
    assert!(card.is_blank());

    dev.close().expect("mock 关闭应当成功");
    assert!(!dev.is_open());
}

/// 业务层通过 trait 传的入参必须原样到达硬件层——这是解耦后唯一的接口面。
#[test]
fn guest_day_spec_reaches_device_unchanged() {
    let mock = MockCardDevice::new();
    let spec = GuestDaySpec {
        room_index: 15,
        days: 5,
        card_no: 0x2288,
        lost: false,
        issued_at: issued_at(),
    };
    mock.write_guest_day(&spec).expect("mock 写卡应当成功");

    let calls = mock.guest_day_calls.lock().expect("锁被污染");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], spec);
    // 入参里只有序号，没有任何房号文本
    assert_eq!(calls[0].room_index, 15);
}

/// 硬件错误能穿过 trait 对象原样传回，业务层据此分支。
#[test]
fn device_errors_pass_through_trait_object() {
    let mock = MockCardDevice::new();
    *mock.write_result.lock().expect("锁被污染") = Err(DeviceError::NoCard);

    let dev: Box<dyn CardDevice> = Box::new(mock);
    let err = dev
        .write_guest_day(&GuestDaySpec {
            room_index: 0,
            days: 1,
            card_no: 1,
            lost: false,
            issued_at: issued_at(),
        })
        .expect_err("应当返回 NoCard");

    assert_eq!(err.sdk_code(), 5);
    assert!(!err.is_device_fault(), "没放卡是操作问题，不是设备故障");
    assert!(err.hint().contains("125 kHz"));
}

/// 读卡器故障与门锁卡故障互不影响：身份证读不出来时，门锁卡照常能发。
#[test]
fn id_card_failure_does_not_block_card_device() {
    let cards: Box<dyn CardDevice> = Box::new(MockCardDevice::new());
    let idc: Box<dyn IdCardReader> =
        Box::new(MockIdCardReader::failing(DeviceError::IdCardService(
            "连不上 9001".into(),
        )));

    let read_err = idc
        .read(&IdCardReadSpec::default())
        .expect_err("应当报身份证服务错误");
    assert_eq!(read_err.sdk_code(), -1);
    assert!(read_err.hint().contains("9001"));

    // 门锁卡设备完全不受影响
    cards.open().expect("门锁卡设备应当照常可用");
    assert!(cards.selftest().expect("自检应当成功").is_all_green());
}

/// 空壳身份证结果不算读到人，业务层据此不落空白住客。
#[test]
fn id_card_shell_result_is_not_identity() {
    let idc = MockIdCardReader::returning(IdCardData {
        device_model: Some("HS-2000".into()),
        ..Default::default()
    });
    let data = idc.read(&IdCardReadSpec::default()).expect("读卡应当成功");
    assert!(!data.has_identity());
}

/// 串口索引越界属于参数错误，在硬件层就要拦住。
#[test]
fn out_of_range_port_is_a_param_error() {
    let dev = MockCardDevice::new();
    let err = dev.set_port(8).expect_err("索引 8 应当被拒绝");
    assert_eq!(err.sdk_code(), 1);
}

/// 自检报告能通过 trait 返回并判定全绿。
#[test]
fn selftest_report_survives_trait_boundary() {
    let dev: Box<dyn CardDevice> = Box::new(MockCardDevice::new());
    let report = dev.selftest().expect("自检应当成功");
    assert_eq!(report.total(), 46);
    assert!(report.is_all_green());
}

/// 生产用的模拟后端（`sim::SimCardDevice`）必须同样满足契约。
///
/// 与 `MockCardDevice` 的区别：mock 只回放预置结果，模拟后端真的跑
/// `builder` + `parser`，因此它能抓住「契约被实现歪了」这类问题——
/// 写进去的字节必须能原样读回来，擦卡必须写出全零，自检必须 46 项全绿。
#[test]
fn sim_backend_satisfies_the_contract() {
    let dev = rc_device::sim::SimCardDevice::new();
    let spec = GuestDaySpec {
        room_index: 15,
        days: 1,
        card_no: rc_device::timestamp_card_no(&issued_at(), 0).expect("卡号应可编码"),
        lost: false,
        issued_at: issued_at(),
    };

    let written = dev.write_guest_day(&spec).expect("写卡应当成功");
    assert_eq!(written.kind, CardKind::GuestDay);
    assert_eq!(written.room_index, Some(15));

    let back = dev.read_card().expect("读卡应当成功");
    assert_eq!(back.raw, written.raw, "写进去的字节必须能原样读回来");

    let erased = dev.erase_card().expect("擦卡应当成功");
    assert_eq!(erased.kind, CardKind::Blank);
    assert!(erased.raw.iter().all(|b| *b == 0));

    assert!(dev.selftest().expect("自检应当成功").is_all_green());
}

/// 两个 trait 可以同时被同一个业务用例持有，互不干扰。
#[test]
fn both_traits_coexist_in_one_use_case() {
    let cards = MockCardDevice::new();
    let idc = MockIdCardReader::returning(IdCardData {
        name: Some("张三".into()),
        id_number: Some("110101199001011234".into()),
        ..Default::default()
    });

    cards.open().expect("打开应当成功");
    let guest = idc.read(&IdCardReadSpec::default()).expect("读证应当成功");
    assert!(guest.has_identity());

    let card = cards
        .write_guest_day(&GuestDaySpec {
            room_index: 15,
            days: 1,
            card_no: 0x2403,
            lost: false,
            issued_at: issued_at(),
        })
        .expect("写卡应当成功");
    // mock 回放的是预置快照，这里只断言调用链是通的
    assert_eq!(card.kind, CardKind::Blank);
}

/// 总控卡与客卡的时长单位走同一个枚举，语义不歧义。
#[test]
fn duration_variants_are_unambiguous() {
    let master = MasterSpec {
        duration: CardDuration::Hours(2),
        card_no: 0x2BC2,
        issued_at: issued_at(),
    };
    assert_eq!(master.duration.amount(), 2);
    assert!(!master.duration.is_days());
}
