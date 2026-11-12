//! PE 文件头解析：判断 DLL 的架构，避免用 `LoadLibrary` 去撞一个含糊的 193 错误。
//!
//! # 为什么需要提前判断
//!
//! `LC32RFRW.dll` **只有 x86 版本**。在 64 位宿主里 `LoadLibrary` 它会失败，
//! 系统错误码是 `ERROR_BAD_EXE_FORMAT (193)` —— 这个错误既不说明是位数问题，
//! 也不提示该怎么修。现场只会看到「设备不可用」，排查方向完全靠猜。
//!
//! 所以先读 PE 头，明确报出 [`DriverArchMismatch`](crate::DeviceError::DriverArchMismatch)，
//! 附带 DLL 与宿主各自的架构。
//!
//! 本模块是**纯函数、无 unsafe、无系统调用**，可以对着仓库里的真实 DLL 做离线测试。

use std::fs;
use std::path::Path;

use crate::error::{DeviceError, DeviceResult, DriverArch};

/// DOS 头里 `e_lfanew` 的偏移量（指向 PE 头）。
const DOS_E_LFANEW_OFFSET: usize = 0x3C;

/// PE 签名长度（`PE\0\0`）。
const PE_SIGNATURE: [u8; 4] = [b'P', b'E', 0, 0];

/// `Machine` 字段相对 PE 头起点的偏移。
const PE_MACHINE_OFFSET: usize = 4;

/// `Machine` 取值：32 位 x86。
const MACHINE_I386: u16 = 0x014C;

/// `Machine` 取值：64 位 x86。
const MACHINE_AMD64: u16 = 0x8664;

/// 读 PE 头所需的最小文件长度（DOS 头 + `e_lfanew` 字段）。
const MIN_PE_HEADER_LEN: usize = 64;

/// 当前宿主进程的架构。
///
/// 与 [`pe_arch`] 的返回值比较即可判断能否加载目标 DLL。
#[must_use]
pub fn host_arch() -> DriverArch {
    #[cfg(target_arch = "x86")]
    {
        DriverArch::X86
    }
    #[cfg(target_arch = "x86_64")]
    {
        DriverArch::X86_64
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        DriverArch::Unknown
    }
}

/// 读一个文件的 PE 架构。
///
/// 文件不存在、读不出来、或不是合法 PE 时返回 [`DriverArch::Unknown`]——
/// 本函数只负责「尽量判断」，不负责报错；调用方按 [`check_loadable`] 的规则处理。
#[must_use]
pub fn pe_arch(path: &Path) -> DriverArch {
    let Ok(bytes) = fs::read(path) else {
        return DriverArch::Unknown;
    };
    arch_of_bytes(&bytes)
}

/// 从字节流解析 PE 架构（纯函数，便于测试）。
#[must_use]
pub fn arch_of_bytes(bytes: &[u8]) -> DriverArch {
    if bytes.len() < MIN_PE_HEADER_LEN || bytes[0] != b'M' || bytes[1] != b'Z' {
        return DriverArch::Unknown;
    }

    // e_lfanew：DOS 头里记录 PE 头偏移的 4 字节小端整数
    let raw = [
        bytes[DOS_E_LFANEW_OFFSET],
        bytes[DOS_E_LFANEW_OFFSET + 1],
        bytes[DOS_E_LFANEW_OFFSET + 2],
        bytes[DOS_E_LFANEW_OFFSET + 3],
    ];
    let pe_off = u32::from_le_bytes(raw) as usize;

    // PE 头至少要能容纳签名 + Machine 两个字段
    if pe_off > usize::MAX - PE_MACHINE_OFFSET - 2 {
        return DriverArch::Unknown;
    }
    if bytes.len() < pe_off + PE_MACHINE_OFFSET + 2 {
        return DriverArch::Unknown;
    }
    if bytes[pe_off..pe_off + 4] != PE_SIGNATURE {
        return DriverArch::Unknown;
    }

    let m = [
        bytes[pe_off + PE_MACHINE_OFFSET],
        bytes[pe_off + PE_MACHINE_OFFSET + 1],
    ];
    match u16::from_le_bytes(m) {
        MACHINE_I386 => DriverArch::X86,
        MACHINE_AMD64 => DriverArch::X86_64,
        _ => DriverArch::Unknown,
    }
}

