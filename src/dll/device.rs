//! `CardDevice` 的生产实现：直接动态加载 `LC32RFRW.dll`。
//!
//! # 设备与业务的分界
//!
//! 本文件只做设备 I/O：打开串口、读卡、写卡、回读校验、探测端口。
//! **它不知道房号、房态、入住单、发卡上限**——那些全在业务层。
//!
//! # 会话与并发
//!
//! 串口是独占资源，同时只能有一个实例在用。全部驱动调用经 `Mutex` 串行化，
//! 保证不会有两个写卡动作交错。
//!
//! # 为什么不自动重试写卡
//!
//! 写卡是硬件动作。重试可能把同一张卡写两遍、把卡号推进两次，比失败本身更难处理。
//! 失败就返回错误，让前台人工确认。
//!
//! 唯一的例外是**回读校验的 3 次重试**——那是协议的一部分：
//! 写完立刻读可能拿到旧值，这是实测结论。
//!
//! # 打开序列（严格照抄原 C# `LockCardReader.Open`）
//!
//! ```text
//! mif_selecom(port, 9600) != 0     → PortOpenFailed
//! sleep 150ms
//! mif_shake() != 0                 → config_parameter(0); sleep 80ms; mif_shake()
//! config_parameter(0)              ← 关键：明文模式
//! sleep 60ms
//! mif_shake()
//! mif_selecard(5) != 0             → NoCard（没插卡/卡不在读卡器上）
//! get_id_asc(buf)                  ← 失败不致命
//! ```

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::builder;
use crate::card::{
    CardSnapshot, CheckoutHourSpec, DeviceBackend, DeviceInfo, DriverHashStatus, GuestDaySpec,
    GuestHourSpec, InstallSpec, MasterSpec, PlainCardSpec, PortProbe, SelfTestReport, TimeSyncSpec,
    CARD_BYTES,
};
use crate::card_device::CardDevice;
use crate::dll::arch::KNOWN_DRIVER_SHA256;
use crate::dll::timing::{
    CONFIG_SETTLE_MS, OPEN_SETTLE_MS, RESHAKE_DELAY_MS, SETUP_PWM_DELAY_MS, VERIFY_RETRY_DELAY_MS,
    WRITE_PAGE1_DELAY_MS, WRITE_PAGE2_DELAY_MS,
};
use crate::dll::{c_string_bytes, page_to_hex, pe, DllExports};
use crate::error::{DeviceError, DeviceResult};
use crate::parser;
use crate::protocol::{
    CARD_TYPE_TEMIC, CONFIG_MODE_PLAIN, MAX_PORT_INDEX, SERIAL_BAUD, VERIFY_ATTEMPTS,
    WRITE_PASSWORD,
};

/// 驱动文件名。
pub const DRIVER_FILE: &str = "LC32RFRW.dll";

/// 小写十六进制字符表，供 SHA-256 摘要输出使用。
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// 驱动目录相对路径（随包分发时的布局）。
pub const DRIVER_SUBDIR: &str = "IOServer/LC2000";

/// 读卡返回的十六进制串最小长度：28 字节 → 56 个字符。
const MIN_HEX_LEN: usize = CARD_BYTES * 2;

/// 直接加载驱动的设备实现。
pub struct DirectDllDevice {
    /// 驱动目录。
    driver_dir: PathBuf,
    /// 驱动与串口状态。`Mutex` 串行化全部驱动调用（串口是独占资源）。
    state: Mutex<SessionState>,
}

/// 会话状态：已加载的驱动 + 是否已打开 + 当前端口。
///
/// 端口放在状态里而不是 `&self` 的不可变字段上，因为 [`CardDevice::set_port`]
/// 需要在 `&self` 下切换端口。
struct SessionState {
    dll: Option<DllExports>,
    opened: bool,
    reader_id: Option<String>,
    /// 当前串口索引 0–7。
    port: u8,
}

impl SessionState {
    fn closed(port: u8) -> Self {
        Self {
            dll: None,
            opened: false,
            reader_id: None,
            port,
        }
    }
}

