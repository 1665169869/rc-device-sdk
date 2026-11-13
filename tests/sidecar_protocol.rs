//! sidecar 端到端测试：起真进程，走 NDJSON。
//!
//! 这些测试**不碰硬件**：`selftest` 是纯离线的 46 项，`device_info` 只读驱动
//! 文件信息。它们验证的是「协议能通、进程能起、错误能回」，不是驱动能不能加载。
//!
//! 驱动目录通过 `RC_DRIVER_DIR` 显式传入，避免测试依赖 exe 的落盘位置。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

use rc_device::sidecar::{Operation, Request, Response};
use rc_device::{CardKind, CardSnapshot, DeviceInfo, RawSpec, SelfTestReport};

fn driver_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("drivers")
        .join("LC2000")
}

struct Sidecar {
    child: Child,
    reader: BufReader<ChildStdout>,
}

impl Sidecar {
    fn start() -> Self {
        Self::spawn(&[])
    }

    /// 带 `--sim` 启动：后端换成内存模拟，不加载驱动。
    ///
    /// 写卡路径因此能在**任何架构、无读卡器**的机器上跨进程跑通，
    /// 这就是「模拟由硬件层负责」的验证点。
    fn start_sim() -> Self {
        Self::spawn(&["--sim"])
    }

    fn spawn(extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rc-device-sidecar"))
            .env("RC_DRIVER_DIR", driver_dir())
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sidecar 应能启动");

        let stdout = child.stdout.take().expect("stdout 应可用");
        Self {
            child,
            reader: BufReader::new(stdout),
        }
    }

    /// 发一条请求，读一行响应。空行会被跳过（sidecar 只会写响应行）。
    fn call(&mut self, id: u64, op: Operation) -> Response {
        let line = Request::new(id, op).to_line().expect("请求应能编码");
        self.send_raw(&line)
    }

    /// 直接写一行文本，用来测协议错误路径。
    fn send_raw(&mut self, line: &str) -> Response {
        {
            let stdin = self.child.stdin.as_mut().expect("stdin 应可用");
            writeln!(stdin, "{line}").expect("写 stdin 应成功");
            stdin.flush().expect("flush 应成功");
        }

        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self.reader.read_line(&mut buf).expect("读响应应成功");
            assert!(n > 0, "sidecar 意外关闭了 stdout");
            let trimmed = buf.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            return Response::from_line(trimmed).expect("响应应能解析");
        }
    }

    fn shutdown(mut self) {
        let resp = self.call(9999, Operation::Shutdown);
        assert!(resp.ok, "shutdown 应成功");
        let status = self.child.wait().expect("应能回收 sidecar");
        assert!(status.success(), "sidecar 应以 0 退出：{status}");
    }
}

/// 自检必须能跨进程跑通，并且 46 项全绿。
#[test]
fn selftest_is_green_over_ndjson() {
    let mut sidecar = Sidecar::start();
    let resp = sidecar.call(1, Operation::Selftest);
    assert!(resp.ok, "selftest 不该失败：{:?}", resp.error);

    let report = resp.into_result::<SelfTestReport>().expect("结果应是自检报告");
    assert!(report.is_all_green(), "{} 项失败", report.failed);
    assert_eq!(report.total(), 46, "自检项数必须是 46");

    sidecar.shutdown();
}

/// `device_info` 不打开设备也能返回，且要带上真实的驱动路径。
#[test]
fn device_info_reports_driver_path() {
    let mut sidecar = Sidecar::start();
    let resp = sidecar.call(2, Operation::DeviceInfo);
    assert!(resp.ok, "device_info 不该失败：{:?}", resp.error);

    let info = resp.into_result::<DeviceInfo>().expect("结果应是设备信息");
    assert!(
        info.driver_path.ends_with("LC32RFRW.dll"),
        "驱动路径不对：{}",
        info.driver_path
    );
    assert!(!info.opened, "没 open 过就不该是已打开状态");

    sidecar.shutdown();
}

/// 非法请求必须回一条带 `protocol` 分类的错误，而不是让进程静默退出。
#[test]
fn malformed_request_returns_protocol_error() {
    let mut sidecar = Sidecar::start();
    let resp = sidecar.send_raw(r#"{"id":3,"op":"format_disk"}"#);
    assert!(!resp.ok, "未知 op 必须失败");
    let err = resp.into_result::<()>().unwrap_err();
    assert_eq!(err.kind, "protocol");

    sidecar.shutdown();
}

/// 未打开设备就读卡：走真实错误路径，验证错误码与提示能到对端。
///
/// 不依赖硬件——`read_card` 在未打开时直接返回 `NotOpen`（SDK 码 3），
/// 只有真机测试才会去碰串口。
#[test]
fn read_card_before_open_reports_error_across_process() {
    let mut sidecar = Sidecar::start();
    let resp = sidecar.call(4, Operation::ReadCard);
    assert!(!resp.ok, "未打开就读卡必须失败");

    let err = resp.into_result::<rc_device::CardSnapshot>().unwrap_err();
    if cfg!(target_arch = "x86") {
        assert_eq!(err.kind, "not_open");
        assert_eq!(err.sdk_code, 3);
    } else {
        // 64 位 sidecar 在加载驱动那一步就先撞上架构不符——
        // 这正是“为什么需要 32 位 sidecar”的现场证据。
        assert_eq!(err.kind, "driver_arch_mismatch");
        assert_eq!(err.sdk_code, 2);
    }
    assert!(err.device_fault);

    sidecar.shutdown();
}

/// `--sim` 下的完整写卡路径：`write_raw` 写进去的 8 字节就是读回来的那张卡。
///
/// 这条覆盖三件事：模拟后端能跨进程启动、`write_raw` 在线上协议里形状正确、
/// 写进去的字节不会被任何一层顺手「修正」。
#[test]
fn write_raw_round_trips_over_ndjson_in_sim() {
    let mut sidecar = Sidecar::start_sim();

    let spec = RawSpec {
        p1: [0x5B, 0xEC, 0xF0, 0x00],
        p2: [0x81, 0x04, 0x24, 0xAB],
    };
    let resp = sidecar.call(5, Operation::WriteRaw { spec });
    assert!(resp.ok, "write_raw 不该失败：{:?}", resp.error);
    let written = resp
        .into_result::<CardSnapshot>()
        .expect("结果应是卡快照");
    assert_eq!(written.kind, CardKind::GuestDay);
    assert_eq!(written.room_index, Some(15));
    assert_eq!(
        &written.raw[0..8],
        &[0x5B, 0xEC, 0xF0, 0x00, 0x81, 0x04, 0x24, 0xAB]
    );

    // 回读必须与写入一致：模拟后端写的是内存卡槽，不存在「驱动返回旧值」
    let back = sidecar
        .call(6, Operation::ReadCard)
        .into_result::<CardSnapshot>()
        .expect("读卡应成功");
    assert_eq!(back.raw, written.raw);

    sidecar.shutdown();
}

/// 模拟会话里没放卡时，读卡必须报 `no_card`，而不是回一张空白卡。
#[test]
fn sim_without_card_reports_no_card() {
    let mut sidecar = Sidecar::spawn(&["--sim", "--sim-card", "none"]);
    let resp = sidecar.call(7, Operation::ReadCard);
    assert!(!resp.ok, "没放卡就该失败");
    let err = resp.into_result::<CardSnapshot>().unwrap_err();
    assert_eq!(err.kind, "no_card");
    assert_eq!(err.sdk_code, 5);

    sidecar.shutdown();
}
