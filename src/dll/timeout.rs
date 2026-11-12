//! 驱动调用的超时兜底。
//!
//! # 为什么需要它
//!
//! `LC32RFRW.dll` 的导出全部是**同步阻塞**调用：进去之后只有驱动自己返回才轮到我们。
//! 读卡器掉电、串口被别家软件抢走、USB 转串口芯片固件卡死时，驱动可能长时间不返回。
//!
//! 设备层用 `Mutex` 串行化全部驱动调用（串口是独占资源），所以**一个线程卡在驱动里，
//! 后续所有调用都会排队等**——前台界面就此永久转圈，用户只能强杀进程。
//!
//! # 做法
//!
//! ```text
//! 主线程 ──spawn──▶ 工作线程 ──▶ dll.xxx(..)   ← 可能永久阻塞
//!    │                    │
//!    └──recv_timeout(T)───┘
//!        超时 → 返回 DeviceError::Timeout(T)
//! ```
//!
//! 工作线程**无法被杀死**（Rust 没有安全的手段）。所以超时之后那个线程仍然卡在驱动里，
//! 我们只是不再等它。调用方必须据此把会话标记为不可信——见 [`crate::dll::device`] 的
//! `untrusted` 处理。
//!
//! # 超时 ≠ 可以重试
//!
//! 超时只说明「我们不知道驱动做了什么」。写卡时尤其危险：卡可能已经写了一半，
//! 此时重写会让同一张卡被写两遍、卡号凭空推进。**超时后绝不自动重试**。

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::error::{DeviceError, DeviceResult};

/// 在独立线程里执行一次驱动调用，超过 `timeout_ms` 就放弃等待。
///
/// # 参数
///
/// - `what`：动作名，进错误文案。要写成现场能看懂的话（`"读卡"`、`"写页1"`），
///   不要写成函数名。
/// - `timeout_ms`：超时上限，取自 [`crate::dll::timing`] 的常量。
/// - `call`：真正碰驱动的闭包。**必须是 `'static`**：它会被移到工作线程里执行，
///   所以捕获的东西要么是 `Arc`，要么是值拷贝，不能是借用。
///
/// # 错误
///
/// - 闭包自身返回的错误原样传出。
/// - 超时 → [`DeviceError::Timeout`]，参数是**秒**（向上取整，至少 1 秒）。
/// - 工作线程 panic → [`DeviceError::Io`]。
///
/// # 关于 `Timeout` 的单位
///
/// [`DeviceError::Timeout`] 的展示文案是「设备操作超时（N 秒）」，所以这里传秒。
/// 传毫秒会显示出「超时（10000 秒）」这种明显错误的文案。
pub fn run_with_timeout<F, T>(what: &str, timeout_ms: u64, call: F) -> DeviceResult<T>
where
    F: FnOnce() -> DeviceResult<T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel();

    // 工作线程可能永久卡在驱动里。这里不保存 JoinHandle：
    // 即便保存了也无法安全终止它，留着反而暗示"可以回收"。
    thread::spawn(move || {
        // 发送失败说明主线程已经因超时放弃，结果无人接收——直接丢弃即可。
        let _ = tx.send(call());
    });

    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(DeviceError::Timeout(elapsed_secs_ceil(timeout_ms))),
        // 通道断开但没超时：工作线程在闭包里 panic 了
        Err(RecvTimeoutError::Disconnected) => Err(DeviceError::Io(format!(
            "{what}时工作线程异常退出（驱动调用可能崩溃）"
        ))),
    }
}

/// 毫秒向上取整成秒，至少为 1。
///
/// `DeviceError::Timeout` 的文案按秒展示，传 0 会显示「超时（0 秒）」，读起来像没超时。
fn elapsed_secs_ceil(timeout_ms: u64) -> u64 {
    timeout_ms.div_ceil(1000).max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 正常返回：结果原样传出，不该被超时机制干扰。
    #[test]
    fn fast_call_returns_value() {
        let got = run_with_timeout("读卡", 5_000, || Ok(42_u32)).expect("应成功");
        assert_eq!(got, 42);
    }

    /// 闭包自身的错误要原样传出，不能被包装成超时。
    #[test]
    fn inner_error_passes_through() {
        let err = run_with_timeout("读卡", 5_000, || {
            Err::<u32, _>(DeviceError::NoCard)
        })
        .expect_err("应失败");
        assert_eq!(err, DeviceError::NoCard);
    }

    /// **核心场景**：永不返回的调用必须在超时后收手。
    ///
    /// 用 `thread::sleep` 模拟卡死在驱动里的线程：真实驱动不会返回，
    /// 我们也无法杀死它，唯一能做的就是不再等。
    #[test]
    fn never_returning_call_times_out() {
        let started = Instant::now();
        let err = run_with_timeout("读卡", 100, || {
            // 远长于超时，模拟驱动卡死
            thread::sleep(Duration::from_secs(5));
            Ok(1_u32)
        })
        .expect_err("永不返回必须超时");

        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(2), "应在超时附近返回，实际 {elapsed:?}");
        match err {
            DeviceError::Timeout(secs) => assert_eq!(secs, 1, "100ms 向上取整为 1 秒"),
            other => panic!("应为 Timeout，实际 {other:?}"),
        }
    }

    /// 超时后主线程必须能继续工作——工作线程仍卡着，但不影响主线程。
    ///
    /// 这条证明「放弃等待」确实生效，而不是把主线程一起拖住。
    #[test]
    fn main_thread_keeps_running_after_timeout() {
        let err = run_with_timeout("写卡", 80, || {
            thread::sleep(Duration::from_secs(5));
            Ok(())
        })
        .expect_err("应超时");
        assert!(matches!(err, DeviceError::Timeout(_)));

        // 主线程立刻能做别的事
        let after = run_with_timeout("读卡", 5_000, || Ok(7_u32)).expect("后续调用应正常");
        assert_eq!(after, 7);
    }

    /// 工作线程 panic 不能把主线程一起带崩，也不能伪装成超时。
    #[test]
    fn panicking_worker_reports_io_error() {
        // 显式标注返回类型：`panic!` 让 `T` 无法推断，而 `expect_err` 要求 `T: Debug`
        let err = run_with_timeout("写卡", 5_000, || -> DeviceResult<()> {
            panic!("模拟驱动崩溃");
        })
        .expect_err("应报错");

        match err {
            DeviceError::Io(msg) => assert!(msg.contains("工作线程"), "提示应说明原因：{msg}"),
            other => panic!("应为 Io，实际 {other:?}"),
        }
    }

    /// `Timeout` 的参数是秒而不是毫秒——文案按秒展示。
    #[test]
    fn timeout_uses_seconds_for_display() {
        assert_eq!(elapsed_secs_ceil(1), 1);
        assert_eq!(elapsed_secs_ceil(999), 1);
        assert_eq!(elapsed_secs_ceil(1_000), 1);
        assert_eq!(elapsed_secs_ceil(1_001), 2);
        assert_eq!(elapsed_secs_ceil(15_000), 15);
        // 0 会显示成「超时（0 秒）」，读起来像没超时
        assert_eq!(elapsed_secs_ceil(0), 1);
    }
}