impl DirectDllDevice {
    /// 用驱动目录构造设备。默认串口索引 6（COM7），与原厂软件一致。
    ///
    /// 注：不同机器上读卡器挂的串口不同（实测有 COM3、COM7 两种），
    /// 用 [`CardDevice::probe_ports`] 找到实际端口后再 [`CardDevice::set_port`]。
    #[must_use]
    pub fn new(driver_dir: impl Into<PathBuf>) -> Self {
        Self {
            driver_dir: driver_dir.into(),
            state: Mutex::new(SessionState::closed(6)),
        }
    }

    /// 构造并自动探测可用串口。
    ///
    /// 探测到读卡器就把端口切过去；一个都没找到则保留默认值 6（COM7）。
    /// 这样命令行工具与首启向导都不必写死端口。
    #[must_use]
    pub fn with_auto_port(driver_dir: impl Into<PathBuf>) -> Self {
        let dev = Self::new(driver_dir);
        if let Ok(list) = dev.probe_ports() {
            if let Some(found) = list.iter().find(|p| p.available) {
                let _ = dev.set_port(found.port);
            }
        }
        dev
    }

    /// 用「运行时目录」构造：驱动放在其下的 `IOServer/LC2000/`。
    #[must_use]
    pub fn from_runtime_dir(runtime_dir: impl Into<PathBuf>) -> Self {
        Self::new(runtime_dir.into().join(DRIVER_SUBDIR))
    }

    /// 覆盖串口索引。
    #[must_use]
    pub fn with_port(self, port: u8) -> Self {
        if port <= MAX_PORT_INDEX {
            if let Ok(mut st) = self.state.lock() {
                st.port = port;
            }
        }
        self
    }

    /// 驱动 DLL 的完整路径。
    #[must_use]
    pub fn driver_path(&self) -> PathBuf {
        self.driver_dir.join(DRIVER_FILE)
    }

