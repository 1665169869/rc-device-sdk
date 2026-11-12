//! 本机华视身份证读卡服务（`ws://127.0.0.1:9001`）的 [`IdCardReader`] 实现。
//!
//! # 为什么是同步实现
//!
//! 设备层整体是「同步阻塞 + `Mutex` 串行」的风格（见 [`crate::dll`]）。
//! 为了一个本机回环连接拉进 tokio 运行时，会让整个 crate 的异步边界变得含糊，
//! 也会让 Tauri 侧的调用约定分裂成两套。`tungstenite` 的同步 API 正好够用。
//!
//! # 实测协议事实（2026-11-13 抓包，服务 `CVR100UWebSocker.exe`）
//!
//! | 操作 | 实际行为 |
//! |---|---|
//! | 连接后不发言 | 服务**不主动推送**，连接一直挂着 |
//! | 发送 `04?timeout=N` | 约 350 ms 内回一条文本 |
//! | 读卡器上没证件 | 回文本 `读卡失败`，**连接不关闭**、也不推第二条 |
//! | 发送未知命令 `01` | 4 秒内无任何响应 |
//!
//! 「读卡失败」这条最关键：它意味着**不能**用「解析不出就继续等」的思路实现，
//! 否则用户要白等满整个超时才知道没读到。所以 [`classify`] 把它判成
//! [`IdCardResponse::ServiceFailure`]，由本模块立刻上报。
//!
//! # 连接生命周期
//!
//! **每次读卡新建连接、读完立即断开**。服务不会主动断，若复用连接就会一直占着
//! 9001 端口的会话——而厂商软件有时需要独占读卡器。短连接麻烦一点，但不会让
//! 别人连不上。

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use tungstenite::{Message, WebSocket};

use crate::error::{DeviceError, DeviceResult};
use crate::idcard::{
    IdCardData, IdCardProbeSpec, IdCardReadSpec, IdCardReader, IdCardServiceInfo,
};
use crate::idcard_proto::{classify, IdCardResponse};

/// 默认服务地址。
pub const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:9001";

/// 读卡命令字。
///
/// `04` = **不拿起模式**：读卡器不会被持续独占，读完就释放。
/// `01` 是拿起模式，会让读卡器一直占用到证件被拿走。
const READ_COMMAND: &str = "04";

/// 单条文本帧的字节上限。
///
/// 正常情况下报文只有几百字节（头像在几十 KB 量级，我们也不读它）。
/// 设上限是为了防止异常服务灌进来一个巨大帧把内存吃爆。
const MAX_FRAME_BYTES: usize = 512 * 1024;

/// 华视身份证读卡器（WebSocket 版）。
#[derive(Debug, Clone)]
pub struct HsWsIdCardReader {
    endpoint: String,
}

impl Default for HsWsIdCardReader {
    fn default() -> Self {
        Self::new()
    }
}

