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
use std::sync::{Arc, Mutex};
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
    CLEANUP_TIMEOUT_MS, CONFIG_SETTLE_MS, OPEN_SETTLE_MS, OPEN_TIMEOUT_MS, PROBE_PORT_TIMEOUT_MS,
    READ_TIMEOUT_MS, RESHAKE_DELAY_MS, SETUP_PWM_DELAY_MS, VERIFY_RETRY_DELAY_MS,
    WRITE_PAGE1_DELAY_MS, WRITE_PAGE2_DELAY_MS, WRITE_TIMEOUT_MS,
};
use crate::dll::timeout::run_with_timeout;
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
    /// 串行化「打开序列」。
    ///
    /// 打开过程包含多次驱动调用、总耗时约 1 秒，期间不能持有 `state` 锁——
    /// 否则超时机制无法工作（见 [`crate::dll::timeout`]：驱动调用要放到工作线程里，
    /// 而工作线程再去抢 `state` 锁就会死锁）。这里用一把独立的轻量锁串行打开动作。
    open_lock: Mutex<()>,
}

/// 会话状态：已加载的驱动 + 是否已打开 + 当前端口。
///
/// 端口放在状态里而不是 `&self` 的不可变字段上，因为 [`CardDevice::set_port`]
/// 需要在 `&self` 下切换端口。
struct SessionState {
    /// 驱动句柄。**必须是 `Arc`**：超时后工作线程仍卡在驱动里，
    /// 若主线程在这里 drop 掉句柄，`Library` 会被卸载，而那个线程还在里面执行——
    /// 那是崩溃级的未定义行为。`Arc` 让句柄活到最后一个线程退出为止。
    dll: Option<Arc<DllExports>>,
    opened: bool,
    reader_id: Option<String>,
    /// 当前串口索引 0–7。
    port: u8,
    /// 会话是否已变得不可信；`Some(原因)` 时后续调用一律拒绝。
    ///
    /// 只有超时会让会话变成不可信——因为超时意味着「我们不知道驱动做了什么，
    /// 也不知道它会不会返回」。此后继续下发命令，会让两个线程同时操作同一个串口。
    untrusted: Option<String>,
}