/// 判断一个 DLL 能否在当前宿主里加载。
///
/// # 检查顺序（先便宜后昂贵）
///
/// 1. 文件是否存在 → [`DeviceError::DriverFileMissing`]
/// 2. PE 架构是否与宿主一致 → [`DeviceError::DriverArchMismatch`]
///
/// 架构未知（不是 PE、或读不出来）时**放行**：交给 `LoadLibrary` 去报真实错误，
/// 总比在这里武断地拒绝更接近事实。
///
/// # Errors
///
/// 见上面的两个变体。
pub fn check_loadable(path: &Path) -> DeviceResult<DriverArch> {
    if !path.is_file() {
        return Err(DeviceError::DriverFileMissing(path.display().to_string()));
    }
    let dll = pe_arch(path);
    let host = host_arch();
    match dll {
        DriverArch::Unknown => Ok(dll),
        same if same == host => Ok(dll),
        _ => Err(DeviceError::DriverArchMismatch { dll, host }),
    }
}

/// 按**期望架构**检查驱动是否可加载（构建期约束检查）。
///
/// 与 [`check_loadable`] 的区别：后者比的是「DLL 与当前宿主」，
/// 本函数比的是「DLL 与 [`crate::dll::arch::EXPECTED_DRIVER_ARCH`]」。
///
/// 用途：构建脚本与 CI 在打包前确认「这个 DLL 是不是我们支持的那一代」，
/// 而不是等到用户机器上 `LoadLibrary` 失败。
///
/// # Errors
///
/// 文件不存在或架构与期望不符时返回对应错误。
pub fn check_expected_arch(path: &Path) -> DeviceResult<DriverArch> {
    if !path.is_file() {
        return Err(DeviceError::DriverFileMissing(path.display().to_string()));
    }
    let dll = pe_arch(path);
    let expected = crate::dll::arch::EXPECTED_DRIVER_ARCH;
    if dll == DriverArch::Unknown || dll == expected {
        Ok(dll)
    } else {
        Err(DeviceError::DriverArchMismatch {
            dll,
            host: expected,
        })
    }
}

/// 导出表在数据目录中的索引。
const EXPORT_DIR_INDEX: usize = 0;

/// 具名导出数的合理上限，超过即认为 PE 结构损坏。
const MAX_REASONABLE_EXPORTS: usize = 65_536;

/// 段表项长度。
const SECTION_HEADER_LEN: usize = 40;

/// 取 PE 头偏移；不是合法 PE 时返回 `None`。
fn pe_header_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < MIN_PE_HEADER_LEN || bytes[0] != b'M' || bytes[1] != b'Z' {
        return None;
    }
    let pe_off = read_u32(bytes, DOS_E_LFANEW_OFFSET) as usize;
    if bytes.len() < pe_off + 4 || bytes[pe_off..pe_off + 4] != PE_SIGNATURE {
        return None;
    }
    Some(pe_off)
}

/// 读 PE 可选头里的第 `index` 个数据目录项，返回 `(RVA, Size)`。
fn data_directory(bytes: &[u8], pe_off: usize, index: usize) -> Option<(u32, u32)> {
    let opt_off = pe_off + 24; // PE 签名 4 + COFF 头 20
    if bytes.len() < opt_off + 2 {
        return None;
    }
    // 可选头魔数：0x20B = PE32+，否则按 PE32 处理
    let magic = u16::from_le_bytes([bytes[opt_off], bytes[opt_off + 1]]);
    let (num_rva_off, dir_off) = if magic == 0x020B {
        (opt_off + 0x6C, opt_off + 0x70)
    } else {
        (opt_off + 0x5C, opt_off + 0x60)
    };
    let num_rva = read_u32(bytes, num_rva_off) as usize;
    if index >= num_rva {
        return None;
    }
    let entry = dir_off + index * 8;
    if bytes.len() < entry + 8 {
        return None;
    }
    Some((read_u32(bytes, entry), read_u32(bytes, entry + 4)))
}