    /// 取会话锁；锁中毒时统一报 I/O 错，不 panic。
    fn lock(&self) -> DeviceResult<std::sync::MutexGuard<'_, SessionState>> {
        self.state
            .lock()
            .map_err(|_| DeviceError::Io("设备会话锁已中毒，请重启程序".to_string()))
    }

    /// 确保驱动已加载且串口已打开。
    fn ensure_open(&self) -> DeviceResult<std::sync::MutexGuard<'_, SessionState>> {
        let mut st = self.lock()?;
        if st.opened {
            return Ok(st);
        }

        // 加载驱动（已加载则复用）
        if st.dll.is_none() {
            st.dll = Some(DllExports::load(&self.driver_path())?);
        }
        let dll = st
            .dll
            .as_ref()
            .ok_or_else(|| DeviceError::DriverNotLoaded("驱动句柄丢失".to_string()))?;

        // ---- 打开序列，逐个延时都是实测必需 ----
        if dll.open_port(i32::from(st.port), i32::try_from(SERIAL_BAUD).unwrap_or(i32::MAX)) != 0 {
            st.dll = None;
            return Err(DeviceError::PortOpenFailed { port: st.port });
        }
        sleep_ms(OPEN_SETTLE_MS);

        // 握手；首次失败说明读卡器可能处于加扰模式，重新配置后再试一次
        let mut rc = dll.handshake();
        if rc != 0 {
            let _ = dll.set_config(CONFIG_MODE_PLAIN);
            sleep_ms(RESHAKE_DELAY_MS);
            rc = dll.handshake();
        }
        // 两次都失败：读卡器没插好或没供电（原 C# 此处同样不立即返回，
        // 而是继续走配置+握手，靠后面的 selecard 兜底——实测这样更稳）
        let handshake_ok = rc == 0;

        // 明文模式：必须在写卡前设置，否则"能读不能写"
        let _ = dll.set_config(CONFIG_MODE_PLAIN);
        sleep_ms(CONFIG_SETTLE_MS);
        let _ = dll.handshake();

        // 选 TEMIC 卡型；卡不在读卡器上时这里会失败。
        // 握手彻底失败时也走这个分支：两种情况下驱动都拿不到卡，
        // 现场看到的都是"读不到卡"，统一报 NoCard 反而更贴近事实。
        if dll.select_card(CARD_TYPE_TEMIC) != 0 {
            dll.close_port();
            st.dll = None;
            // 握手都没成功时，更可能是读卡器没连好而不是没放卡
            if !handshake_ok {
                return Err(DeviceError::HandshakeFailed);
            }
            return Err(DeviceError::NoCard);
        }

        st.reader_id = dll.reader_id();
        st.opened = true;
        Ok(st)
    }

    /// 读卡的原始 28 字节。
    fn read_raw(st: &SessionState) -> DeviceResult<[u8; CARD_BYTES]> {
        let dll = st
            .dll
            .as_ref()
            .ok_or(DeviceError::NotOpen)?;
        let (rc, hex) = dll.read_card_hex();
        if rc != 0 {
            return Err(DeviceError::ReadFailed(format!("驱动返回 {rc}")));
        }
        parse_hex28(&hex)
    }

    /// 写卡核心：设置密码 → 写页1 → 写页2 → 回读校验。
    ///
    /// 页数据已由 `builder` 构造好，这里只负责"把它写进卡并确认写进去了"。
    fn write_pages(
        &self,
        p1: [u8; 4],
        p2: [u8; 4],
        room_index: Option<u16>,
    ) -> DeviceResult<CardSnapshot> {
        let st = self.ensure_open()?;
        let dll = st.dll.as_ref().ok_or(DeviceError::NotOpen)?;

        let pwd = c_string_bytes(WRITE_PASSWORD);

        if dll.setup_write_password(&pwd) != 0 {
            return Err(DeviceError::WriteFailed("设置写密码失败".to_string()));
        }
        sleep_ms(SETUP_PWM_DELAY_MS);

        if dll.write_page(1, &pwd, c_string_bytes(&page_to_hex(p1)).as_slice()) != 0 {
            return Err(DeviceError::WriteFailed("写页1 失败".to_string()));
        }
        sleep_ms(WRITE_PAGE1_DELAY_MS);

        if dll.write_page(2, &pwd, c_string_bytes(&page_to_hex(p2)).as_slice()) != 0 {
            return Err(DeviceError::WriteFailed("写页2 失败".to_string()));
        }
        sleep_ms(WRITE_PAGE2_DELAY_MS);

        // 回读校验：最多 3 次。写完立刻读可能拿到旧值，这是协议要求不是容错。
        for _ in 0..VERIFY_ATTEMPTS {
            if let Ok(raw) = Self::read_raw(&st) {
                if raw[0..4] == p1 && raw[4..8] == p2 {
                    dll.beep_times(2);
                    let mut snap = parser::parse(&raw)?;
                    if let Some(idx) = room_index {
                        snap.room_index = Some(idx);
                    }
                    return Ok(snap);
                }
            }
            sleep_ms(VERIFY_RETRY_DELAY_MS);
        }
        Err(DeviceError::VerifyFailed)
    }
}

impl CardDevice for DirectDllDevice {
    fn open(&self) -> DeviceResult<()> {
        let mut st = self.ensure_open()?;
        // ensure_open 已置 opened；这里只是消耗掉 guard
        st.opened = true;
        Ok(())
    }

