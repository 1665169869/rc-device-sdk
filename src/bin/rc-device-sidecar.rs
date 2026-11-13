//! 32 位设备宿主进程：把门锁卡操作从 64 位主程序里隔离出来。
//!
//! # 为什么需要它
//!
//! `LC32RFRW.dll` 只有 x86 版本，Windows 不允许 64 位进程加载 32 位 DLL。
//! 本进程按 `i686-pc-windows-msvc` 构建，负责真正碰硬件；64 位壳层通过
//! stdin/stdout 的 NDJSON 与它对话（协议见 [`rc_device::sidecar`]）。
//!
//! # 用法
//!
//! ```text
//! rc-device-sidecar.exe [--driver-dir <目录>] [--sim [--sim-card <初始卡>]]
//! ```
//!
//! 驱动目录优先级：`--driver-dir` > 环境变量 `RC_DRIVER_DIR` > exe 同目录
//! 的 `drivers/LC2000`。
//!
//! `--sim`（别名 `--mock`）把后端换成内存模拟（[`rc_device::sim::SimCardDevice`]）：
//! 不加载驱动、不开串口，卡槽里预置一张卡，写卡字节仍由 `builder` 生成。
//! 线上协议与真机**完全一致**，因此演示、CI 与前端联调不需要任何硬件。
//! `--sim-card` 指定预置卡：`blank`（默认）/ `guest-day` / `guest-hour` / `none`（没放卡）。
//!
//! # 纪律
//!
//! - **stdout 只写响应行**，任何日志都走 stderr——否则壳层会读到半行 JSON；
//! - 收到 `shutdown` 或 stdin EOF 就退出，不留孤儿进程；
//! - 读卡器会话留在本进程内（常驻），避免每次调用都重新开串口。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use rc_device::dll::DirectDllDevice;
use rc_device::sidecar::{Request, RequestId, Response};
use rc_device::sim::SimCardDevice;
use rc_device::{
    build_guest_day, build_guest_hour, timestamp_card_no, to_raw28, CardDevice, CardIssuedAt,
    DeviceError, DeviceResult, GuestDaySpec, GuestHourSpec, Operation,
};
use serde::Serialize;
use serde_json::Value;

fn main() {
    let driver_dir = resolve_driver_dir();
    let device = open_device(flag_present("--sim") || flag_present("--mock"), &driver_dir);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[sidecar] 读取 stdin 失败：{e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let Request { id, body } = match Request::from_line(&line) {
            Ok(req) => req,
            Err(e) => {
                let resp = Response::failure(
                    0,
                    DeviceError::Protocol(format!("请求无法解析：{e}")).payload(),
                );
                if !write_line(&mut stdout, &resp) {
                    break;
                }
                continue;
            }
        };

        let shutdown = matches!(body, Operation::Shutdown);
        eprintln!("[sidecar] #{id} {}", body.name());
        let resp = dispatch(device.as_ref(), id, body);
        if !write_line(&mut stdout, &resp) {
            break;
        }
        if shutdown {
            break;
        }
    }

    // 进程退出前把串口关掉：否则下一次启动可能抢不到端口。
    let _ = device.close();
    eprintln!("[sidecar] 已退出");
}

