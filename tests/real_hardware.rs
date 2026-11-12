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
use rc_device::InstallSpec;

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

/// 真机写卡：写入安装卡并回读校验。
///
/// # 为什么写安装卡
///
/// 安装卡**不占作业流水号**（页2 固定 `46 F2 00 01`），是唯一一种
/// 「写坏了也不会污染卡号序列」的卡。客卡写坏会让流水号凭空推进一张，
/// 那是比写失败更难收拾的事故。
///
/// # 为什么要"按原内容重写"
///
/// 本测试会**真的改动卡片内容**。为了做到可反复运行、且不把现场有用的卡
/// 变成别的东西，这里只做幂等写入：
///
/// 1. 先读卡，确认它当前就是安装卡；
/// 2. 用**读到的同一个 `room_index`** 重写一遍。
///
/// 这样写进去的字节与原来完全相同，验证了整条写卡链路却不让卡变成另一种卡。
/// 若卡不是安装卡，直接跳过——绝不在陌生卡上试写。
#[test]
fn write_install_card_to_real_hardware() {
    let Some(dev) = device_or_skip("write_install") else {
        return;
    };
    dev.open().expect("打开应当成功");

    // 1) 先看看卡里现在是什么
    let before = dev.read_card().expect("写卡前应先能读卡");
    println!("[写卡前] 卡型: {}", before.kind.label());
    println!("[写卡前] 序号: {:?}", before.room_index);
    println!("[写卡前] 原始: {}", before.raw_hex());

    if before.kind != CardKind::Install {
        println!(
            "[write_install] 跳过：当前卡是 {}，不是安装卡。\
             本测试只在安装卡上做幂等重写，不在陌生卡上试写。",
            before.kind.label()
        );
        dev.close().ok();
        return;
    }

    let room_index = before
        .room_index
        .expect("安装卡必须带房间序号，否则解析已出问题");

    // 2) 用同一个序号重写，字节应当与原来完全一致
    let spec = InstallSpec { room_index };
    let written = dev.write_install(&spec).expect("写安装卡应当成功");

    println!("[写卡后] 卡型: {}", written.kind.label());
    println!("[写卡后] 序号: {:?}", written.room_index);
    println!("[写卡后] 原始: {}", written.raw_hex());

    // 3) 回读校验的结论必须是"写的就是读到的"
    assert_eq!(written.kind, CardKind::Install, "写回后应仍是安装卡");
    assert_eq!(
        written.room_index,
        Some(room_index),
        "写回后序号应保持不变"
    );
    assert_eq!(
        written.raw_hex(),
        before.raw_hex(),
        "按原序号重写，28 字节应与写前完全一致"
    );
    assert_eq!(written.card_no, None, "安装卡不占作业流水号");

    // 4) 再独立读一次，确认不是只信写卡返回的快照
    let after = dev.read_card().expect("写卡后应能再次读到");
    assert_eq!(after.raw_hex(), before.raw_hex(), "独立重读应与写前一致");
    assert_eq!(after.kind, CardKind::Install);

    dev.close().expect("关闭应当成功");
}

/// 真机擦卡：页1/页2 全零，卡变空白卡；随后写回安装卡。
///
/// # 这张卡是耗材
///
/// 擦卡会清空卡内容，之后必须重新写入才有用。本测试擦完立刻按原序号写回，
/// 所以卡最终仍是一张可用的安装卡——但中途确实被擦白过。
///
/// # 验证顺序为什么是这样
///
/// 1. 读卡，记下原序号（若本来就不是安装卡则跳过，不在陌生卡上乱擦）；
/// 2. `erase_card()` → 断言返回 `CardKind::Blank`、28 字节全零；
/// 3. 独立再读一次，确认**卡里**真的空了，而不只是返回快照骗人；
/// 4. 写回安装卡，确认擦卡没把卡弄成不可写的状态。
///
/// 第 3 步是关键：只信 `erase_card()` 的返回值等于自证，
/// 必须用一次全新的读卡确认硬件上真的写进去了。
#[test]
fn erase_card_on_real_hardware_then_restore() {
    let Some(dev) = device_or_skip("erase_card") else {
        return;
    };
    dev.open().expect("打开应当成功");

    let before = dev.read_card().expect("擦卡前应先能读卡");
    println!("[擦卡前] 卡型: {}", before.kind.label());
    println!("[擦卡前] 序号: {:?}", before.room_index);
    println!("[擦卡前] 原始: {}", before.raw_hex());

    if before.kind != CardKind::Install {
        println!(
            "[erase_card] 跳过：当前卡是 {}，不是安装卡。\
             擦卡会清空内容，本测试只在确认可弃用的安装卡上做。",
            before.kind.label()
        );
        dev.close().ok();
        return;
    }
    let room_index = before
        .room_index
        .expect("安装卡必须带房间序号");

    // 1) 擦卡
    let erased = dev.erase_card().expect("擦卡应当成功");
    println!("[擦卡后] 卡型: {}", erased.kind.label());
    println!("[擦卡后] 原始: {}", erased.raw_hex());

    assert_eq!(erased.kind, CardKind::Blank, "擦完应是空白卡");
    assert_eq!(erased.raw, [0u8; 28], "擦完 28 字节应全零");
    assert_eq!(
        erased.room_index, None,
        "空白卡不该带房间序号"
    );
    assert_eq!(erased.card_no, None, "擦卡不消耗作业流水号");

    // 2) 独立重读：确认卡里真的空了，不是返回快照骗人
    let reread = dev.read_card().expect("擦卡后应仍能读卡");
    println!("[重读] 卡型: {}", reread.kind.label());
    println!("[重读] 原始: {}", reread.raw_hex());
    assert_eq!(reread.kind, CardKind::Blank, "硬件上应真的是空白卡");
    assert_eq!(reread.raw, [0u8; 28], "硬件上 28 字节应全零");

    // 3) 写回安装卡：确认擦卡后卡仍可写，且内容恢复
    let restored = dev
        .write_install(&InstallSpec { room_index })
        .expect("擦卡后应能重新写入安装卡");
    println!("[写回] 卡型: {}", restored.kind.label());
    println!("[写回] 原始: {}", restored.raw_hex());

    assert_eq!(restored.kind, CardKind::Install);
    assert_eq!(restored.raw_hex(), before.raw_hex(), "写回后应与擦卡前一致");

    // 4) 再独立读一次收尾
    let final_read = dev.read_card().expect("写回后应能读到");
    assert_eq!(final_read.raw_hex(), before.raw_hex());
    assert_eq!(final_read.kind, CardKind::Install);

    dev.close().expect("关闭应当成功");
}