    fn close(&self) -> DeviceResult<()> {
        let mut st = self.lock()?;
        if let Some(dll) = st.dll.as_ref() {
            dll.close_port();
        }
        st.opened = false;
        st.reader_id = None;
        // 保留 dll 句柄以便快速重开；真正卸载要 drop
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.opened)
    }

    fn read_card(&self) -> DeviceResult<CardSnapshot> {
        let st = self.ensure_open()?;
        let raw = Self::read_raw(&st)?;
        parser::parse(&raw)
    }

    fn write_guest_day(&self, spec: &GuestDaySpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_guest_day(spec)?;
        self.write_pages(p1, p2, Some(spec.room_index))
    }

    fn write_guest_hour(&self, spec: &GuestHourSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_guest_hour(spec)?;
        self.write_pages(p1, p2, Some(spec.room_index))
    }

    fn write_master(&self, spec: &MasterSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_master(spec)?;
        self.write_pages(p1, p2, None)
    }

    fn write_install(&self, spec: &InstallSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_install(spec)?;
        self.write_pages(p1, p2, Some(spec.room_index))
    }

    fn write_reset(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_reset(spec)?;
        self.write_pages(p1, p2, None)
    }

    fn write_clear(&self, spec: &PlainCardSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_clear(spec)?;
        self.write_pages(p1, p2, None)
    }

    fn write_checkout_hour(&self, spec: &CheckoutHourSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_checkout_hour(spec)?;
        self.write_pages(p1, p2, None)
    }

    fn write_timesync(&self, spec: &TimeSyncSpec) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::build_timesync(spec)?;
        self.write_pages(p1, p2, None)
    }

    fn erase_card(&self) -> DeviceResult<CardSnapshot> {
        let (p1, p2) = builder::ERASE_PAGES;
        self.write_pages(p1, p2, None)
    }

    fn probe_ports(&self) -> DeviceResult<Vec<PortProbe>> {
        let mut out = Vec::with_capacity(usize::from(MAX_PORT_INDEX) + 1);
        for port in 0..=MAX_PORT_INDEX {
            // 探测只关心「这个串口上有没有读卡器」，不应该受"卡在不在读卡器上"影响。
            // 所以这里不用 `open()`（它包含 selecard，没卡就会失败），
            // 而是走 `probe_port_once`：只做打开+握手+取序列号。
            let (available, reader_id, failure) = probe_port_once(&self.driver_dir, port);
            out.push(PortProbe {
                port,
                com_name: format!("COM{}", u16::from(port) + 1),
                available,
                reader_id,
                failure,
            });
        }
        Ok(out)
    }

    fn set_port(&self, port: u8) -> DeviceResult<()> {
        if port > MAX_PORT_INDEX {
            return Err(DeviceError::Param(format!(
                "串口索引必须在 0–{MAX_PORT_INDEX}，实际 {port}"
            )));
        }
        let mut st = self.lock()?;
        // 换端口前先关掉旧串口，避免占着不放
        if let Some(dll) = st.dll.as_ref() {
            dll.close_port();
        }
        st.opened = false;
        st.reader_id = None;
        st.port = port;
        Ok(())
    }

    fn selftest(&self) -> DeviceResult<SelfTestReport> {
        // 纯离线，不碰硬件
        Ok(crate::selftest::run())
    }

    fn device_info(&self) -> DeviceResult<DeviceInfo> {
        let st = self.lock()?;
        let path = self.driver_path();
        let arch = pe::pe_arch(&path);
        let (sha256, hash_status) = driver_hash_status(&path);
        Ok(DeviceInfo {
            backend: DeviceBackend::DirectDll,
            driver_path: path.display().to_string(),
            driver_sha256: sha256,
            driver_hash_status: hash_status,
            driver_arch: arch,
            port: st.port,
            baud: SERIAL_BAUD,
            opened: st.opened,
            reader_id: st.reader_id.clone(),
        })
    }
}

