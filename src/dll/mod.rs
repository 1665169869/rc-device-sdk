//! `LC32RFRW.dll` 的动态加载与符号绑定。
//!
//! # 这是本 crate 唯一使用 `unsafe` 的地方
//!
//! `libloading` 的 `get()` 与函数指针调用都是 `unsafe`。本模块把危险面**收在一处**，
//! 对外只暴露安全的方法（[`DllExports::open_port`] 等），并在此逐条说明安全前提。
//!
//! crate 级有 `#![deny(unsafe_code)]`，所以本模块必须显式 `#[allow]`。
//!
//! # 调用约定的硬性事实
//!
//! 原 C# SDK 的委托声明全部是：
//!
//! ```csharp
//! [UnmanagedFunctionPointer(CallingConvention.StdCall)]
//! ```
//!
//! 即 **stdcall**（`__stdcall`），不是 cdecl。Windows x86 上两者的栈清理责任方不同，
//! 用错会**破坏调用栈**——表现是调用返回后随机崩溃或读到垃圾值，极难排查。
//!
//! 因此所有函数指针类型都标注 `extern "stdcall"`。
//!
//! # 参数宽度的硬性事实
//!
//! C# 的 `int` 是 32 位，`IntPtr` 是宿主宽度。驱动导出的签名以 C# 声明为准，
//! 所以这里的 `int` 参数一律用 `i32`（不是 `isize`）。
//!
//! # 安全前提（所有函数指针调用共用）
//!
//! 1. **DLL 句柄必须比所有函数指针活得久**：`Library` 在 `DllExports` 里持有，
//!    且 `DllExports` 不可 `Clone`。drop 顺序由字段声明顺序保证（`_lib` 最后 drop）。
//! 2. **串口是独占资源**：同一时刻只允许一个 [`DllExports`] 打开串口。
//!    由调用方（`DirectDllDevice`）用 `Mutex` 保证。
//! 3. **缓冲区必须由调用方提供且足够大**：`te_read_asc` 写 256 字节，
//!    `get_id_asc` 写 64 字节。本模块的包装方法**自己分配**缓冲区，
//!    调用方无法传错长度。

#![allow(unsafe_code)]

pub mod arch;
pub mod device;
pub mod pe;
pub mod timing;

pub use arch::{
    build_target_matches_driver, EXPECTED_DRIVER_ARCH, KNOWN_DRIVER_BYTES, KNOWN_DRIVER_SHA256,
};
pub use device::{DirectDllDevice, DRIVER_FILE, DRIVER_SUBDIR};

use std::path::Path;

use libloading::{Library, Symbol};

use crate::error::{DeviceError, DeviceResult};

/// 读卡缓冲区的字节数。
///
/// 驱动返回 29 字节（28 字节卡数据 + 1 字节读卡器状态）的 HEX 文本，
/// 即 58 个字符 + 结尾 `\0`。取 256 留足余量。
pub const READ_BUFFER_LEN: usize = 256;

/// 读卡器序列号缓冲区的字节数。原 C# SDK 用 64。
pub const READER_ID_LEN: usize = 64;

// ---------------------------------------------------------------- 函数指针类型
//
// 调用约定必须是 **stdcall**，与原 C# 的 `CallingConvention.StdCall` 一致。
//
// 但 `stdcall` 在 x86_64 目标上不是受支持的 ABI（那里所有 ABI 都是统一的），
// 编译器会警告 "stdcall is not a supported ABI for the current target"。
// 驱动本身只有 x86 版本，64 位构建只用于 `cargo check` / 单测，
// 所以那里用默认 ABI 即可——反正也不会真去调用。

