//! 真机集成测试：需要**插入读卡器并放一张卡**才能跑通。
//!
//! # 默认跳过
//!
//! 未设置环境变量 `RC_HW_TEST=1` 时全部跳过。原因：CI 与大多数开发机没有硬件，
//! 设成"默认跑"会让门禁永远是红的。
//!
//! # 运行方式
//!
//! 驱动只有 x86 版本，所以必须按 32 位目标跑：
//!
//! ```text
//! RC_HW_TEST=1 cargo test -p rc-device --test real_hardware \
//!     --target i686-pc-windows-msvc -- --nocapture
//! ```
//!
//! # 这些测试验证什么
//!
//! 1. 驱动能真正加载（`LoadLibrary` 成功、必需导出全可绑定）
//! 2. 端口探测能找到一个可用读卡器
//! 3. 打开序列能真正握手成功（`selecom → shake → config(0) → selecard`）
//! 4. 能读出卡片 28 字节，且解析结果自洽
//!
//! 第 3、4 条**只有真机能证明**——离线测试再好也只能证明字节构造正确，
//! 证明不了驱动协议实现正确。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use rc_device::card::CardKind;
use rc_device::card_device::CardDevice;
use rc_device::dll::DirectDllDevice;

/// 真机测试开关。
fn hw_enabled() -> bool {
    std::env::var("RC_HW_TEST").map(|v| v == "1").unwrap_or(false)
}

fn driver_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("drivers")
        .join("LC2000")
}

/// 准备一台可用设备；不可用时打印原因并返回 `None`（调用方据此跳过）。
///
/// 用函数而不是宏：宏在 `#[cfg]` 分支里返回 `()` 会让类型推断混乱。
fn device_or_skip(test_name: &str) -> Option<DirectDllDevice> {
    if !hw_enabled() {
        println!("[{test_name}] 跳过：未设置 RC_HW_TEST=1");
        return None;
    }

    // 驱动只有 x86 版本：在 64 位宿主里加载必然失败
    if !cfg!(target_arch = "x86") {
        println!(
            "[{test_name}] 跳过：当前是 {}，驱动只有 x86 版本；\
             请用 --target i686-pc-windows-msvc 运行",
            std::env::consts::ARCH
        );
        return None;
    }

    let dev = DirectDllDevice::with_auto_port(driver_dir());
    match dev.probe_ports() {
        Ok(ports) => {
            if ports.iter().any(|p| p.available) {
                Some(dev)
            } else {
                println!("[{test_name}] 跳过：未找到可用读卡器");
                None
            }
        }
        Err(e) => {
            println!("[{test_name}] 跳过：端口探测失败（{e}）");
            None
        }
    }
}

/// 驱动能真正加载，且全部必需导出可绑定。
#[test]
fn driver_loads_and_binds_all_exports() {
    let Some(dev) = device_or_skip("driver_loads") else {
        return;
    };
    dev.open().expect("真机打开应当成功");
    assert!(dev.is_open(), "打开后 is_open 应为 true");

    let info = dev.device_info().expect("应能取到设备信息");
    println!(
        "驱动 {} / 架构 {} / 端口 COM{} / 读卡器 {:?}",
        info.driver_path,
        info.driver_arch,
        u16::from(info.port) + 1,
        info.reader_id
    );
    assert!(info.opened);
    assert_eq!(
        info.driver_sha256.as_deref(),
        Some("f2f483e08ba8048ffd1c57e66cb35d9e32c1c1e280dc5f2ff100a5d881bccbc0"),
        "驱动版本必须与已验证版本一致"
    );

    dev.close().expect("关闭应当成功");
    assert!(!dev.is_open());
}

/// 端口探测能找到一个可用读卡器，并给出序列号。
#[test]
fn probe_finds_a_reader_with_serial() {
    let Some(dev) = device_or_skip("probe_ports") else {
        return;
    };
    let ports = dev.probe_ports().expect("探测应当成功");
    let found: Vec<_> = ports.iter().filter(|p| p.available).collect();
    assert!(!found.is_empty(), "应至少找到一个读卡器");

    for p in found {
        println!("可用: {} 读卡器={:?}", p.com_name, p.reader_id);
        assert!(p.reader_id.is_some(), "{} 应能读出序列号", p.com_name);
    }
}

/// 真机读卡：读出的字节必须自洽。
#[test]
fn read_card_from_real_hardware() {
    let Some(dev) = device_or_skip("read_card") else {
        return;
    };
    dev.open().expect("打开应当成功");

    let card = dev.read_card().expect("读卡应当成功");
    println!("卡型    : {}", card.kind.label());
    println!("房间序号: {:?}", card.room_index);
    println!("时长    : {:?}", card.duration);
    println!("卡号    : {:?}", card.card_no.map(|n| format!("0x{n:04X}")));
    println!("挂失    : {}", card.lost);
    println!("原始 28B: {}", card.raw_hex());

    if card.kind != CardKind::Blank {
        assert!(
            card.has_temic_prefix(),
            "非空白卡必须以 5B EC 开头，实际 {}",
            card.raw_hex()
        );
    }
    assert_eq!(card.raw.len(), 28, "卡数据固定 28 字节");
    assert_eq!(card.raw_hex().len(), 56, "十六进制串固定 56 字符");

    // 客卡与安装卡必须能解出房间序号
    if matches!(
        card.kind,
        CardKind::GuestDay | CardKind::GuestHour | CardKind::Install
    ) {
        assert!(
            card.room_index.is_some(),
            "{} 应当带房间序号",
            card.kind.label()
        );
        let idx = card.room_index.unwrap();
        assert!(idx <= 4094, "序号应在 0–4094（4095 与总控卡冲突）");
    }

    // 安装卡不占流水号、页2 固定 46 F2 00 01
    if card.kind == CardKind::Install {
        assert_eq!(card.card_no, None, "安装卡不应有作业流水号");
        assert_eq!(&card.raw[4..8], &[0x46, 0xF2, 0x00, 0x01]);
        assert!(!card.lost, "安装卡没有挂失概念");
    }

    dev.close().expect("关闭应当成功");
}

/// 离线自检在真机环境里也要全绿。
#[test]
fn selftest_green_on_hardware_machine() {
    let Some(dev) = device_or_skip("selftest") else {
        return;
    };
    let report = dev.selftest().expect("自检应当成功");
    println!("自检: {} 通过 / {} 失败", report.passed, report.failed);
    assert_eq!(report.total(), 46);
    assert!(report.is_all_green(), "46 项自检必须全绿");
}
