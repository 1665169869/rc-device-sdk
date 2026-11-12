//! 真机联调入口：探测端口、读卡、跑自检。
//!
//! 用法（必须先按 32 位目标编译，因为驱动只有 x86 版本）：
//!
//! ```text
//! cargo run -p rc-device --bin probe --target i686-pc-windows-msvc
//! ```

use std::path::PathBuf;

use rc_device::card_device::CardDevice;
use rc_device::dll::DirectDllDevice;

fn main() {
    let driver_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("drivers")
        .join("LC2000");

    println!("=== 硬件层真机联调 ===");
    println!("宿主架构: {}", std::env::consts::ARCH);
    println!("驱动目录: {}", driver_dir.display());

    let dev = DirectDllDevice::with_auto_port(&driver_dir);

    // 1) 设备信息
    match dev.device_info() {
        Ok(info) => {
            println!("\n[设备信息]");
            println!("  后端      : {:?}", info.backend);
            println!("  驱动路径  : {}", info.driver_path);
            println!("  驱动架构  : {}", info.driver_arch);
            println!("  SHA-256   : {}", info.driver_sha256.as_deref().unwrap_or("(读不出)"));
            println!("  串口      : COM{}", u16::from(info.port) + 1);
            println!("  已打开    : {}", info.opened);
        }
        Err(e) => {
            println!("\n[设备信息] 失败: {e}");
            println!("  建议: {}", e.hint());
        }
    }

    // 2) 离线自检（不碰硬件）
    match dev.selftest() {
        Ok(r) => println!(
            "\n[离线自检] {} 通过 / {} 失败（共 {} 项）",
            r.passed,
            r.failed,
            r.total()
        ),
        Err(e) => println!("\n[离线自检] 失败: {e}"),
    }

    // 3) 探测端口
    println!("\n[端口探测] 逐个尝试 0–7 ...");
    match dev.probe_ports() {
        Ok(list) => {
            let mut any = false;
            for p in &list {
                if p.available {
                    any = true;
                    println!(
                        "  ✓ {} (索引 {})  读卡器={}",
                        p.com_name,
                        p.port,
                        p.reader_id.as_deref().unwrap_or("(未知)")
                    );
                } else if let Some(f) = &p.failure {
                    println!("  ✗ {} (索引 {})  {}", p.com_name, p.port, f);
                }
            }
            if !any {
                println!("  未找到可用读卡器");
            }
        }
        Err(e) => println!("  探测失败: {e}"),
    }

    // 4) 读卡
    println!("\n[读卡] 请把卡放在读卡器上 ...");
    match dev.read_card() {
        Ok(card) => {
            println!("  卡型    : {}", card.kind.label());
            println!("  序号    : {:?}", card.room_index);
            println!("  时长    : {:?}", card.duration);
            println!("  卡号    : {:?}", card.card_no.map(|n| format!("0x{n:04X}")));
            println!("  挂失    : {}", card.lost);
            println!("  发卡时刻: {:?}", card.stamp);
            println!("  年月字节: 0x{:02X}", card.year_month_byte);
            println!("  原始 28B: {}", card.raw_hex());
        }
        Err(e) => {
            println!("  读卡失败: {e}");
            println!("  建议: {}", e.hint());
        }
    }
}