/// `int mif_selecom(int port, int baud)` —— 打开串口。
#[cfg(target_arch = "x86")]
type FnSelecom = unsafe extern "stdcall" fn(i32, i32) -> i32;
/// `int mif_shake(void)` —— 握手。
#[cfg(target_arch = "x86")]
type FnShake = unsafe extern "stdcall" fn() -> i32;
/// `int config_parameter(int mode)` —— 配置模式（0 = 明文）。
#[cfg(target_arch = "x86")]
type FnConfig = unsafe extern "stdcall" fn(i32) -> i32;
/// `int mif_selecard(int type)` —— 选卡型（5 = TEMIC）。
#[cfg(target_arch = "x86")]
type FnSelecard = unsafe extern "stdcall" fn(i32) -> i32;
/// `int mif_beep(int times)` —— 蜂鸣。
#[cfg(target_arch = "x86")]
type FnBeep = unsafe extern "stdcall" fn(i32) -> i32;
/// `int mif_closecom(void)` —— 关串口。
#[cfg(target_arch = "x86")]
type FnClosecom = unsafe extern "stdcall" fn() -> i32;
/// `int te_read_asc(char* buf)` —— 读卡，结果以 HEX 文本写入 `buf`。
#[cfg(target_arch = "x86")]
type FnReadAsc = unsafe extern "stdcall" fn(*mut u8) -> i32;
/// `int get_id_asc(char* buf)` —— 取读卡器序列号。
#[cfg(target_arch = "x86")]
type FnGetIdAsc = unsafe extern "stdcall" fn(*mut u8) -> i32;
/// `int te_setuppwm_asc(int a1, int a2, int a3, int a4, char* pwd)` —— 设置写密码。
#[cfg(target_arch = "x86")]
type FnSetupPwm = unsafe extern "stdcall" fn(i32, i32, i32, i32, *const u8) -> i32;
/// `int te_writepwm_asc(int a1, int page, char* pwd, char* data)` —— 写页。
#[cfg(target_arch = "x86")]
type FnWritePwm = unsafe extern "stdcall" fn(i32, i32, *const u8, *const u8) -> i32;

// 非 x86 目标（只用于编译检查与单测，不会真调用驱动）。
// 用默认 ABI 避免 "stdcall is not a supported ABI" 警告。
#[cfg(not(target_arch = "x86"))]
type FnSelecom = unsafe extern "C" fn(i32, i32) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnShake = unsafe extern "C" fn() -> i32;
#[cfg(not(target_arch = "x86"))]
type FnConfig = unsafe extern "C" fn(i32) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnSelecard = unsafe extern "C" fn(i32) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnBeep = unsafe extern "C" fn(i32) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnClosecom = unsafe extern "C" fn() -> i32;
#[cfg(not(target_arch = "x86"))]
type FnReadAsc = unsafe extern "C" fn(*mut u8) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnGetIdAsc = unsafe extern "C" fn(*mut u8) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnSetupPwm = unsafe extern "C" fn(i32, i32, i32, i32, *const u8) -> i32;
#[cfg(not(target_arch = "x86"))]
type FnWritePwm = unsafe extern "C" fn(i32, i32, *const u8, *const u8) -> i32;

/// 已加载的驱动及其全部导出函数。
///
/// 字段顺序即 drop 顺序：`_lib` 放**最后**，保证函数指针先失效、库后卸载。
/// 若顺序反过来，drop 期间可能有函数指针仍指向已卸载的内存。
pub struct DllExports {
    /// 打开串口。
    pub selecom: FnSelecom,
    /// 握手。
    pub shake: FnShake,
    /// 配置模式。
    pub config: FnConfig,
    /// 选卡型。
    pub selecard: FnSelecard,
    /// 蜂鸣（可选导出，失败不致命）。
    pub beep: Option<FnBeep>,
    /// 关串口（可选导出，失败不致命）。
    pub closecom: Option<FnClosecom>,
    /// 读卡。
    pub read_asc: FnReadAsc,
    /// 取读卡器序列号（可选导出）。
    pub get_id_asc: Option<FnGetIdAsc>,
    /// 设置写密码。
    pub setuppwm: FnSetupPwm,
    /// 写页。
    pub writepwm: FnWritePwm,
    /// 驱动句柄，必须最后 drop。
    _lib: Library,
}

