//! `CardDevice` 的生产实现：直接动态加载 `LC32RFRW.dll`。
//!
//! # 打开序列（严格照抄原 C# `LockCardReader.Open`）
//!
//! ```text
//! mif_selecom(port, 9600) != 0        → PortOpenFailed
//! sleep 150ms
//! mif_shake() != 0                    → config_parameter(0); sleep 80ms; mif_shake()
//! config_parameter(0)                 ← 关键：明文模式
//! sleep 60ms
//! mif_shake()
//! mif_selecard(5) != 0                → NoCard
//! get_id_asc(buf)                     ← 失败不致命
//! ```
//!
//! 每个 `sleep` 都是实测必需的：驱动内部状态机需要时间稳定，
//! 去掉任何一个都会在现场表现为「偶发握手失败」。
//!
//! # 会话与并发
//!
//! 串口是**独占资源**，同时只能有一个实例在用。本实现用 `Mutex` 串行化全部驱动调用，
//! 保证不会有两个写卡动作交错。
//!
//! # 为什么写卡不自动重试
//!
//! 写卡是硬件动作。重试可能把同一张卡写两遍、把卡号推进两次，
//! 比失败本身更难处理。失败就返回错误，让前台人工确认。
//!
//! **回读校验的 3 次重试是协议的一部分**（见 [`VERIFY_ATTEMPTS`]），
//! 因为写完立刻读可能拿到旧值——那是实测结论，不是"容错"。

/// 打开串口后等待驱动内部状态稳定的时间。
///
/// 【实测】原 C# `LockCardReader.Open` 里是 `Thread.Sleep(150)`。
pub const OPEN_SETTLE_MS: u64 = 150;

/// 握手失败后重新配置、再握手之间的等待。
///
/// 【实测】原 C# 为 `Thread.Sleep(80)`。
pub const RESHAKE_DELAY_MS: u64 = 80;

/// 设置明文模式后的等待。
///
/// 【实测】原 C# 为 `Thread.Sleep(60)`。
pub const CONFIG_SETTLE_MS: u64 = 60;

/// 设置写密码后的等待。【实测】原 C# `WriteAndVerify` 里为 60ms。
pub const SETUP_PWM_DELAY_MS: u64 = 60;

/// 写完页1 后的等待。【实测】原 C# 为 80ms。
pub const WRITE_PAGE1_DELAY_MS: u64 = 80;

/// 写完页2 后的等待（要等卡内部提交）。【实测】原 C# 为 150ms。
pub const WRITE_PAGE2_DELAY_MS: u64 = 150;

/// 回读校验失败后的重试间隔。【实测】原 C# 为 200ms。
pub const VERIFY_RETRY_DELAY_MS: u64 = 200;

/// 页2 的等待必须比页1 长（要等卡内部提交）——编译期保证。
const _: () = assert!(WRITE_PAGE2_DELAY_MS > WRITE_PAGE1_DELAY_MS);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 各段延时必须与实测值一致——改动它们会让现场出现偶发握手失败。
    #[test]
    fn delays_match_measured_values() {
        assert_eq!(OPEN_SETTLE_MS, 150);
        assert_eq!(RESHAKE_DELAY_MS, 80);
        assert_eq!(CONFIG_SETTLE_MS, 60);
        assert_eq!(SETUP_PWM_DELAY_MS, 60);
        assert_eq!(WRITE_PAGE1_DELAY_MS, 80);
        assert_eq!(WRITE_PAGE2_DELAY_MS, 150);
        assert_eq!(VERIFY_RETRY_DELAY_MS, 200);
    }
}