/// RVA → 文件偏移。按段表逐项匹配。
fn rva_to_offset(bytes: &[u8], pe_off: usize, rva: u32) -> Option<usize> {
    if rva == 0 {
        return None;
    }
    let num_sections = u16::from_le_bytes([bytes[pe_off + 6], bytes[pe_off + 7]]) as usize;
    let size_opt = u16::from_le_bytes([bytes[pe_off + 20], bytes[pe_off + 21]]) as usize;
    let sec_off = pe_off + 24 + size_opt;

    for i in 0..num_sections {
        let s = sec_off + i * SECTION_HEADER_LEN;
        if bytes.len() < s + 40 {
            return None;
        }
        // 段头偏移：+8 VirtualSize、+12 VirtualAddress、+16 SizeOfRawData、+20 PointerToRawData
        let vsize = read_u32(bytes, s + 8);
        let vaddr = read_u32(bytes, s + 12);
        let rawsize = read_u32(bytes, s + 16);
        let rawptr = read_u32(bytes, s + 20);

        let span = vsize.max(rawsize);
        if rva >= vaddr && rva < vaddr.saturating_add(span) {
            let delta = rva - vaddr;
            let off = (rawptr as usize).checked_add(delta as usize)?;
            return (off <= bytes.len()).then_some(off);
        }
    }
    None
}