/// 探测单个串口：只做「打开 → 握手 → 取序列号」，**不选卡**。
///
/// # 为什么不选卡
///
/// `mif_selecard(5)` 在卡不在读卡器上时会失败。若探测以它为判据，
/// 「读卡器插着但没放卡」会被误报成「这个串口没有读卡器」——
/// 现场就会去拔线重插,而其实只是没放卡。
///
/// 返回 `(是否可用, 序列号, 失败原因)`。
fn probe_port_once(driver_dir: &Path, port: u8) -> (bool, Option<String>, Option<String>) {
    let path = driver_dir.join(DRIVER_FILE);
    let dll = match DllExports::load(&path) {
        Ok(d) => d,
        Err(e) => return (false, None, Some(e.to_string())),
    };

    if dll.open_port(i32::from(port), i32::try_from(SERIAL_BAUD).unwrap_or(i32::MAX)) != 0 {
        return (
            false,
            None,
            Some(format!(
                "串口 COM{} 打不开（可能被其它门锁软件占用）",
                u16::from(port) + 1
            )),
        );
    }
    sleep_ms(OPEN_SETTLE_MS);

    let mut rc = dll.handshake();
    if rc != 0 {
        let _ = dll.set_config(CONFIG_MODE_PLAIN);
        sleep_ms(RESHAKE_DELAY_MS);
        rc = dll.handshake();
    }
    let _ = dll.set_config(CONFIG_MODE_PLAIN);
    sleep_ms(CONFIG_SETTLE_MS);

    if rc != 0 {
        dll.close_port();
        return (
            false,
            None,
            Some("串口能打开但握手失败（读卡器未插好或未供电）".to_string()),
        );
    }

    let reader_id = dll.reader_id();
    dll.close_port();
    (true, reader_id, None)
}

/// 睡眠指定毫秒。封装成函数便于将来统一替换（例如测试里注入假时钟）。
fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// 把 56+ 字符的十六进制串解析成 28 字节卡数据。
///
/// 驱动返回 29 字节（58 字符），最后 1 字节是读卡器状态，恒为 `0x02`——
/// 只要前 28 字节，与卡内容无关。
fn parse_hex28(hex: &str) -> DeviceResult<[u8; CARD_BYTES]> {
    let t = hex.trim();
    if t.len() < MIN_HEX_LEN {
        return Err(DeviceError::ReadFailed(format!(
            "读卡返回的十六进制串只有 {} 字符，至少需要 {MIN_HEX_LEN}",
            t.len()
        )));
    }
    let mut raw = [0u8; CARD_BYTES];
    for (i, slot) in raw.iter_mut().enumerate() {
        let start = i * 2;
        let pair = &t[start..start + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|e| {
            DeviceError::Protocol(format!("读卡数据不是合法十六进制（第 {i} 字节 {pair}）：{e}"))
        })?;
    }
    Ok(raw)
}

/// 计算文件 SHA-256（用于比对已验证的驱动版本）。
///
/// 找不到文件或读不出来时返回 `None`——设备信息不应因为算不出哈希而报错。
fn sha256_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(sha256_hex(&bytes))
}

/// 计算驱动哈希并与已验证版本比对，返回 `(原始哈希, 比对结论)`。
///
/// # 为什么在设备层比对
///
/// 比对只需要两样东西：文件哈希、以及「已验证版本是哪一版」。后者是编译期常量
/// [`KNOWN_DRIVER_SHA256`]，本来就只属于硬件层。把它推给业务层意味着业务层要抄
/// 一份 64 位十六进制常量——那份副本迟早与驱动文件脱节。
///
/// # 不符时不报错
///
/// 哈希不符不是故障：可能是厂商小版本升级，也可能是文件被替换，我们无法区分。
/// 拒绝加载会让现场直接不可用，所以只如实报告结论，由业务层显示警告。
fn driver_hash_status(path: &Path) -> (Option<String>, DriverHashStatus) {
    match sha256_of(path) {
        Some(actual) if actual == KNOWN_DRIVER_SHA256 => (Some(actual), DriverHashStatus::Verified),
        Some(actual) => (
            Some(actual.clone()),
            DriverHashStatus::Mismatch {
                expected: KNOWN_DRIVER_SHA256.to_string(),
                actual,
            },
        ),
        None => (None, DriverHashStatus::Unknown),
    }
}

/// 供其它模块的测试复用的 SHA-256 入口。
///
/// 单独暴露而非直接 `pub` 整个实现：`arch.rs` 的测试要独立校验「记录的哈希
/// 与真实文件一致」，如果它自己再写一份实现，两份实现同时写错就会互相印证。
#[cfg(test)]
pub(crate) fn sha256_hex_for_test(data: &[u8]) -> String {
    sha256_hex(data)
}