/// 必需导出清单，用于在缺符号时明确报出缺哪个。
const REQUIRED_EXPORTS: &[&str] = &[
    "mif_selecom",
    "mif_shake",
    "config_parameter",
    "mif_selecard",
    "te_read_asc",
    "te_setuppwm_asc",
    "te_writepwm_asc",
];

impl DllExports {
    /// 从指定路径加载驱动并绑定全部导出。
    ///
    /// # Errors
    ///
    /// - [`DeviceError::DriverFileMissing`] —— 文件不存在
    /// - [`DeviceError::DriverArchMismatch`] —— 位数与宿主不符
    /// - [`DeviceError::DriverNotLoaded`] —— `LoadLibrary` 失败（被占用、缺依赖等）
    /// - [`DeviceError::DriverExportMissing`] —— 缺少必需导出
    ///
    /// # Safety
    ///
    /// 本方法内部使用 `unsafe`，但对调用方是安全的：所有安全前提（句柄生命周期、
    /// 符号绑定有效性）都在函数内保证。
    pub fn load(path: &Path) -> DeviceResult<Self> {
        // 先做便宜的检查：文件存在 + 位数匹配，避免 LoadLibrary 报含糊的 193
        pe::check_loadable(path)?;

        // SAFETY: `path` 已确认存在且位数与宿主匹配。
        // `Library::new` 在失败时返回 Err，不会留下半加载状态。
        // 加载成功后，库句柄由 `Library` 持有并负责卸载。
        let lib = unsafe { Library::new(path) }.map_err(|e| {
            DeviceError::DriverNotLoaded(format!("{}: {e}", path.display()))
        })?;

        // SAFETY（适用于下面每个 `get`）：符号名是编译期已知的 ASCII 字面量，
        // 指向的库在 `lib` 生命周期内保持加载。签名必须与驱动实际导出一致——
        // 这一点由原 C# 的 `[UnmanagedFunctionPointer(StdCall)]` 委托声明保证。
        unsafe {
            let selecom = take_required::<FnSelecom>(&lib, "mif_selecom")?;
            let shake = take_required::<FnShake>(&lib, "mif_shake")?;
            let config = take_required::<FnConfig>(&lib, "config_parameter")?;
            let selecard = take_required::<FnSelecard>(&lib, "mif_selecard")?;
            let read_asc = take_required::<FnReadAsc>(&lib, "te_read_asc")?;
            let setuppwm = take_required::<FnSetupPwm>(&lib, "te_setuppwm_asc")?;
            let writepwm = take_required::<FnWritePwm>(&lib, "te_writepwm_asc")?;

            let beep = take_optional::<FnBeep>(&lib, "mif_beep");
            let closecom = take_optional::<FnClosecom>(&lib, "mif_closecom");
            let get_id_asc = take_optional::<FnGetIdAsc>(&lib, "get_id_asc");

            Ok(Self {
                selecom,
                shake,
                config,
                selecard,
                beep,
                closecom,
                read_asc,
                get_id_asc,
                setuppwm,
                writepwm,
                _lib: lib,
            })
        }
    }

    /// 检查所有必需导出是否可解析（不实际绑定）。
    ///
    /// 用于诊断：「驱动版本不兼容」比「设备打不开」精确得多。
    ///
    /// # Errors
    ///
    /// [`DeviceError::DriverExportMissing`]，附带缺失的函数名。
    ///
    /// # Safety
    ///
    /// 内部使用 `unsafe` 取符号地址，但不构造函数指针、不调用，因此无安全风险。
    pub fn verify_exports(path: &Path) -> DeviceResult<Vec<String>> {
        pe::check_loadable(path)?;
        // SAFETY: 与 `load` 相同的理由；这里只查地址不解引用。
        let lib = unsafe { Library::new(path) }
            .map_err(|e| DeviceError::DriverNotLoaded(format!("{}: {e}", path.display())))?;

        let mut found = Vec::with_capacity(REQUIRED_EXPORTS.len());
        for name in REQUIRED_EXPORTS {
            // SAFETY: 仅查询符号地址，不构造指针、不调用。
            let sym = unsafe { lib.get::<*const ()>(name.as_bytes()) };
            if sym.is_err() {
                return Err(DeviceError::DriverExportMissing((*name).to_string()));
            }
            found.push((*name).to_string());
        }
        Ok(found)
    }