impl HsWsIdCardReader {
    /// 用默认地址 `ws://127.0.0.1:9001` 构造。
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    /// 指定服务地址构造（现场若改过端口或换了机器）。
    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    /// 当前使用的服务地址。
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 在给定截止时间前完成 TCP 连接 + WebSocket 握手。
    ///
    /// 返回握手响应头里的 `Server` 字段（若有），供 [`Self::probe`] 上报版本特征。
    fn connect(&self, deadline: Instant) -> DeviceResult<(WebSocket<TcpStream>, Option<String>)> {
        let addr = resolve(&self.endpoint)?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DeviceError::IdCardService(format!(
                "连接 {} 前已超时",
                self.endpoint
            )));
        }

        let stream = TcpStream::connect_timeout(&addr, remaining).map_err(|e| {
            DeviceError::IdCardService(format!(
                "无法连接身份证读卡服务 {}：{e}。请确认华视读卡服务已启动并占用该端口。",
                self.endpoint
            ))
        })?;

        // 读写都受截止时间约束：服务不回话时不能在这里永久卡住
        let _ = stream.set_read_timeout(Some(remaining));
        let _ = stream.set_write_timeout(Some(remaining));
        let _ = stream.set_nodelay(true);

        let (ws, response) = tungstenite::client(self.endpoint.as_str(), stream).map_err(|e| {
            DeviceError::IdCardService(format!(
                "身份证读卡服务握手失败（{}）：{e}",
                self.endpoint
            ))
        })?;

        let server = response
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        Ok((ws, server))
    }

    /// 发命令并等到「有意义的回应」，心跳类内容忽略后继续等。
    fn await_response(
        &self,
        ws: &mut WebSocket<TcpStream>,
        command: &str,
        deadline: Instant,
    ) -> DeviceResult<IdCardResponse> {
        ws.send(Message::Text(command.to_string().into()))
            .map_err(|e| {
                DeviceError::IdCardService(format!("向身份证读卡服务发送读卡命令失败：{e}"))
            })?;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.timeout_error());
            }
            // 每次读之前把超时收紧到剩余时间，避免最后一次读超出截止时间太多
            let _ = ws.get_ref().set_read_timeout(Some(remaining));

            let text = match ws.read() {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) => {
                    return Err(DeviceError::IdCardService(format!(
                        "身份证读卡服务主动断开了连接（{}）",
                        self.endpoint
                    )))
                }
                // 二进制帧与 Ping/Pong 不在已知协议里，当作心跳忽略
                Ok(_) => continue,
                Err(e) => return Err(self.map_read_error(&e)),
            };

            if text.len() > MAX_FRAME_BYTES {
                return Err(DeviceError::IdCardService(format!(
                    "身份证读卡服务返回的报文过大（{} 字节）",
                    text.len()
                )));
            }

            // 心跳或无关内容落空，自然进入下一轮继续等；有意义的应答才返回。
            // 这里不用 `continue`：它是循环体最后一条语句，多余的 continue 会被
            // clippy::needless_continue 拒绝。
            match classify(text.as_str()) {
                IdCardResponse::Heartbeat => {}
                other => return Ok(other),
            }
        }
    }

    /// 超时错误。文案要告诉前台「该做什么」，而不是只报「超时」。
    fn timeout_error(&self) -> DeviceError {
        DeviceError::IdCardService(format!(
            "读取身份证超时（{}）。{}",
            self.endpoint,
            crate::idcard_proto::no_card_hint()
        ))
    }

    /// 读失败时的错误映射。区分「超时」与「连接被断」，两者现场处理不一样。
    fn map_read_error(&self, e: &tungstenite::Error) -> DeviceError {
        match e {
            tungstenite::Error::Io(io) => match io.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => self.timeout_error(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe => {
                    DeviceError::IdCardService(format!(
                        "身份证读卡服务在读取过程中断开了连接（{}）",
                        self.endpoint
                    ))
                }
                _ => DeviceError::IdCardService(format!("读取身份证时发生 I/O 错误：{io}")),
            },
            tungstenite::Error::ConnectionClosed | tungstenite::Error::Protocol(_) => {
                DeviceError::IdCardService(format!(
                    "身份证读卡服务在读取过程中断开了连接（{}）：{e}",
                    self.endpoint
                ))
            }
            other => DeviceError::IdCardService(format!("读取身份证失败：{other}")),
        }
    }
}

impl IdCardReader for HsWsIdCardReader {
    fn read(&self, spec: &IdCardReadSpec) -> DeviceResult<IdCardData> {
        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
        let (mut ws, _) = self.connect(deadline)?;

        // 把剩余时间告诉服务，让它自己也会提前放弃；不依赖它遵守
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1);
        let command = format!("{READ_COMMAND}?timeout={remaining}");

        let result = self.await_response(&mut ws, &command, deadline);

        // 读完立刻断开，不占端口
        let _ = ws.close(None);

        match result? {
            IdCardResponse::Identity(data) | IdCardResponse::Shell(data) => Ok(data),
            IdCardResponse::ServiceFailure(msg) => Err(DeviceError::IdCardService(format!(
                "身份证读卡服务返回失败：{msg}。{}",
                crate::idcard_proto::no_card_hint()
            ))),
            // 走到这里说明循环没拿到有意义内容就退出了，实际不可达
            IdCardResponse::Heartbeat => Err(self.timeout_error()),
        }
    }

    fn probe(&self, spec: &IdCardProbeSpec) -> DeviceResult<IdCardServiceInfo> {
        let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);

        match self.connect(deadline) {
            Ok((mut ws, server)) => {
                let _ = ws.close(None);
                Ok(IdCardServiceInfo {
                    endpoint: self.endpoint.clone(),
                    reachable: true,
                    // 只做了握手，没读卡，因此拿不到设备型号；
                    // 有 Server 头就报它，没有就老实说 None——不瞎猜版本。
                    service_version: server,
                    failure: None,
                })
            }
            Err(e) => Ok(IdCardServiceInfo {
                endpoint: self.endpoint.clone(),
                reachable: false,
                service_version: None,
                failure: Some(e.to_string()),
            }),
        }
    }
}