/// 纯 Rust 的 SHA-256（避免为了一个哈希引入依赖）。
///
/// 输出**小写**十六进制 64 字符，与常见工具（`certutil`、`sha256sum`）及
/// [`DeviceInfo::driver_sha256`] 的约定一致。比对时两边都是小写，不会因大小写误判。
///
/// # lint 豁免
///
/// - `unreadable_literal`：常量表是 FIPS 180-4 规定的标准初始值，逐位核对时
///   不加下划线反而更容易与规范原文对照。
/// - `many_single_char_names`：`a`–`h` 是 SHA-256 规范里的固定变量名，
///   改名会让人无法与规范逐行对照。
/// - `cast_possible_truncation`：`data.len()` 转 u64 在 32/64 位下都无损。
///
/// 正确性由 [`tests::sha256_matches_known_vectors`] 用官方测试向量保证。
#[allow(
    clippy::unreadable_literal,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::needless_range_loop
)]
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, s) in w.iter_mut().take(16).enumerate() {
            let b = i * 4;
            *s = u32::from_be_bytes([chunk[b], chunk[b + 1], chunk[b + 2], chunk[b + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut s = String::with_capacity(64);
    for v in h {
        for b in v.to_be_bytes() {
            s.push(HEX_LOWER[(b >> 4) as usize] as char);
            s.push(HEX_LOWER[(b & 0x0F) as usize] as char);
        }
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// 56 字符的十六进制能正确解析成 28 字节。
    #[test]
    fn parse_hex28_reads_28_bytes() {
        let hex = "5BECF000858822AB0000000000000000000000000000000000000000";
        let raw = parse_hex28(hex).expect("合法十六进制应解析成功");
        assert_eq!(&raw[0..8], &[0x5B, 0xEC, 0xF0, 0x00, 0x85, 0x88, 0x22, 0xAB]);
        assert_eq!(raw.len(), CARD_BYTES);
    }

    /// 驱动返回 58 字符（29 字节），多出的状态字节要被忽略。
    #[test]
    fn parse_hex28_ignores_trailing_status_byte() {
        let hex = "5BECF000858822AB000000000000000000000000000000000000000002";
        assert_eq!(hex.len(), 58, "驱动实际返回 58 字符");
        let raw = parse_hex28(hex).expect("应解析成功");
        assert_eq!(&raw[0..2], &crate::card::CARD_PREFIX);
    }

    /// 太短的串要报读卡失败，不能"猜测补齐"。
    #[test]
    fn parse_hex28_rejects_short_input() {
        let err = parse_hex28("5BEC").expect_err("过短应报错");
        assert_eq!(err.sdk_code(), 6);
        assert!(err.to_string().contains("至少需要"));
    }

    /// 非十六进制字符要报协议错误。
    #[test]
    fn parse_hex28_rejects_invalid_hex() {
        let hex = "ZZ".repeat(28);
        let err = parse_hex28(&hex).expect_err("非法字符应报错");
        assert_eq!(err.sdk_code(), -1);
        assert!(err.kind().eq("protocol"));
    }

    /// SHA-256 实现必须与标准答案一致（否则驱动指纹比对就失去意义）。
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 驱动指纹必须与已验证版本一致——这是「卡格式结论只对该版本成立」的锚点。
    #[test]
    fn real_driver_hash_matches_known_version() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("drivers")
            .join("LC2000")
            .join(DRIVER_FILE);
        let hash = sha256_of(&path).expect("驱动应可读");
        assert_eq!(
            hash, "f2f483e08ba8048ffd1c57e66cb35d9e32c1c1e280dc5f2ff100a5d881bccbc0",
            "驱动版本与已验证版本不一致，卡格式结论需重新评估"
        );
    }

    /// 未打开时 `is_open` 为 false，`device_info` 也能正常返回。
    #[test]
    fn device_info_works_before_open() {
        let dev = DirectDllDevice::from_runtime_dir("C:/nonexistent-runtime");
        assert!(!dev.is_open());
        let info = dev.device_info().expect("设备信息不应依赖打开状态");
        assert_eq!(info.backend, DeviceBackend::DirectDll);
        assert!(!info.opened);
        assert_eq!(info.baud, SERIAL_BAUD);
    }

    /// 仓库里这份驱动必须被判为「已验证」——这是哈希告警机制的正向基线。
    ///
    /// 若这条失败，说明驱动被换过：**卡格式结论需要重新评估**，
    /// 此时 `driver_hash_status` 会变成 `Mismatch`，业务层据此显示警告。
    #[test]
    fn shipped_driver_hash_is_verified() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("drivers")
            .join("LC2000")
            .join(DRIVER_FILE);
        let (sha, status) = driver_hash_status(&path);
        assert_eq!(sha.as_deref(), Some(KNOWN_DRIVER_SHA256));
        assert_eq!(status, DriverHashStatus::Verified);
        assert!(status.is_verified());
        assert_eq!(status.warning(), None, "已验证时不该有告警");
    }

    /// 哈希不符时**不报错**，只如实给出 `Mismatch`，并带期望值与实际值。
    ///
    /// 这是本条目最关键的一条：不符必须能加载、能工作，只是结论不可信。
    #[test]
    fn mismatched_hash_is_reported_not_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rc-hash-mismatch-{}.dll", std::process::id()));
        std::fs::write(&path, b"not the verified driver").expect("写临时文件");

        let (sha, status) = driver_hash_status(&path);
        let actual = sha.clone().expect("文件可读，哈希应能算出");
        assert_eq!(actual, sha256_hex(b"not the verified driver"));
        match &status {
            DriverHashStatus::Mismatch { expected, actual: got } => {
                assert_eq!(expected, KNOWN_DRIVER_SHA256);
                assert_eq!(got, &actual);
            }
            other => panic!("应为 Mismatch，实际 {other:?}"),
        }
        assert!(!status.is_verified());
        let warning = status.warning().expect("不符时必须给出告警");
        assert!(warning.contains("不一致"), "告警应说明不一致：{warning}");
        assert!(warning.contains(KNOWN_DRIVER_SHA256), "告警应带期望哈希");

        let _ = std::fs::remove_file(&path);
    }

    /// 读不出哈希时是 `Unknown`，不能与「不符」混为一谈。
    #[test]
    fn unreadable_hash_is_unknown_not_mismatch() {
        let path = Path::new(r"C:\no-such-dir\LC32RFRW.dll");
        let (sha, status) = driver_hash_status(path);
        assert_eq!(sha, None);
        assert_eq!(status, DriverHashStatus::Unknown);
        assert!(!status.is_verified());
        let warning = status.warning().expect("读不出也应提示");
        assert!(warning.contains("无法读取"), "告警应说明读不出来：{warning}");
    }

    /// 串口索引越界必须是参数错误。
    #[test]
    fn set_port_rejects_out_of_range() {
        let dev = DirectDllDevice::new("C:/nonexistent-driver-dir");
        let err = dev.set_port(8).expect_err("索引 8 应被拒绝");
        assert_eq!(err.sdk_code(), 1);
    }

    /// 自检是纯离线的：即使驱动目录不存在也要能跑。
    #[test]
    fn selftest_is_offline_and_green() {
        let dev = DirectDllDevice::new("C:/no-such-driver-dir");
        let report = dev.selftest().expect("自检不应依赖驱动");
        assert_eq!(report.total(), 46);
        assert!(report.is_all_green());
    }

    /// 驱动目录不存在时，打开要报「文件缺失」而非含糊的加载失败。
    #[test]
    fn open_without_driver_reports_file_missing() {
        let dev = DirectDllDevice::new("C:/no-such-driver-dir");
        let err = dev.open().expect_err("没有驱动应打不开");
        assert_eq!(err.kind(), "driver_file_missing");
    }
}