/// 读 4 字节小端；越界返回 0（调用方另有长度检查）。
fn read_u32(bytes: &[u8], off: usize) -> u32 {
    if bytes.len() < off + 4 {
        return 0;
    }
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// 解析 PE 导出表，返回全部具名导出符号。
///
/// # 为什么需要它
///
/// 开发机通常是 64 位，`LoadLibrary` 一个 32 位 DLL 必然失败——但那失败是**位数问题**，
/// 不是「缺符号」。想在没有 32 位宿主的情况下确认「驱动版本是否兼容」，
/// 只能直接读 PE 导出表。本函数纯字节解析，不需要加载 DLL。
///
/// 解析失败（不是 PE、没有导出表、结构损坏）一律返回空 `Vec`，不报错——
/// 调用方据此判断「无法验证」，而不是「验证失败」。
#[must_use]
pub fn exported_names(bytes: &[u8]) -> Vec<String> {
    let Some(pe_off) = pe_header_offset(bytes) else {
        return Vec::new();
    };
    let Some(export_dir) = data_directory(bytes, pe_off, EXPORT_DIR_INDEX) else {
        return Vec::new();
    };
    if export_dir.0 == 0 {
        return Vec::new();
    }
    let Some(exp_off) = rva_to_offset(bytes, pe_off, export_dir.0) else {
        return Vec::new();
    };

    // IMAGE_EXPORT_DIRECTORY 布局（均为 4 字节 LE）：
    //   0: Characteristics  4: TimeDateStamp  8: Major/Minor  12: Name RVA
    //  16: Base(OrdinalBase) 20: NumberOfFunctions 24: NumberOfNames
    //  28: AddressOfFunctions 32: AddressOfNames 36: AddressOfNameOrdinals
    if bytes.len() < exp_off + 40 {
        return Vec::new();
    }
    let num_names = read_u32(bytes, exp_off + 24) as usize;
    let addr_names = read_u32(bytes, exp_off + 32);

    // 防御：具名导出数异常大时说明结构已损坏，不要尝试解析
    if num_names == 0 || num_names > MAX_REASONABLE_EXPORTS {
        return Vec::new();
    }
    let Some(names_off) = rva_to_offset(bytes, pe_off, addr_names) else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(num_names);
    for i in 0..num_names {
        let entry = names_off + i * 4;
        if bytes.len() < entry + 4 {
            break;
        }
        let name_rva = read_u32(bytes, entry);
        let Some(name_file_off) = rva_to_offset(bytes, pe_off, name_rva) else {
            continue;
        };
        if let Some(nul) = bytes[name_file_off..].iter().position(|b| *b == 0) {
            let raw = &bytes[name_file_off..name_file_off + nul];
            out.push(String::from_utf8_lossy(raw).into_owned());
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    /// 仓库里随包分发的驱动路径。
    fn real_driver() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("drivers")
            .join("LC2000")
            .join("LC32RFRW.dll")
    }

    /// 造一段最小的合法 PE32 头：DOS 头 + `e_lfanew` 指向 PE 头。
    fn fake_pe(machine: u16) -> Vec<u8> {
        let pe_off: usize = 0x80;
        let mut b = vec![0u8; pe_off + 8];
        b[0] = b'M';
        b[1] = b'Z';
        b[DOS_E_LFANEW_OFFSET..DOS_E_LFANEW_OFFSET + 4]
            .copy_from_slice(&(pe_off as u32).to_le_bytes());
        b[pe_off..pe_off + 4].copy_from_slice(&PE_SIGNATURE);
        b[pe_off + PE_MACHINE_OFFSET..pe_off + PE_MACHINE_OFFSET + 2]
            .copy_from_slice(&machine.to_le_bytes());
        b
    }

    /// 真实的 `LC32RFRW.dll` 必须被识别成 32 位。
    ///
    /// 这条测试同时锁住「驱动只有 x86 版本」这个前提——若有人误换了 64 位驱动，
    /// 这里会立刻失败。
    #[test]
    fn real_driver_is_x86() {
        let path = real_driver();
        assert!(path.is_file(), "驱动应随仓库分发：{}", path.display());
        assert_eq!(pe_arch(&path), DriverArch::X86);
    }

    /// 造出来的 PE32 / PE32+ 头要能被正确识别。
    #[test]
    fn synthetic_headers_are_recognised() {
        assert_eq!(arch_of_bytes(&fake_pe(MACHINE_I386)), DriverArch::X86);
        assert_eq!(arch_of_bytes(&fake_pe(MACHINE_AMD64)), DriverArch::X86_64);
        assert_eq!(arch_of_bytes(&fake_pe(0x01C4)), DriverArch::Unknown);
    }

    /// 非 PE 输入一律返回 `Unknown`，绝不 panic。
    #[test]
    fn malformed_inputs_are_unknown() {
        assert_eq!(arch_of_bytes(&[]), DriverArch::Unknown);
        assert_eq!(arch_of_bytes(b"MZ"), DriverArch::Unknown);
        // 有 MZ 但 e_lfanew 指向文件外
        let mut bad = vec![0u8; 64];
        bad[0] = b'M';
        bad[1] = b'Z';
        bad[DOS_E_LFANEW_OFFSET] = 0xFF;
        bad[DOS_E_LFANEW_OFFSET + 1] = 0xFF;
        assert_eq!(arch_of_bytes(&bad), DriverArch::Unknown);
        // 有 MZ、e_lfanew 合理，但 PE 签名不对
        let mut no_sig = fake_pe(MACHINE_I386);
        no_sig[0x80] = b'X';
        assert_eq!(arch_of_bytes(&no_sig), DriverArch::Unknown);
    }

    /// 文件不存在要报 `DriverFileMissing`，而不是笼统的"驱动加载失败"。
    #[test]
    fn missing_file_is_file_missing_error() {
        let err = check_loadable(Path::new(r"C:\does\not\exist\LC32RFRW.dll"))
            .expect_err("不存在的文件应报错");
        assert_eq!(err.sdk_code(), 2);
        assert_eq!(err.kind(), "driver_file_missing");
    }

    /// 位数不匹配要明确报出来，并带上双方架构。
    #[test]
    fn arch_mismatch_reports_both_sides() {
        let tmp = std::env::temp_dir().join("rc-device-fake-x64.dll");
        fs::write(&tmp, fake_pe(MACHINE_AMD64)).expect("写临时文件");

        let result = check_loadable(&tmp);
        let _ = fs::remove_file(&tmp);

        // 宿主是 x86 时，64 位 DLL 应被拒；宿主是 x86_64 时，64 位 DLL 可加载
        match host_arch() {
            DriverArch::X86 => {
                let err = result.expect_err("32 位宿主不能加载 64 位 DLL");
                assert_eq!(err.kind(), "driver_arch_mismatch");
                assert!(err.to_string().contains("x86_64"), "错误里应带上 DLL 架构");
            }
            DriverArch::X86_64 => {
                assert_eq!(result.expect("64 位宿主可加载 64 位 DLL"), DriverArch::X86_64);
            }
            DriverArch::Unknown => {}
        }
    }

    /// 架构未知时放行，交给 `LoadLibrary` 报真实错误。
    #[test]
    fn unknown_arch_is_not_blocked() {
        let tmp = std::env::temp_dir().join("rc-device-not-a-pe.dll");
        fs::write(&tmp, b"this is not a PE file at all").expect("写临时文件");

        let result = check_loadable(&tmp);
        let _ = fs::remove_file(&tmp);

        assert_eq!(result.expect("架构未知时应放行"), DriverArch::Unknown);
    }

    /// 宿主架构必须是编译目标的真实架构。
    #[test]
    fn host_arch_matches_target() {
        #[cfg(target_arch = "x86")]
        assert_eq!(host_arch(), DriverArch::X86);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(host_arch(), DriverArch::X86_64);
    }
}
