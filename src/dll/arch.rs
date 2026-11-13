//! 驱动架构策略：为将来切换到 64 位驱动预留的**显式开关**。
//!
//! # 现状
//!
//! 当前驱动 `LC32RFRW.dll` **只有 x86 版本**（102,492 字节，92 个导出，
//! SHA-256 `f2f483e0…bccbc0`）。所以整个程序必须按 **i686** 目标构建。
//!
//! # 为什么不现在就做 64 位
//!
//! 不是技术惰性，而是**没有 64 位驱动可用**。在拿到 64 位驱动之前，
//! 任何「提前适配」都是空转：没有东西可以验证。
//!
//! # 什么情况下才该切换
//!
//! 只有**硬性限制**出现时才启动换驱动，具体是这两条之一：
//!
//! 1. Windows 不再支持 32 位程序（微软已宣布逐步收紧，但 32 位子系统仍在）；
//! 2. 现有 32 位 DLL 彻底无法使用（厂商停止供货、驱动签名策略变更等）。
//!
//! **不构成理由的情况**：
//!
//! - 「64 位看起来更现代」——性能不是瓶颈，串口波特率固定 9600；
//! - 「构建链想统一到 x64」——为构建方便而换驱动是本末倒置；
//! - 「别的项目都用 64 位」——与本项目的硬件约束无关。
//!
//! # 切换时需要改什么
//!
//! 本模块把「当前要求哪种架构」收敛成 `EXPECTED_DRIVER_ARCH` 一个常量。
//! 换驱动时只需：
//!
//! 1. 把新驱动放到 `drivers/LC2000/`，更新 `KNOWN_DRIVER_SHA256`；
//! 2. 把 [`EXPECTED_DRIVER_ARCH`] 改成 `DriverArch::X86_64`；
//! 3. 构建目标从 `i686-pc-windows-msvc` 改成 `x86_64-pc-windows-msvc`；
//! 4. 重跑 46 项自检 + 真机读卡/写卡复测。
//!
//! **业务层与设备调用代码一行都不用改**——`CardDevice` trait 与
//! [`crate::dll::DirectDllDevice`] 的接口对架构无感知，架构检查只在
//! [`crate::dll::pe`] 这一层做，且是参数化的。
//!
//! # 前提：导出签名一致
//!
//! 换驱动的前提是**导出的 10 个符号签名不变**（stdcall、int 参数、HEX 文本缓冲区）。
//! 若新驱动改了签名或加了新的初始化步骤，那就不只是「换架构」，
//! 而是协议实现要重做，届时应新开一个 td 条目而不是改这里的常量。

use crate::error::DriverArch;

/// 当前驱动要求的架构。
///
/// 改这个常量 + 换 DLL + 换构建目标，就是完整的架构切换流程。
pub const EXPECTED_DRIVER_ARCH: DriverArch = DriverArch::X86;

/// 已验证驱动的 SHA-256（小写十六进制）。
///
/// **卡格式结论只对这个版本成立。** 换驱动后必须重新跑真机读写验证，
/// 并把新哈希更新到这里。
pub const KNOWN_DRIVER_SHA256: &str = "f2f483e08ba8048ffd1c57e66cb35d9e32c1c1e280dc5f2ff100a5d881bccbc0";

/// 已验证驱动的字节数。
pub const KNOWN_DRIVER_BYTES: u64 = 102_492;

/// 已验证驱动的导出符号数。
pub const KNOWN_DRIVER_EXPORTS: usize = 92;

/// 当前构建目标是 32 位还是 64 位。
#[must_use]
pub fn host_is_32bit() -> bool {
    cfg!(target_arch = "x86")
}

/// 当前构建目标能否加载 [`EXPECTED_DRIVER_ARCH`] 的驱动。
///
/// 返回 `false` 说明构建目标选错了——这是**构建配置问题**，不是运行期故障，
/// 应在 CI / 构建脚本里就拦住。
#[must_use]
pub fn build_target_matches_driver() -> bool {
    crate::dll::pe::host_arch() == EXPECTED_DRIVER_ARCH
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn real_driver() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("drivers")
            .join("LC2000")
            .join("LC32RFRW.dll")
    }

    /// 声明的期望架构必须与驱动实际架构一致。
    ///
    /// 换了驱动却忘了改常量，这条会立刻失败。
    #[test]
    fn expected_arch_matches_real_driver() {
        let path = real_driver();
        assert!(path.is_file(), "驱动应随仓库分发：{}", path.display());
        assert_eq!(
            crate::dll::pe::pe_arch(&path),
            EXPECTED_DRIVER_ARCH,
            "EXPECTED_DRIVER_ARCH 与真实驱动的架构不符"
        );
    }

    /// 记录的哈希必须与真实文件一致。
    #[test]
    fn known_hash_matches_real_driver() {
        let bytes = std::fs::read(real_driver()).expect("读驱动文件");
        assert_eq!(
            sha256_lower(&bytes),
            KNOWN_DRIVER_SHA256,
            "已知驱动指纹与文件不符"
        );
    }

    /// 记录的字节数必须与真实文件一致。
    #[test]
    fn known_size_matches_real_driver() {
        let meta = std::fs::metadata(real_driver()).expect("读驱动元数据");
        assert_eq!(meta.len(), KNOWN_DRIVER_BYTES);
    }

    /// 记录的导出数必须与真实文件一致。
    #[test]
    fn known_export_count_matches_real_driver() {
        let bytes = std::fs::read(real_driver()).expect("读驱动文件");
        let names = crate::dll::pe::exported_names(&bytes);
        assert_eq!(names.len(), KNOWN_DRIVER_EXPORTS, "导出数量不符");
    }

    /// 32 位构建目标与当前期望架构一致。
    ///
    /// 用 `x86_64` 目标构建时这条会失败——那正是提醒你「构建目标选错了」，
    /// 应该在 CI 里用 `--target i686-pc-windows-msvc` 跑。
    #[test]
    fn build_target_matches_current_driver() {
        if cfg!(target_arch = "x86") {
            assert!(host_is_32bit());
            assert!(build_target_matches_driver(), "32 位目标应匹配 x86 驱动");
        } else {
            // 64 位目标只用于快速 cargo check，不应视为可发布构建
            assert!(!build_target_matches_driver(), "64 位目标与 x86 驱动不匹配");
        }
    }

    /// 本地 SHA-256 实现（测试用）。
    ///
    /// 复用 `device.rs` 的实现而不是重写一份：如果两份实现同时写错，
    /// 就会互相印证成"正确"；共用一份 + 另一条用官方向量验证的测试，
    /// 才能保证「记录的哈希」可信。
    fn sha256_lower(data: &[u8]) -> String {
        crate::dll::device::sha256_hex_for_test(data)
    }
}