impl SessionState {
    fn closed(port: u8) -> Self {
        Self {
            dll: None,
            opened: false,
            reader_id: None,
            port,
            untrusted: None,
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
            open_lock: Mutex::new(()),
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

    /// 会话若已被标记为不可信，返回拒绝原因。
    ///
    /// 超时之后**绝不能**继续下发命令：工作线程可能仍卡在驱动里，
    /// 此时再来一条命令会让两个线程同时操作同一个串口——那是会把卡写坏的情形。
    fn reject_if_untrusted(st: &SessionState) -> DeviceResult<()> {
        match &st.untrusted {
            Some(reason) => Err(DeviceError::SessionUntrusted(format!(
                "{reason}。请关闭程序后重新启动——继续操作可能把卡写坏。"
            ))),
            None => Ok(()),
        }
    }

    /// 把会话标记为不可信。只在驱动调用超时后调用。
    fn mark_untrusted(&self, reason: &str) {
        if let Ok(mut st) = self.state.lock() {
            st.untrusted = Some(reason.to_string());
            // 不再相信串口状态：既不能读也不能写
            st.opened = false;
        }
    }

    /// 取驱动句柄，必要时加载。返回 `Arc` 以便跨线程使用。
    ///
    /// 加载动作本身也可能卡住（`LoadLibrary` 要读文件），但它发生在持有 `state` 锁时，
    /// 无法用超时机制包住——这是已知的残余风险，已记录在 TD-018 的「遗留」里。
    fn dll_handle(&self) -> DeviceResult<Arc<DllExports>> {
        let mut st = self.lock()?;
        Self::reject_if_untrusted(&st)?;
        if let Some(dll) = &st.dll {
            return Ok(Arc::clone(dll));
        }
        let dll = Arc::new(DllExports::load(&self.driver_path())?);
        st.dll = Some(Arc::clone(&dll));
        Ok(dll)
    }

    /// 确保驱动已加载且串口已打开，返回可跨线程使用的驱动句柄。
    ///
    /// # 为什么返回 `Arc` 而不是 `MutexGuard`
    ///
    /// 打开序列本身要在工作线程里跑（带超时），工作线程必须是 `'static`。
    /// `MutexGuard` 借用 `&self`，跨不了线程；`Arc<DllExports>` 可以克隆进去。
    ///
    /// 这也顺带避免了「持锁做驱动调用」——那样超时机制根本无法生效。
    fn ensure_open(&self) -> DeviceResult<Arc<DllExports>> {
        // 快速路径：已打开就直接复用
        {
            let st = self.lock()?;
            Self::reject_if_untrusted(&st)?;
            if st.opened {
                if let Some(dll) = &st.dll {
                    return Ok(Arc::clone(dll));
                }
            }
        }

        // 打开动作串行化，避免两个线程同时打开同一个串口
        let _open_guard = self
            .open_lock
            .lock()
            .map_err(|_| DeviceError::Io("打开锁已中毒，请重启程序".to_string()))?;

        // 二次检查：等锁期间别人可能已经打开了
        let port = {
            let st = self.lock()?;
            Self::reject_if_untrusted(&st)?;
            if st.opened {
                if let Some(dll) = &st.dll {
                    return Ok(Arc::clone(dll));
                }
            }
            st.port
        };

        let dll = self.dll_handle()?;
        let dll_for_thread = Arc::clone(&dll);

        // 打开序列整体带超时：里面有三段 sleep 与四次驱动调用
        let outcome = run_with_timeout("打开读卡器", OPEN_TIMEOUT_MS, move || {
            open_sequence(&dll_for_thread, port)
        });

        match outcome {
            Ok(reader_id) => {
                let mut st = self.lock()?;
                st.opened = true;
                st.reader_id = reader_id;
                Ok(dll)
            }
            Err(e @ DeviceError::Timeout(_)) => {
                // 超时：驱动可能仍卡在工作线程里，串口状态未知
                self.mark_untrusted("打开读卡器超时");
                Err(e)
            }
            Err(e) => {
                // 正常失败（无卡、握手失败等）：驱动已返回，句柄仍然可信
                let mut st = self.lock()?;
                st.dll = None;
                Err(e)
            }
        }
    }

    /// 读卡的原始 28 字节（带超时）。
    fn read_raw(&self, dll: &Arc<DllExports>) -> DeviceResult<[u8; CARD_BYTES]> {
        let dll = Arc::clone(dll);
        let outcome = run_with_timeout("读卡", READ_TIMEOUT_MS, move || read_sequence(&dll));
        match outcome {
            Err(e @ DeviceError::Timeout(_)) => {
                self.mark_untrusted("读卡超时");
                Err(e)
            }
            other => other,
        }
    }

    /// 写卡核心：设置密码 → 写页1 → 写页2 → 回读校验。
    ///
    /// 页数据已由 `builder` 构造好，这里只负责"把它写进卡并确认写进去了"。
    ///
    /// # 整条序列作为一个整体超时
    ///
    /// **不能**把每个驱动调用分别超时：那样一旦中途超时，我们既不知道写到哪一步，
    /// 又还能继续下发后续命令。整条写卡序列放进一个工作线程、共享一个总超时，
    /// 中途卡死就整体失败并隔离会话——这才是安全的行为。
    fn write_pages(
        &self,
        p1: [u8; 4],
        p2: [u8; 4],
        room_index: Option<u16>,
    ) -> DeviceResult<CardSnapshot> {
        let dll = self.ensure_open()?;

        let outcome = run_with_timeout("写卡", WRITE_TIMEOUT_MS, move || {
            write_sequence(&dll, p1, p2)
        });

        let raw = match outcome {
            Ok(raw) => raw,
            Err(e @ DeviceError::Timeout(_)) => {
                // 卡可能只写了一半，且驱动可能仍卡着。隔离会话，等人工处理。
                self.mark_untrusted("写卡超时，卡内容未知");
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        let mut snap = parser::parse(&raw)?;
        if let Some(idx) = room_index {
            snap.room_index = Some(idx);
        }
        Ok(snap)
    }
}

/// 打开序列：打开串口 → 握手 → 明文模式 → 选卡。
///
/// 整段作为**一个工作线程任务**执行，由 [`run_with_timeout`] 兜底超时。
/// 这就是为什么它是一个自由函数而不是 `DirectDllDevice` 的方法：
/// 工作线程要求 `'static`，方法借用 `&self` 跨不过去。
///
/// 返回读卡器序列号（取不到时为 `None`，不致命）。
fn open_sequence(dll: &DllExports, port: u8) -> DeviceResult<Option<String>> {
    if dll.open_port(i32::from(port), i32::try_from(SERIAL_BAUD).unwrap_or(i32::MAX)) != 0 {
        return Err(DeviceError::PortOpenFailed { port });
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
        // 握手都没成功时，更可能是读卡器没连好而不是没放卡
        if !handshake_ok {
            return Err(DeviceError::HandshakeFailed);
        }
        return Err(DeviceError::NoCard);
    }

    Ok(dll.reader_id())
}

/// 读一次卡的原始 28 字节。
///
/// 同一份逻辑既给 [`CardDevice::read_card`] 用，也给写卡后的回读校验用——
/// 两处必须走完全相同的路径，否则「读卡能读、校验读不出」这种偏差会很难查。
fn read_sequence(dll: &DllExports) -> DeviceResult<[u8; CARD_BYTES]> {
    let (rc, hex) = dll.read_card_hex();
    if rc != 0 {
        return Err(DeviceError::ReadFailed(format!("驱动返回 {rc}")));
    }
    parse_hex28(&hex)
}

/// 整条写卡序列：设置密码 → 写页1 → 写页2 → 回读校验 → 蜂鸣。
///
/// 返回**读回的** 28 字节（校验通过的那一次），供调用方解析。
///
/// # 为什么整条序列一起超时
///
/// 序列内部的每一步都依赖上一步的结果（没设密码写不了页，没写页2 无法校验）。
/// 若按步单独超时，中途卡住时我们既不知道进行到哪一步，又还能继续下发后续命令。
/// 所以整条序列放进一个工作线程、共享一个总超时：卡住就整体失败并隔离会话。
fn write_sequence(
    dll: &DllExports,
    p1: [u8; 4],
    p2: [u8; 4],
) -> DeviceResult<[u8; CARD_BYTES]> {
    let pwd = c_string_bytes(WRITE_PASSWORD);

    if dll.setup_write_password(&pwd) != 0 {
        return Err(DeviceError::WriteFailed("设置写密码失败".to_string()));
    }
    sleep_ms(SETUP_PWM_DELAY_MS);

    if dll.write_page(1, &pwd, &c_string_bytes(&page_to_hex(p1))) != 0 {
        return Err(DeviceError::WriteFailed("写页1 失败".to_string()));
    }
    sleep_ms(WRITE_PAGE1_DELAY_MS);

    if dll.write_page(2, &pwd, &c_string_bytes(&page_to_hex(p2))) != 0 {
        return Err(DeviceError::WriteFailed("写页2 失败".to_string()));
    }
    sleep_ms(WRITE_PAGE2_DELAY_MS);

    // 回读校验：写完立刻读可能拿到旧值（实测结论），所以最多试 3 次。
    // 重试是**协议的一部分**，不是容错；整次写卡绝不因此重来。
    let raw = verify_written(
        || read_sequence(dll),
        p1,
        p2,
        VERIFY_ATTEMPTS,
        VERIFY_RETRY_DELAY_MS,
    )?;

    // 只有校验通过才响——写失败时蜂鸣会让前台误以为成功了
    dll.beep_times(2);
    Ok(raw)
}

/// 回读校验：反复读卡，直到页1/页2 与刚写入的一致，或次数用尽。
///
/// # 为什么抽成独立函数
///
/// 这段逻辑是整条写卡流程里**唯一有分支**的地方，而它偏偏依赖真实驱动
/// （`read_raw` 从会话句柄取数据）。抽出来以后，测试只需传一个闭包就能构造
/// 「前两次拿到旧值、第三次才刷新」这种场景——那正是真机上最常遇到的。
///
/// # 参数
///
/// - `read`：读一次卡的闭包。生产代码传真实驱动，测试传脚本化的假实现。
/// - `p1` / `p2`：刚写进去的页1、页2，用来和读回的结果逐字节比对。
/// - `attempts`：最多读几次（协议值 [`VERIFY_ATTEMPTS`]，此处可注入便于测试）。
/// - `retry_delay_ms`：两次读之间的间隔；测试传 0，不白等。
///
/// # 返回
///
/// 成功时给出**读回的那 28 字节**（不是刚写入的 p1/p2）——后续解析要用它，
/// 这样「写入的」和「读回的」若有差异会立刻暴露在解析结果里。
///
/// # 读卡报错的处理
///
/// 读卡失败（驱动返回非零）与「读到了但内容还是旧值」都只是继续重试：
/// 卡刚写完时驱动可能短暂报错。次数用尽后统一报 [`DeviceError::VerifyFailed`]，
/// 因为此时**已经不知道卡里是什么**了，这比报 `ReadFailed` 更贴近事实。
fn verify_written<F>(
    mut read: F,
    p1: [u8; 4],
    p2: [u8; 4],
    attempts: u32,
    retry_delay_ms: u64,
) -> DeviceResult<[u8; CARD_BYTES]>
where
    F: FnMut() -> DeviceResult<[u8; CARD_BYTES]>,
{
    for attempt in 0..attempts {
        if let Ok(raw) = read() {
            if raw[0..4] == p1 && raw[4..8] == p2 {
                return Ok(raw);
            }
        }
        // 最后一次失败后不再等待，直接返回，别让前台多等一个 retry_delay
        if attempt + 1 < attempts {
            sleep_ms(retry_delay_ms);
        }
    }
    Err(DeviceError::VerifyFailed)
}

impl CardDevice for DirectDllDevice {
    fn open(&self) -> DeviceResult<()> {
        // `ensure_open` 内部已把会话置为 opened 并记录读卡器序列号，
        // 这里只需要驱动一次打开动作、丢弃句柄。
        let _dll = self.ensure_open()?;
        Ok(())
    }

    fn close(&self) -> DeviceResult<()> {
        // 先把状态改掉、取出句柄，**不持锁做驱动调用**——
        // 持锁调用正是 TD-018 要消除的模式：驱动卡住时会把所有后续调用一起堵死。
        let dll = {
            let mut st = self.lock()?;
            let dll = st.dll.as_ref().map(Arc::clone);
            st.opened = false;
            st.reader_id = None;
            // 保留 dll 句柄以便快速重开；真正卸载要 drop
            dll
        };

        let Some(dll) = dll else {
            return Ok(());
        };

        match run_with_timeout("关闭串口", CLEANUP_TIMEOUT_MS, move || {
            dll.close_port();
            Ok(())
        }) {
            Ok(()) => Ok(()),
            Err(e @ DeviceError::Timeout(_)) => {
                // 关闭都能卡住，说明驱动状态已经不可信
                self.mark_untrusted("关闭串口超时");
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    fn is_open(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.opened)
    }

    fn read_card(&self) -> DeviceResult<CardSnapshot> {
        let dll = self.ensure_open()?;
        let raw = self.read_raw(&dll)?;
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

        // 同样先改状态再关旧串口，避免持锁调用驱动
        let dll = {
            let mut st = self.lock()?;
            let dll = st.dll.as_ref().map(Arc::clone);
            st.opened = false;
            st.reader_id = None;
            st.port = port;
            dll
        };

        // 换端口前先关掉旧串口，避免占着不放
        if let Some(dll) = dll {
            // 超时要隔离会话；其余失败都不阻止换端口——新端口会重新打开，
            // 旧句柄关不掉不影响这个结果
            if let Err(e @ DeviceError::Timeout(_)) =
                run_with_timeout("关闭串口", CLEANUP_TIMEOUT_MS, move || {
                    dll.close_port();
                    Ok(())
                })
            {
                self.mark_untrusted("换端口时关闭串口超时");
                return Err(e);
            }
        }
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
///
/// 整个探测序列带超时：`probe_ports` 要挨个试 8 个端口，某个端口上的读卡器
/// 若半死不活（插着但固件卡死），没有超时就会让「探测」本身变成不可用的操作。
fn probe_port_once(driver_dir: &Path, port: u8) -> (bool, Option<String>, Option<String>) {
    let path = driver_dir.join(DRIVER_FILE);
    let dll = match DllExports::load(&path) {
        Ok(d) => Arc::new(d),
        Err(e) => return (false, None, Some(e.to_string())),
    };

    let dll_for_thread = Arc::clone(&dll);
    match run_with_timeout("探测串口", PROBE_PORT_TIMEOUT_MS, move || {
        Ok(probe_sequence(&dll_for_thread, port))
    }) {
        Ok(outcome) => outcome,
        Err(DeviceError::Timeout(_)) => (
            false,
            None,
            Some(format!(
                "串口 COM{} 探测超时（{PROBE_PORT_TIMEOUT_MS} ms 无响应）",
                u16::from(port) + 1
            )),
        ),
        Err(e) => (false, None, Some(e.to_string())),
    }
}

/// 在已加载的驱动上跑一次探测序列。**不带超时**，由调用方包。
fn probe_sequence(
    dll: &DllExports,
    port: u8,
) -> (bool, Option<String>, Option<String>) {
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

    // ------------------------------------------------- 超时与会话隔离（TD-018）

    /// 新会话是可信的，`reject_if_untrusted` 放行。
    #[test]
    fn fresh_session_is_trusted() {
        let st = SessionState::closed(2);
        assert!(st.untrusted.is_none());
        assert!(DirectDllDevice::reject_if_untrusted(&st).is_ok());
    }

    /// 标记不可信之后，后续调用必须被拒绝，且错误文案要求重启程序。
    ///
    /// 这是 TD-018 最关键的行为：超时后驱动可能仍卡在工作线程里，
    /// 此时继续下发命令会让两个线程同时操作同一个串口。
    #[test]
    fn untrusted_session_rejects_further_calls() {
        let st = SessionState {
            untrusted: Some("读卡超时".to_string()),
            ..SessionState::closed(2)
        };

        let err = DirectDllDevice::reject_if_untrusted(&st)
            .expect_err("不可信会话必须拒绝调用");

        assert_eq!(err.kind(), "session_untrusted");
        assert_eq!(err.sdk_code(), -1, "这不是 SDK 语义码");
        assert!(err.is_device_fault(), "应被当作设备故障标红");
        let text = err.to_string();
        assert!(text.contains("读卡超时"), "应带原始超时原因：{text}");
        assert!(text.contains("重新启动") || text.contains("关闭程序"), "应告诉用户怎么办：{text}");
        assert!(err.hint().contains("卡写坏"), "提示要说清风险：{}", err.hint());
    }

    /// 标记不可信时必须同时把 `opened` 置 false——串口状态已不可信。
    #[test]
    fn marking_untrusted_clears_opened() {
        let dev = DirectDllDevice::new("C:/nonexistent-driver-dir");
        {
            let mut st = dev.state.lock().expect("取锁");
            st.opened = true;
            st.reader_id = Some("SN1".to_string());
        }
        assert!(dev.is_open());

        dev.mark_untrusted("写卡超时，卡内容未知");

        assert!(!dev.is_open(), "不可信之后不该再认为串口是打开的");
        let st = dev.state.lock().expect("取锁");
        assert!(st.untrusted.is_some());
        assert!(st
            .untrusted
            .as_deref()
            .is_some_and(|r| r.contains("写卡超时")));
    }

    /// 已标记不可信的设备，任何读写都必须先失败，不再碰驱动。
    ///
    /// 用不存在的驱动目录构造：若真去碰驱动，会先报 `DriverFileMissing`。
    /// 现在应先报 `SessionUntrusted`，证明拒绝检查发生在驱动调用之前。
    #[test]
    fn untrusted_device_fails_before_touching_driver() {
        let dev = DirectDllDevice::new("C:/nonexistent-driver-dir");
        dev.mark_untrusted("打开读卡器超时");

        let err = dev.read_card().expect_err("不可信设备不能读卡");
        assert_eq!(
            err.kind(),
            "session_untrusted",
            "应报会话失效，而不是驱动缺失：{err}"
        );

        let err = dev
            .write_install(&InstallSpec { room_index: 1 })
            .expect_err("不可信设备不能写卡");
        assert_eq!(err.kind(), "session_untrusted");
    }

    /// **端到端**：永不返回的假驱动调用 → 超时 → 会话隔离 → 后续调用被拒。
    ///
    /// 前一刻的行为来自 [`run_with_timeout`]，后一刻来自 `mark_untrusted` +
    /// `reject_if_untrusted`。这里把两端接起来验证，因为「超时了但没人去隔离」
    /// 正是这个机制最容易出现的失效方式——单看任何一半都是对的。
    #[test]
    fn timeout_poisons_session_and_blocks_follow_up() {
        let dev = DirectDllDevice::new("C:/nonexistent-driver-dir");

        // 1) 假驱动调用：永不返回（模拟卡在 DLL 里）
        let err = run_with_timeout("读卡", 80, || -> DeviceResult<[u8; CARD_BYTES]> {
            std::thread::sleep(Duration::from_secs(5));
            Ok([0u8; CARD_BYTES])
        })
        .expect_err("永不返回必须超时");
        assert!(matches!(err, DeviceError::Timeout(_)));

        // 2) 按约定隔离会话
        dev.mark_untrusted("读卡超时");

        // 3) 后续调用一律拒绝，且拒绝发生在碰驱动之前
        for label in ["读卡", "写安装卡"] {
            let e = if label == "读卡" {
                dev.read_card().expect_err("不可信会话不能读")
            } else {
                dev.write_install(&InstallSpec { room_index: 1 })
                    .expect_err("不可信会话不能写")
            };
            assert_eq!(e.kind(), "session_untrusted", "{label} 应被拒绝：{e}");
        }

        // 4) 会话仍然不可信——隔离是持久的，不会自己恢复
        assert!(dev
            .state
            .lock()
            .expect("取锁")
            .untrusted
            .as_deref()
            .is_some_and(|r| r.contains("读卡超时")));
    }

    // ---------------------------------------------------------- 回读校验（TD-005）

    /// 回读校验用的 28 字节：页1/页2 为指定值，其余补零。
    fn raw_of(p1: [u8; 4], p2: [u8; 4]) -> [u8; CARD_BYTES] {
        let mut raw = [0u8; CARD_BYTES];
        raw[0..4].copy_from_slice(&p1);
        raw[4..8].copy_from_slice(&p2);
        raw
    }

    /// 新写入的页（期望值）。
    fn fresh() -> ([u8; 4], [u8; 4]) {
        ([0x5B, 0xEC, 0x18, 0x01], [0x46, 0xF2, 0x00, 0x01])
    }

    /// 卡里原有内容的旧值（与期望不同）。
    fn stale() -> ([u8; 4], [u8; 4]) {
        ([0x5B, 0xEC, 0x00, 0x00], [0x80, 0x01, 0x00, 0xAB])
    }

    /// 第一次就读对了 → 立即返回，不该有多余读取。
    ///
    /// 发卡台是高频操作，每次写卡都白等是不必要的。
    #[test]
    fn verify_returns_immediately_when_first_read_matches() {
        let (p1, p2) = fresh();
        let mut calls = 0;
        let got = verify_written(
            || {
                calls += 1;
                Ok(raw_of(p1, p2))
            },
            p1,
            p2,
            3,
            0,
        )
        .expect("第一次就对上，应当成功");

        assert_eq!(calls, 1, "对上之后不该再读");
        assert_eq!(&got[0..8], &raw_of(p1, p2)[0..8]);
    }

    /// **核心场景**：前两次读到旧值，第三次才刷新 → 成功。
    ///
    /// 这是真机上最常遇到的情况：写完立刻读会拿到卡里原来的内容。
    /// 这条测试此前写不出来，因为回读循环直接取驱动句柄。
    #[test]
    fn verify_succeeds_after_two_stale_reads() {
        let (p1, p2) = fresh();
        let (s1, s2) = stale();
        let mut calls = 0;

        let got = verify_written(
            || {
                calls += 1;
                if calls < 3 {
                    Ok(raw_of(s1, s2))
                } else {
                    Ok(raw_of(p1, p2))
                }
            },
            p1,
            p2,
            3,
            0,
        )
        .expect("第三次应读到新值并成功");

        assert_eq!(calls, 3, "应当恰好读 3 次");
        assert_eq!(&got[0..8], &raw_of(p1, p2)[0..8]);
    }

    /// 三次都读到旧值 → `VerifyFailed`。
    ///
    /// 这条错误分支此前从未被执行过。它是「卡可能写了一半」的信号，
    /// 前台必须人工介入，所以行为必须确定。
    #[test]
    fn verify_fails_after_all_attempts_are_stale() {
        let (p1, p2) = fresh();
        let (s1, s2) = stale();
        let mut calls = 0;

        let err = verify_written(
            || {
                calls += 1;
                Ok(raw_of(s1, s2))
            },
            p1,
            p2,
            VERIFY_ATTEMPTS,
            0,
        )
        .expect_err("一直读到旧值应当失败");

        assert_eq!(calls, VERIFY_ATTEMPTS, "读满次数后必须停手，绝不自动重试写卡");
        assert_eq!(err, DeviceError::VerifyFailed);
        assert_eq!(err.sdk_code(), 8);
    }

    /// 读卡一直报错 → 同样报 `VerifyFailed`。
    ///
    /// 此时已经不知道卡里是什么，报 `VerifyFailed` 比报 `ReadFailed` 更贴近事实，
    /// 处理建议也是「把卡放稳重试；多次失败换一张卡」。
    #[test]
    fn verify_treats_read_errors_like_stale_reads() {
        let (p1, p2) = fresh();
        let mut calls = 0;

        let err = verify_written(
            || {
                calls += 1;
                Err(DeviceError::ReadFailed("驱动返回 6".to_string()))
            },
            p1,
            p2,
            VERIFY_ATTEMPTS,
            0,
        )
        .expect_err("读卡全失败应当报校验失败");

        assert_eq!(calls, VERIFY_ATTEMPTS);
        assert_eq!(err, DeviceError::VerifyFailed);
    }

    /// 只有页1 对上、页2 还是旧值 → 不算成功。
    ///
    /// 协议要求两页都写进去。只写了一半的卡必须报失败，
    /// 否则会产出一张「读得出但打不开门」的卡。
    #[test]
    fn verify_requires_both_pages_to_match() {
        let (p1, p2) = fresh();
        let (_, s2) = stale();

        let err = verify_written(
            || Ok(raw_of(p1, s2)),
            p1,
            p2,
            VERIFY_ATTEMPTS,
            0,
        )
        .expect_err("页2 不符不能算成功");

        assert_eq!(err, DeviceError::VerifyFailed);
    }

    /// 一次也不读时（attempts = 0）直接失败，不进入任何读卡动作。
    ///
    /// 防的是调用方把 `VERIFY_ATTEMPTS` 误配成 0 却静默返回成功。
    #[test]
    fn verify_with_zero_attempts_fails_without_reading() {
        let (p1, p2) = fresh();
        let mut calls = 0;

        let err = verify_written(
            || {
                calls += 1;
                Ok(raw_of(p1, p2))
            },
            p1,
            p2,
            0,
            0,
        )
        .expect_err("0 次尝试必然是失败");

        assert_eq!(calls, 0);
        assert_eq!(err, DeviceError::VerifyFailed);
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