/// 从 `ws://host:port` 解析出套接字地址。
///
/// 只支持本项目需要的形态（`ws://` + 主机 + 端口），不做通用 URL 解析——
/// 引入完整的 `url` crate 为了解析一个固定格式的地址不划算。
fn resolve(endpoint: &str) -> DeviceResult<SocketAddr> {
    let rest = endpoint.strip_prefix("ws://").ok_or_else(|| {
        DeviceError::IdCardService(format!(
            "服务地址必须以 ws:// 开头，实际 {endpoint}"
        ))
    })?;

    // 去掉可能存在的路径部分
    let authority = rest.split('/').next().unwrap_or(rest);

    let mut addrs = authority.to_socket_addrs().map_err(|e| {
        DeviceError::IdCardService(format!("无法解析服务地址 {authority}：{e}"))
    })?;

    addrs.next().ok_or_else(|| {
        DeviceError::IdCardService(format!("服务地址 {authority} 未解析出任何 IP"))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// 地址解析：正常形态要能解析出回环地址与端口。
    #[test]
    fn resolve_parses_default_endpoint() {
        let addr = resolve(DEFAULT_ENDPOINT).expect("默认地址必须能解析");
        assert_eq!(addr.port(), 9001);
        assert!(addr.ip().is_loopback());
    }

    /// 带路径的地址也能解析（只取 authority 部分）。
    #[test]
    fn resolve_ignores_path() {
        let addr = resolve("ws://127.0.0.1:9100/ws").expect("应能解析");
        assert_eq!(addr.port(), 9100);
    }

    /// 非 `ws://` 前缀要给出明确错误，而不是含糊的解析失败。
    #[test]
    fn resolve_rejects_non_ws_scheme() {
        let err = resolve("http://127.0.0.1:9001").expect_err("应拒绝");
        assert!(err.to_string().contains("ws://"), "错误应说明要求的前缀");
    }

    /// 连不上的端口要立刻报 `IdCardService`，且提示里带端口号。
    ///
    /// 用一个几乎不可能被监听的端口，避免与真实服务冲突。
    #[test]
    fn unreachable_service_reports_endpoint() {
        let reader = HsWsIdCardReader::with_endpoint("ws://127.0.0.1:1");
        let err = reader
            .read(&IdCardReadSpec { timeout_ms: 500 })
            .expect_err("端口 1 不应有服务");
        assert!(matches!(err, DeviceError::IdCardService(_)));
        assert!(err.to_string().contains("ws://127.0.0.1:1"), "提示应带地址");
    }

    /// 探测不可用服务时**不报错**，而是返回 `reachable=false` 与失败原因。
    ///
    /// 探测是给设置页用的：服务没启动是正常状态，不该让页面弹错误。
    #[test]
    fn probe_reports_unreachable_without_error() {
        let reader = HsWsIdCardReader::with_endpoint("ws://127.0.0.1:1");
        let info = reader
            .probe(&IdCardProbeSpec { timeout_ms: 500 })
            .expect("探测本身不应失败");
        assert!(!info.reachable);
        assert!(info.failure.is_some());
        assert_eq!(info.endpoint, "ws://127.0.0.1:1");
        assert!(info.service_version.is_none(), "连不上就没有版本信息");
    }

    /// 构造器要能改地址，默认是 9001。
    #[test]
    fn endpoint_is_configurable() {
        assert_eq!(HsWsIdCardReader::new().endpoint(), DEFAULT_ENDPOINT);
        assert_eq!(
            HsWsIdCardReader::with_endpoint("ws://10.0.0.2:9100").endpoint(),
            "ws://10.0.0.2:9100"
        );
    }

    // ------------------------------------------------------ 假服务端场景测试

    /// 起一个本地假服务端：握完手、收到一条命令后，按脚本依次回文本。
    ///
    /// 脚本为空表示「收到命令后立刻关闭连接」，用来测主动断开。
    /// 绑定 `127.0.0.1:0` 让系统分配空闲端口，避免与真实 9001 冲突。
    fn spawn_fake_server(
        script: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定随机端口");
        let port = listener.local_addr().expect("取本地地址").port();

        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            // 防止测试进程结束时线程还挂在这里
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let Ok(mut ws) = tungstenite::accept(stream) else {
                return;
            };
            // 等客户端发读卡命令——服务不会主动推，这是实测结论
            let _ = ws.read();
            for msg in script {
                if ws.send(Message::Text(msg.to_string().into())).is_err() {
                    return;
                }
            }
            let _ = ws.close(None);
            while ws.read().is_ok() {}
        });

        (format!("ws://127.0.0.1:{port}"), handle)
    }

    /// 起一个「收命令后既不回也不关」的假服务端，用来测客户端超时兜底。
    fn spawn_silent_server() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定随机端口");
        let port = listener.local_addr().expect("取本地地址").port();

        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let Ok(mut ws) = tungstenite::accept(stream) else {
                return;
            };
            let _ = ws.read();
            // 什么都不发，也不关闭，模拟「服务活着但读不到卡且不回话」
            std::thread::sleep(Duration::from_millis(1_500));
        });

        (format!("ws://127.0.0.1:{port}"), handle)
    }

    /// 正常收到 `|` 报文时返回身份信息。
    #[test]
    fn reads_identity_from_fake_service() {
        let sample = "CVR100U|SN123|I|张三|男|汉|19900101|地址|110101199001011234|\
                      机关|20150301|20350301|||||||aGVsbG8=";
        let (endpoint, handle) = spawn_fake_server(vec![sample]);
        let reader = HsWsIdCardReader::with_endpoint(endpoint);

        let data = reader
            .read(&IdCardReadSpec { timeout_ms: 5_000 })
            .expect("应读到身份信息");

        assert_eq!(data.name.as_deref(), Some("张三"));
        assert_eq!(data.gender.as_deref(), Some("男"));
        assert_eq!(data.birth_date.as_deref(), Some("1990-01-01"));
        assert!(data.has_identity());
        let _ = handle.join();
    }

    /// 心跳在前、真数据在后：必须跳过心跳继续等，不能把心跳当成结果返回。
    ///
    /// 这是「读卡器还没放证」时的真实时序：服务先给提示，用户放证后才给数据。
    #[test]
    fn heartbeat_is_skipped_until_real_response() {
        let sample = "CVR100U|SN123|I|李四|女|汉|19851231|上海|310101198512310001|\
                      机关|20100101|20300101|||||||aGVsbG8=";
        let (endpoint, handle) =
            spawn_fake_server(vec!["请将证件放上读卡器", "正在读取", sample]);
        let reader = HsWsIdCardReader::with_endpoint(endpoint);

        let data = reader
            .read(&IdCardReadSpec { timeout_ms: 5_000 })
            .expect("跳过心跳后应读到真数据");

        assert_eq!(data.name.as_deref(), Some("李四"));
        assert!(data.has_identity());
        let _ = handle.join();
    }

    /// 服务直接回 `读卡失败`：立刻报错，**不要**傻等到超时。
    ///
    /// 本机实测就是这个行为。若按「解析不出就继续等」实现，用户要白等满 10 秒。
    #[test]
    fn service_failure_text_fails_fast() {
        let (endpoint, handle) = spawn_fake_server(vec!["读卡失败"]);
        let reader = HsWsIdCardReader::with_endpoint(endpoint);

        let started = Instant::now();
        let err = reader
            .read(&IdCardReadSpec { timeout_ms: 5_000 })
            .expect_err("服务报失败就该失败");
        let elapsed = started.elapsed();

        assert!(matches!(err, DeviceError::IdCardService(_)));
        assert!(
            err.to_string().contains("确认证件已放上读卡器"),
            "错误里要给出该做什么：{err}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "应当在收到失败文本后立即返回，实际耗时 {elapsed:?}"
        );
        let _ = handle.join();
    }

    /// 服务收到命令后立刻断开：报 `IdCardService`，提示连接被断开。
    #[test]
    fn service_closing_connection_is_reported() {
        let (endpoint, handle) = spawn_fake_server(vec![]);
        let reader = HsWsIdCardReader::with_endpoint(endpoint);

        let err = reader
            .read(&IdCardReadSpec { timeout_ms: 5_000 })
            .expect_err("服务断开必须报错");

        assert!(matches!(err, DeviceError::IdCardService(_)));
        let text = err.to_string();
        assert!(
            text.contains("断开") || text.contains("关闭") || text.contains("连接"),
            "错误应说明连接问题：{text}"
        );
        let _ = handle.join();
    }

    /// 服务既不回话也不关闭：客户端必须按超时自己收手，并提示去放证件。
    ///
    /// 这条是实测「服务不主动断开」的直接后果——没有超时兜底，界面会永远转圈。
    #[test]
    fn silent_service_times_out_with_hint() {
        let (endpoint, handle) = spawn_silent_server();
        let reader = HsWsIdCardReader::with_endpoint(endpoint);

        let started = Instant::now();
        let err = reader
            .read(&IdCardReadSpec { timeout_ms: 400 })
            .expect_err("无响应必须超时");
        let elapsed = started.elapsed();

        assert!(matches!(err, DeviceError::IdCardService(_)));
        assert!(
            err.to_string().contains("确认证件已放上读卡器"),
            "超时提示要说清该做什么：{err}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "超时应在设定时间附近收手，实际 {elapsed:?}"
        );
        let _ = handle.join();
    }

    /// 探测真实存在的服务：`reachable=true`，且不读卡。
    #[test]
    fn probe_reports_reachable_service() {
        let (endpoint, handle) = spawn_fake_server(vec![]);
        let reader = HsWsIdCardReader::with_endpoint(endpoint.clone());

        let info = reader
            .probe(&IdCardProbeSpec { timeout_ms: 3_000 })
            .expect("探测应当成功");

        assert!(info.reachable, "假服务端在监听，应判为可达");
        assert_eq!(info.endpoint, endpoint);
        assert!(info.failure.is_none());
        let _ = handle.join();
    }
}