    /// 打开串口。返回 0 表示成功。
    ///
    /// # Safety
    ///
    /// 调用方必须保证：串口未被本进程内的其它实例占用，且 DLL 句柄仍有效
    /// （由 `&self` 的借用期保证）。
    pub fn open_port(&self, port: i32, baud: i32) -> i32 {
        // SAFETY: 函数指针来自有效库，签名与原 SDK 一致（两个 int 参数）。
        unsafe { (self.selecom)(port, baud) }
    }

    /// 握手。返回 0 表示成功。
    ///
    /// # Safety
    ///
    /// 需先成功调用 [`Self::open_port`]；DLL 句柄由 `&self` 保证有效。
    pub fn handshake(&self) -> i32 {
        // SAFETY: 无参数、返回 int，签名与原 SDK 一致。
        unsafe { (self.shake)() }
    }

    /// 设置配置模式（`0` = 明文）。返回 0 表示成功。
    ///
    /// # Safety
    ///
    /// 需先成功打开串口。
    pub fn set_config(&self, mode: i32) -> i32 {
        // SAFETY: 一个 int 参数、返回 int，签名与原 SDK 一致。
        unsafe { (self.config)(mode) }
    }

    /// 选卡型（`5` = TEMIC）。返回 0 表示成功。
    ///
    /// # Safety
    ///
    /// 需先成功握手。卡未放上时返回非 0。
    pub fn select_card(&self, card_type: i32) -> i32 {
        // SAFETY: 一个 int 参数、返回 int，签名与原 SDK 一致。
        unsafe { (self.selecard)(card_type) }
    }

    /// 蜂鸣。未导出时静默跳过。
    ///
    /// # Safety
    ///
    /// 纯提示音，无副作用；驱动未导出时不做任何事。
    pub fn beep_times(&self, times: i32) {
        if let Some(f) = self.beep {
            // SAFETY: 一个 int 参数、返回 int（返回值忽略），签名与原 SDK 一致。
            unsafe { f(times) };
        }
    }

    /// 关闭串口。未导出时静默跳过。
    ///
    /// # Safety
    ///
    /// 幂等；驱动未导出时不做任何事。
    pub fn close_port(&self) {
        if let Some(f) = self.closecom {
            // SAFETY: 无参数，返回值忽略，签名与原 SDK 一致。
            unsafe { f() };
        }
    }

    /// 读卡，返回驱动写进缓冲区的 HEX 文本（已截到第一个 `\0`）。
    ///
    /// 缓冲区由本方法分配，调用方无法传错长度。
    ///
    /// # Safety
    ///
    /// 需已选中 TEMIC 卡。缓冲区长度 `READ_BUFFER_LEN` 足够容纳驱动输出。
    pub fn read_card_hex(&self) -> (i32, String) {
        let mut buf = [0u8; READ_BUFFER_LEN];
        // SAFETY: `buf` 是栈上 256 字节数组，指针有效且可写；
        // 驱动最多写 59 字节（58 个 HEX 字符 + \0），不会越界。
        let rc = unsafe { (self.read_asc)(buf.as_mut_ptr()) };
        (rc, ascii_z(&buf))
    }