/// 按启动参数选后端。模拟与真机走同一个 `CardDevice` 契约，dispatch 不需要分支。
fn open_device(sim: bool, driver_dir: &Path) -> Box<dyn CardDevice> {
    if sim {
        let kind = arg_text("--sim-card").unwrap_or_else(|| "blank".to_string());
        match sim_device(&kind) {
            Ok(dev) => {
                eprintln!("[sidecar] 模拟模式：不碰硬件，初始卡槽 = {kind}");
                Box::new(dev)
            }
            Err(msg) => {
                eprintln!("[sidecar] --sim-card 取值无效：{msg}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!(
            "[sidecar] 架构 {}，驱动目录 {}",
            std::env::consts::ARCH,
            driver_dir.display()
        );
        if !cfg!(target_arch = "x86") {
            eprintln!("[sidecar] 警告：当前不是 32 位构建，加载 LC32RFRW.dll 会返回 DriverArchMismatch");
        }
        Box::new(DirectDllDevice::new(driver_dir))
    }
}

/// 构造预置了某张卡的模拟设备。
///
/// 固定发卡时刻（2026-11-08 14:00）而不是读系统时间：模拟会话里的卡应该**可复现**，
/// 每次启动拿到不同卡号只会让演示与截图对不上。
fn sim_device(kind: &str) -> Result<SimCardDevice, String> {
    let at = CardIssuedAt {
        year: 2026,
        month: 11,
        day: 8,
        hour: 14,
    };
    let card_no = timestamp_card_no(&at, 0).map_err(|e| e.to_string())?;

    match kind {
        "none" => {
            let dev = SimCardDevice::new();
            dev.remove_card();
            Ok(dev)
        }
        "blank" => Ok(SimCardDevice::new()),
        "guest-day" => {
            let spec = GuestDaySpec {
                room_index: 15,
                days: 1,
                card_no,
                lost: false,
                issued_at: at,
            };
            let (p1, p2) = build_guest_day(&spec).map_err(|e| e.to_string())?;
            Ok(SimCardDevice::with_card(to_raw28(&p1, &p2)))
        }
        "guest-hour" => {
            let spec = GuestHourSpec {
                room_index: 15,
                hours: 3,
                card_no,
                lost: false,
                issued_at: at,
            };
            let (p1, p2) = build_guest_hour(&spec).map_err(|e| e.to_string())?;
            Ok(SimCardDevice::with_card(to_raw28(&p1, &p2)))
        }
        other => Err(format!(
            "必须是 blank / guest-day / guest-hour / none，实际 {other:?}"
        )),
    }
}

/// 执行一条操作并把它转成响应。
fn dispatch(device: &dyn CardDevice, id: RequestId, op: Operation) -> Response {
    match op {
        Operation::Open => from_result(id, device.open()),
        Operation::Close => from_result(id, device.close()),
        Operation::IsOpen => from_result(id, Ok(device.is_open())),
        Operation::ReadCard => from_result(id, device.read_card()),
        Operation::EraseCard => from_result(id, device.erase_card()),
        Operation::WriteRaw { spec } => from_result(id, device.write_raw(&spec)),
        Operation::WriteGuestDay { spec } => from_result(id, device.write_guest_day(&spec)),
        Operation::WriteGuestHour { spec } => from_result(id, device.write_guest_hour(&spec)),
        Operation::WriteMaster { spec } => from_result(id, device.write_master(&spec)),
        Operation::WriteInstall { spec } => from_result(id, device.write_install(&spec)),
        Operation::WriteReset { spec } => from_result(id, device.write_reset(&spec)),
        Operation::WriteClear { spec } => from_result(id, device.write_clear(&spec)),
        Operation::WriteCheckoutHour { spec } => from_result(id, device.write_checkout_hour(&spec)),
        Operation::WriteTimesync { spec } => from_result(id, device.write_timesync(&spec)),
        Operation::ProbePorts => from_result(id, device.probe_ports()),
        Operation::SetPort { port } => from_result(id, device.set_port(port)),
        Operation::Selftest => from_result(id, device.selftest()),
        Operation::DeviceInfo => from_result(id, device.device_info()),
        Operation::Shutdown => from_result(id, Ok(Value::Null)),
    }
}

/// 把设备结果转成响应行；序列化失败按协议错误上报。
fn from_result<T: Serialize>(id: RequestId, result: DeviceResult<T>) -> Response {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(json) => Response::success(id, json),
            Err(e) => Response::failure(
                id,
                DeviceError::Protocol(format!("结果无法序列化：{e}")).payload(),
            ),
        },
        Err(e) => Response::failure(id, e.payload()),
    }
}

/// 写一行响应。返回 `false` 表示管道已断，应当退出。
fn write_line(stdout: &mut impl Write, resp: &Response) -> bool {
    match resp.to_line() {
        Ok(line) => writeln!(stdout, "{line}").is_ok() && stdout.flush().is_ok(),
        Err(e) => {
            eprintln!("[sidecar] 响应无法序列化：{e}");
            true
        }
    }
}

/// 驱动目录：`--driver-dir` > `RC_DRIVER_DIR` > exe 同目录 `drivers/LC2000`。
fn resolve_driver_dir() -> PathBuf {
    if let Some(dir) = arg_value("--driver-dir") {
        return dir;
    }
    if let Ok(dir) = std::env::var("RC_DRIVER_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    exe_dir().join("drivers").join("LC2000")
}

/// 当前 exe 所在目录；取不到时退回当前工作目录。
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 取 `--name value` 或 `--name=value` 形式的参数。
fn arg_value(name: &str) -> Option<PathBuf> {
    arg_text(name).map(PathBuf::from)
}

/// 取 `--name value` 或 `--name=value` 形式的**文本**参数。
fn arg_text(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 开关型参数：只看有没有出现，不看取值。
fn flag_present(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == name)
}