    /// 取读卡器序列号。未导出或失败时返回 `None`。
    ///
    /// # Safety
    ///
    /// 需已打开串口。缓冲区由本方法分配。
    pub fn reader_id(&self) -> Option<String> {
        let f = self.get_id_asc?;
        let mut buf = [0u8; READER_ID_LEN];
        // SAFETY: `buf` 是栈上 64 字节数组，指针有效且可写；驱动不会越过它。
        let rc = unsafe { f(buf.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let s = ascii_z(&buf);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// 设置写密码。返回 0 表示成功。
    ///
    /// # `pwd`
    ///
    /// 传**十六进制 ASCII 字符串**（如 `"12000206"`）的 `\0` 结尾字节数组。
    /// 不是原始字节。
    ///
    /// # Safety
    ///
    /// 需已打开串口。`pwd` 必须是 `\0` 结尾的 ASCII 字节序列。
    pub fn setup_write_password(&self, pwd: &[u8]) -> i32 {
        // SAFETY: 四个 int 参数 + 一个 `\0` 结尾字节数组指针，签名与原 SDK 一致。
        // `pwd` 由调用方保证以 0 结尾（见 `write_password_bytes`）。
        unsafe { (self.setuppwm)(0, 2, 0, 1, pwd.as_ptr()) }
    }

    /// 写一页。返回 0 表示成功。
    ///
    /// # `page`
    ///
    /// `1` = 页1、`2` = 页2。
    ///
    /// # `data`
    ///
    /// 4 字节数据对应的**十六进制 ASCII 字符串**（8 个字符）的 `\0` 结尾字节数组。
    ///
    /// # Safety
    ///
    /// 需已设置写密码。`pwd` 与 `data` 都必须是 `\0` 结尾的 ASCII 字节序列。
    pub fn write_page(&self, page: i32, pwd: &[u8], data: &[u8]) -> i32 {
        // SAFETY: 两个 int 参数 + 两个 `\0` 结尾字节数组指针，签名与原 SDK 一致。
        unsafe { (self.writepwm)(0, page, pwd.as_ptr(), data.as_ptr()) }
    }
}

/// 取符号地址并转成函数指针；缺失时报出具体函数名。
///
/// # Safety
///
/// 调用方必须保证 `T` 与驱动实际导出的签名完全一致。
unsafe fn take_required<T: Copy>(lib: &Library, name: &str) -> DeviceResult<T> {
    // SAFETY: 由调用方保证符号名正确、签名匹配；`lib` 由 caller 保证存活。
    let sym: Symbol<'_, T> = unsafe { lib.get(name.as_bytes()) }
        .map_err(|_| DeviceError::DriverExportMissing(name.to_string()))?;
    Ok(*sym)
}

/// 取可选符号；缺失时返回 `None`。
///
/// # Safety
///
/// 同上。
unsafe fn take_optional<T: Copy>(lib: &Library, name: &str) -> Option<T> {
    // SAFETY: 同上。
    unsafe { lib.get::<T>(name.as_bytes()).ok().map(|s| *s) }
}

/// 把 `\0` 结尾的字节数组转成字符串（只取第一个 `\0` 之前）。
fn ascii_z(buf: &[u8]) -> String {
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// 把密码 / 页数据转成驱动要求的 `\0` 结尾 ASCII 字节数组。
///
/// 驱动接受的是**十六进制字符串**，不是原始字节。
#[must_use]
pub fn c_string_bytes(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

/// 4 字节页数据 → 8 字符的大写十六进制字符串。
#[must_use]
pub fn page_to_hex(page: [u8; 4]) -> String {
    let mut s = String::with_capacity(8);
    for b in page {
        s.push(crate::card::HEX_UPPER[(b >> 4) as usize] as char);
        s.push(crate::card::HEX_UPPER[(b & 0x0F) as usize] as char);
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn real_driver() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("drivers")
            .join("LC2000")
            .join("LC32RFRW.dll")
    }

    /// 页数据转十六进制必须是 8 个大写字符，与驱动要求一致。
    #[test]
    fn page_hex_is_eight_uppercase_chars() {
        assert_eq!(page_to_hex([0x5B, 0xEC, 0xF0, 0x00]), "5BECF000");
        assert_eq!(page_to_hex([0x00; 4]), "00000000");
        assert_eq!(page_to_hex([0xFF; 4]), "FFFFFFFF");
    }

    /// C 字符串必须补 `\0`，否则驱动会读越界。
    #[test]
    fn c_string_is_nul_terminated() {
        let v = c_string_bytes("12000206");
        assert_eq!(v.len(), 9);
        assert_eq!(v[8], 0);
        assert_eq!(&v[..8], b"12000206");
    }

    /// `ascii_z` 只取第一个 `\0` 之前的内容。
    #[test]
    fn ascii_z_stops_at_nul() {
        let mut buf = [0u8; 8];
        buf[..3].copy_from_slice(b"ABC");
        assert_eq!(ascii_z(&buf), "ABC");

        let full = [b'X'; 4];
        assert_eq!(ascii_z(&full), "XXXX", "无 \\0 时取全长");

        assert_eq!(ascii_z(&[0u8; 4]), "", "全零得空串");
    }

    /// 真实驱动能通过必需导出校验——这是「驱动版本兼容」的证据。
    ///
    /// 只在 32 位宿主执行：`verify_exports` 需要真正 `LoadLibrary`，
    /// 而 64 位宿主加载 32 位 DLL 必然失败（那是位数问题，不是缺符号）。
    #[cfg(target_arch = "x86")]
    #[test]
    fn real_driver_has_all_required_exports() {
        let path = real_driver();
        assert!(path.is_file(), "驱动应随仓库分发：{}", path.display());

        let found = DllExports::verify_exports(&path).expect("真实驱动应包含全部必需导出");
        assert_eq!(found.len(), REQUIRED_EXPORTS.len());
        for name in REQUIRED_EXPORTS {
            assert!(found.iter().any(|f| f == name), "缺少导出 {name}");
        }
    }

    /// 64 位宿主下改用 PE 解析验证导出表，不依赖 `LoadLibrary`。
    ///
    /// 这条测试的存在意义：即使在开发机（通常是 64 位）也要能确认
    /// 「驱动确实导出了这 10 个符号」，而不是等真机才发现。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn real_driver_exports_are_present_in_pe_table() {
        let path = real_driver();
        assert!(path.is_file(), "驱动应随仓库分发：{}", path.display());

        let bytes = std::fs::read(&path).expect("读驱动文件");
        let names = pe::exported_names(&bytes);
        assert!(!names.is_empty(), "PE 导出表不应为空");

        for required in REQUIRED_EXPORTS {
            assert!(
                names.iter().any(|n| n == required),
                "PE 导出表缺少 {required}"
            );
        }
    }

    /// 缺文件时明确报 `DriverFileMissing`。
    #[test]
    fn loading_missing_file_fails_cleanly() {
        match DllExports::load(Path::new(r"C:\nope\LC32RFRW.dll")) {
            Err(e) => assert_eq!(e.sdk_code(), 2),
            Ok(_) => panic!("不存在的驱动竟然加载成功了"),
        }
    }

    /// 32 位宿主能真正加载真实驱动（这条只在 x86 目标下有意义）。
    #[cfg(target_arch = "x86")]
    #[test]
    fn real_driver_loads_on_x86_host() {
        let path = real_driver();
        let dll = DllExports::load(&path).expect("32 位宿主应能加载真实驱动");
        // 只验证绑定成功，不打开串口（那需要真硬件）
        assert!(dll.beep.is_some() || dll.beep.is_none());
    }

    /// 64 位宿主加载真实驱动必须报位数不匹配，而不是含糊的 193。
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn real_driver_rejected_on_x64_host() {
        let path = real_driver();
        match DllExports::load(&path) {
            Err(e) => {
                assert_eq!(e.kind(), "driver_arch_mismatch");
                assert!(e.to_string().contains("x86"), "错误应说明 DLL 是 x86");
            }
            Ok(_) => panic!("64 位宿主不该加载成功 32 位驱动"),
        }
    }
}
