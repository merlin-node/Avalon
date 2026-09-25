use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

/// 单个节点最多运行的监控数，与 hub 一致。hub 已经截断过，这里是第二道闸。
const MAX_TASKS: usize = 64;
/// 探测线程数。64 个监控、最坏每次 2.7 秒，4 条线程足够跑完最密的 5 秒间隔；
/// 再多只是让被监控的机器替别人跑并发。
const WORKERS: usize = 4;
/// 域名解析预算。超过这个时间这一轮不记录，也不算丢包——慢 DNS 不是目标的锅。
const RESOLVE_BUDGET: Duration = Duration::from_millis(900);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(900);
/// 一个域名解析出多个地址时最多试几个。
const MAX_ADDRESSES: usize = 3;
/// 读超时。主循环靠它醒过来去调度探测和发指标，不需要额外的线程。
const POLL: Duration = Duration::from_millis(400);
/// 心跳：每 10 秒 ping 一次 hub，25 秒听不到任何回音就断开重连。
/// agent 平时只发不收，连接死了只能等写入报错，而家宽换 IP、4G 切基站时旧连接常常不报错，
/// 要等系统的 TCP 超时（默认十几分钟）。有了心跳，大约半分钟内就能发现并重连，
/// 赶在 hub 60 秒的掉线通知之前。
const HEARTBEAT: Duration = Duration::from_secs(10);
const SILENT_LIMIT: Duration = Duration::from_secs(25);

#[derive(Serialize)]
struct Metrics {
    cpu: f64,
    memory: f64,
    disk: f64,
    rx: f64,
    tx: f64,
    load: f64,
    uptime: u64,
    mem_used: u64,
    mem_total: u64,
    swap_used: u64,
    swap_total: u64,
    disk_used: u64,
    disk_total: u64,
    rx_bytes: u64,
    tx_bytes: u64,
    load5: f64,
    load15: f64,
    os: String,
    kernel: String,
    arch: String,
    cpu_model: String,
    cpu_cores: u32,
    latency_ms: Option<f64>,
    /// 排查「hub 升了 agent 没升」时唯一能指望的东西。
    version: &'static str,
    /// 本次开机的 ID。hub 靠它区分「计数器涨了」和「机器重启过、计数器从零开始」，
    /// agent 自己不存任何状态。
    boot: String,
}

/// hub 下发的一个监控任务。
#[derive(Deserialize, Clone)]
struct Task {
    i: i64,
    h: String,
    p: u16,
    s: u64,
}

/// 一次探测的结果。`ms` 为 None 表示丢包。
#[derive(Serialize)]
struct Sample {
    i: i64,
    ms: Option<f64>,
}

#[derive(Serialize)]
#[serde(tag = "t")]
enum Up<'a> {
    #[serde(rename = "m")]
    Metrics(&'a Metrics),
    #[serde(rename = "p")]
    Ping { r: &'a [Sample] },
    /// 本机的公网地址，每族一个。连上时报一次，之后变了再报。
    #[serde(rename = "f")]
    Facts { v4: Option<&'a str>, v6: Option<&'a str> },
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum Down {
    #[serde(rename = "tasks")]
    Tasks { tasks: Vec<Task> },
}

struct SystemInfo { os: String, kernel: String, arch: String, cpu_model: String, cpu_cores: u32, boot: String }

fn system_info() -> SystemInfo {
    let os = fs::read_to_string("/etc/os-release").ok().and_then(|text| text.lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "Linux".into());
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
    let cpu_model = fs::read_to_string("/proc/cpuinfo").ok().and_then(|text| text.lines().find_map(|line| {
        let (key,value) = line.split_once(':')?;
        if matches!(key.trim(), "model name" | "Hardware" | "Processor") { Some(value.trim().to_string()) } else { None }
    })).unwrap_or_default();
    SystemInfo { os: os.chars().take(80).collect(), kernel: kernel.chars().take(80).collect(),
        arch: std::env::consts::ARCH.into(), cpu_model: cpu_model.chars().take(100).collect(),
        cpu_cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        boot: fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().chars().take(64).collect() }
}

struct Counters { total: u64, idle: u64, rx: u64, tx: u64, at: Instant }

fn cpu_counters() -> io::Result<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat")?;
    let values: Vec<u64> = text.lines().next().unwrap_or("").split_whitespace()
        .skip(1).map(str::parse).collect::<Result<_, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if values.len() < 5 { return Err(io::Error::new(io::ErrorKind::InvalidData, "short /proc/stat")); }
    Ok((values.iter().sum(), values[3] + values[4]))
}

fn memory_usage() -> io::Result<(u64,u64,u64,u64)> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let mut total = 0_u64; let mut available = 0_u64; let mut swap_total = 0_u64; let mut swap_free = 0_u64;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => total = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0),
            Some("MemAvailable:") => available = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0),
            Some("SwapTotal:") => swap_total = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0),
            Some("SwapFree:") => swap_free = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0),
            _ => {}
        }
    }
    Ok((total.saturating_sub(available).saturating_mul(1024), total.saturating_mul(1024),
        swap_total.saturating_sub(swap_free).saturating_mul(1024), swap_total.saturating_mul(1024)))
}

fn disk_usage() -> io::Result<(u64,u64)> {
    use std::ffi::CString;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let path = CString::new("/").unwrap();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 { return Err(io::Error::last_os_error()); }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_blocks.saturating_sub(stat.f_bavail).saturating_mul(stat.f_frsize),
        stat.f_blocks.saturating_mul(stat.f_frsize)))
}

/// 只统计真实网卡。判据是 /sys/class/net/<名字> 指向哪里：docker0、veth*、
/// wg0、tailscale0、bond*、ifb* 这些都落在 /devices/virtual/net/ 下面，而云上的
/// virtio 网卡挂在 PCI 路径下。比前缀黑名单准，也不用追着新的虚拟网卡改名单。
fn physical(name: &str) -> bool {
    match fs::read_link(format!("/sys/class/net/{name}")) {
        Ok(target) => !target.to_string_lossy().contains("/devices/virtual/net/"),
        // 没有 sysfs（容器里挂载不全）时退回旧的前缀过滤。
        Err(_) => !["veth", "docker", "br-", "virbr", "tun", "tap", "wg", "zt", "tailscale", "bond", "ifb"]
            .iter().any(|prefix| name.starts_with(prefix)),
    }
}

fn network_counters() -> io::Result<(u64, u64)> {
    let text = fs::read_to_string("/proc/net/dev")?;
    let mut rx = 0_u64; let mut tx = 0_u64;
    for line in text.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || !physical(name) { continue; }
        let cols: Vec<_> = counters.split_whitespace().collect();
        if cols.len() >= 9 {
            rx = rx.saturating_add(cols[0].parse::<u64>().unwrap_or(0));
            tx = tx.saturating_add(cols[8].parse::<u64>().unwrap_or(0));
        }
    }
    Ok((rx, tx))
}

fn read_metrics(previous: &mut Option<Counters>, system: &SystemInfo) -> io::Result<Metrics> {
    let (total, idle) = cpu_counters()?;
    let (rx, tx) = network_counters()?;
    let now = Instant::now();
    let (cpu, rx_rate, tx_rate) = if let Some(old) = previous {
        let delta = total.saturating_sub(old.total);
        let elapsed = now.duration_since(old.at).as_secs_f64().max(0.001);
        (if delta > 0 { 100.0 * (1.0 - idle.saturating_sub(old.idle) as f64 / delta as f64) } else { 0.0 },
         rx.saturating_sub(old.rx) as f64 / elapsed, tx.saturating_sub(old.tx) as f64 / elapsed)
    } else { (0.0, 0.0, 0.0) };
    *previous = Some(Counters { total, idle, rx, tx, at: now });
    let load_text = fs::read_to_string("/proc/loadavg")?;
    let mut loads = load_text.split_whitespace();
    let load = loads.next().unwrap_or("0").parse().unwrap_or(0.0);
    let load5 = loads.next().unwrap_or("0").parse().unwrap_or(0.0);
    let load15 = loads.next().unwrap_or("0").parse().unwrap_or(0.0);
    let uptime = fs::read_to_string("/proc/uptime")?.split_whitespace().next().unwrap_or("0").parse::<f64>().unwrap_or(0.0) as u64;
    let (mem_used,mem_total,swap_used,swap_total) = memory_usage()?;
    let (disk_used,disk_total) = disk_usage()?;
    let percent = |used:u64,total:u64| if total>0 { (100.0*used as f64/total as f64).clamp(0.0,100.0) } else { 0.0 };
    Ok(Metrics { cpu: cpu.clamp(0.0, 100.0), memory: percent(mem_used,mem_total),
        disk: percent(disk_used,disk_total), rx: rx_rate, tx: tx_rate, load, uptime,
        mem_used,mem_total,swap_used,swap_total,disk_used,disk_total,rx_bytes:rx,tx_bytes:tx,
        load5,load15,os:system.os.clone(),kernel:system.kernel.clone(),arch:system.arch.clone(),
        cpu_model:system.cpu_model.clone(),cpu_cores:system.cpu_cores,latency_ms:None,
        version:env!("CARGO_PKG_VERSION"),boot:system.boot.clone() })
}

/// 常见的公网 IPv4 判断。标准库的 is_global 还没稳定，这里自己列：去掉私有、回环、链路本地、
/// 运营商级 NAT（100.64/10）、基准测试（198.18/15）、文档、组播和保留段。
fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_broadcast() || ip.is_documentation()
        || ip.is_unspecified() || ip.is_multicast() || o[0] == 0 || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xC0) == 64) || (o[0] == 198 && (o[1] & 0xFE) == 18))
}

/// 全球单播（2000::/3），去掉文档段。
fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xE000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
}

/// /proc/net/if_inet6 里「长期有效」的全局地址：排除隐私扩展生成的临时地址、已废弃的、
/// 还在做重复检测的和检测失败的。家宽的 IPv6 大多开着隐私扩展，临时地址一天一换，报它没意义。
fn stable_v6(text: &str) -> Vec<Ipv6Addr> {
    const TEMPORARY: u32 = 0x01;
    const DAD_FAILED: u32 = 0x08;
    const DEPRECATED: u32 = 0x20;
    const TENTATIVE: u32 = 0x40;
    text.lines().filter_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 || cols[0].len() != 32 {
            return None;
        }
        let scope = u32::from_str_radix(cols[3], 16).ok()?;
        let flags = u32::from_str_radix(cols[4], 16).ok()?;
        if scope != 0 || flags & (TEMPORARY | DAD_FAILED | DEPRECATED | TENTATIVE) != 0 {
            return None;
        }
        let mut bytes = [0_u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&cols[0][i * 2..i * 2 + 2], 16).ok()?;
        }
        let ip = Ipv6Addr::from(bytes);
        public_v6(ip).then_some(ip)
    }).collect()
}

/// 系统往外发包时会用哪个源地址。UDP 的 connect 只查路由表，不发任何数据包。
fn route_source(target: &str, bind: &str) -> Option<IpAddr> {
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(target).ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}

/// 本机的公网地址，每族一个：公网地址优先，IPv6 不取临时和已废弃地址。
/// 只读 /proc、做一次不发包的探测，不需要 netlink 权限——沙箱里是禁掉的。
/// 在 NAT 后面（家宽、大部分国内机器）时 IPv4 一般报不出来，hub 会改用它看到的出口地址。
fn local_addresses() -> (Option<String>, Option<String>) {
    let v4 = match route_source("1.1.1.1:53", "0.0.0.0:0") {
        Some(IpAddr::V4(ip)) if public_v4(ip) => Some(ip.to_string()),
        _ => None,
    };
    let candidates = stable_v6(&fs::read_to_string("/proc/net/if_inet6").unwrap_or_default());
    let preferred = match route_source("[2606:4700:4700::1111]:53", "[::]:0") {
        Some(IpAddr::V6(ip)) => Some(ip),
        _ => None,
    };
    let v6 = preferred.filter(|ip| candidates.contains(ip)).or_else(|| candidates.first().copied()).map(|ip| ip.to_string());
    (v4, v6)
}

fn hub_tcp_latency(url: &url::Url) -> Option<f64> {
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let address = (host, port).to_socket_addrs().ok()?.next()?;
    let started = Instant::now();
    TcpStream::connect_timeout(&address, Duration::from_millis(750)).ok()?;
    Some(started.elapsed().as_secs_f64() * 1000.0)
}

enum Outcome {
    /// 握手用时，毫秒。
    Latency(f64),
    /// 超时或被拒绝。
    Loss,
    /// 这一轮不记录：解析太慢，测出来的不是目标的延迟。
    Skip,
}

/// 一次探测。解析时间不计入延迟：只有连接那一段才是要测的东西。
fn check(task: &Task) -> Outcome {
    let started = Instant::now();
    let Ok(addresses) = (task.h.as_str(), task.p).to_socket_addrs() else { return Outcome::Loss };
    if started.elapsed() > RESOLVE_BUDGET { return Outcome::Skip; }
    for address in addresses.take(MAX_ADDRESSES) {
        let at = Instant::now();
        if TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok() {
            return Outcome::Latency(at.elapsed().as_secs_f64() * 1000.0);
        }
    }
    Outcome::Loss
}

/// 探测线程。所有线程共用一个接收端：谁空闲谁取活，不需要每个任务一条线程。
fn worker(jobs: Arc<Mutex<Receiver<Task>>>, results: mpsc::Sender<Sample>) {
    loop {
        let job = { let Ok(queue) = jobs.lock() else { return }; queue.recv() };
        let Ok(task) = job else { return };
        let sample = match check(&task) {
            Outcome::Latency(ms) => Sample { i: task.i, ms: Some((ms * 10.0).round() / 10.0) },
            Outcome::Loss => Sample { i: task.i, ms: None },
            Outcome::Skip => continue,
        };
        if results.send(sample).is_err() { return; }
    }
}

#[derive(Default)]
struct Schedule {
    tasks: Vec<Task>,
    due: HashMap<i64, Instant>,
}

impl Schedule {
    /// 整表替换。已有任务保留它原来的下次时间，新任务错开半秒起跑，
    /// 免得一次下发 20 个监控就在同一毫秒全部发出去。
    fn replace(&mut self, tasks: Vec<Task>) {
        let now = Instant::now();
        let mut due = HashMap::with_capacity(tasks.len());
        for (index, task) in tasks.iter().enumerate() {
            let at = self.due.get(&task.i).copied()
                .unwrap_or_else(|| now + Duration::from_millis(500 * index as u64));
            due.insert(task.i, at);
        }
        self.tasks = tasks;
        self.due = due;
    }

    fn dispatch(&mut self, jobs: &SyncSender<Task>) {
        let now = Instant::now();
        let due = &mut self.due;
        for task in &self.tasks {
            let ready = due.get(&task.i).map(|at| now >= *at).unwrap_or(false);
            if !ready { continue; }
            // 队列满说明上一轮还没跑完：一秒后再看，不堆积。
            let next = if jobs.try_send(task.clone()).is_ok() {
                now + Duration::from_secs(task.s)
            } else {
                now + Duration::from_secs(1)
            };
            due.insert(task.i, next);
        }
    }
}

/// hub 已经校验过，这里再挡一次：agent 只连它认得出形状的地址。
fn accept(tasks: Vec<Task>) -> Vec<Task> {
    tasks.into_iter()
        .filter(|task| {
            task.p > 0 && !task.h.is_empty() && task.h.len() <= 253
                && task.h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
        })
        .map(|task| Task { s: task.s.clamp(5, 3600), ..task })
        .take(MAX_TASKS)
        .collect()
}

fn retryable(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(io) if matches!(io.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted))
}

/// 读超时让主循环定期醒来。rustls 的 StreamOwned 也可以写成 `&stream.sock`。
fn set_read_timeout(socket: &WebSocket<MaybeTlsStream<TcpStream>>, timeout: Duration) {
    let stream = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => stream.get_ref(),
        _ => return,
    };
    let _ = stream.set_read_timeout(Some(timeout));
}

fn option(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    // 安装脚本用它确认下载的程序在这台机器上跑得起来（系统太旧、架构不对都会在这里暴露），
    // 然后才替换正在用的那个。
    if args.iter().any(|arg| arg == "--version") {
        println!("probe-agent {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let server = option(&args, "--server").or_else(|| std::env::var("PROBE_SERVER").ok()).ok_or("需要 --server")?;
    let id = option(&args, "--id").or_else(|| std::env::var("PROBE_ID").ok()).ok_or("需要 --id")?;
    let token = option(&args, "--token").or_else(|| std::env::var("PROBE_TOKEN").ok()).ok_or("需要 --token 或 PROBE_TOKEN")?;
    let interval: u64 = option(&args, "--interval").as_deref().unwrap_or("5").parse()?;
    if !(2..=10).contains(&interval) || !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.len() != 16 {
        return Err("interval 应为 2–10 秒；ID 应为 16 位十六进制".into());
    }
    let mut url = url::Url::parse(&server)?;
    let ws_scheme = match (url.scheme(), url.host_str()) {
        ("https", Some(_)) => "wss",
        ("http", Some("localhost" | "127.0.0.1" | "[::1]" | "::1")) => "ws",
        _ => return Err("远程 hub 必须使用 HTTPS；仅允许回环地址使用 HTTP".into()),
    };
    if url.username() != "" || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("hub 地址不能包含凭据、查询参数或片段".into());
    }
    url.set_scheme(ws_scheme).map_err(|_| "无效的 URL scheme")?;
    url.set_path(&format!("/api/agent/{id}"));

    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Task>(MAX_TASKS);
    let (results_tx, results_rx) = mpsc::channel::<Sample>();
    let queue = Arc::new(Mutex::new(jobs_rx));
    for n in 0..WORKERS {
        let (queue, results) = (Arc::clone(&queue), results_tx.clone());
        // 小栈：这些线程只做一次解析和一次连接，默认的 8 MiB 纯属浪费。
        thread::Builder::new().name(format!("probe{n}")).stack_size(96 * 1024)
            .spawn(move || worker(queue, results))?;
    }
    drop(results_tx);

    let system = system_info();
    let mut schedule = Schedule::default();
    let mut previous = None;
    let mut latency = None;
    let mut next_latency = Instant::now();

    loop {
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse()?);
        match connect(request) {
            Ok((mut socket, _)) => {
                eprintln!("已连接到 hub");
                set_read_timeout(&socket, POLL);
                // 断线期间攒下的结果按 hub 的收报时间落库会错位，直接丢掉。
                while results_rx.try_recv().is_ok() {}
                let mut next_metrics = Instant::now();
                let mut last_heard = Instant::now();
                let mut next_ping = Instant::now() + HEARTBEAT;
                let mut sent_facts: Option<(Option<String>, Option<String>)> = None;
                let mut next_facts = Instant::now();
                let mut pending: Vec<Sample> = Vec::new();
                loop {
                    match socket.read() {
                        Ok(message) => {
                            // 收到任何东西（任务、pong）都说明连接还活着。
                            last_heard = Instant::now();
                            match message {
                                Message::Text(text) => {
                                    if text.len() <= 16384 {
                                        if let Ok(Down::Tasks { tasks }) = serde_json::from_str::<Down>(text.as_str()) {
                                            let tasks = accept(tasks);
                                            eprintln!("收到 {} 个监控任务", tasks.len());
                                            schedule.replace(tasks);
                                        }
                                    }
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        Err(error) if retryable(&error) => {}
                        Err(error) => { eprintln!("连接已断开: {error}"); break; }
                    }

                    if Instant::now() >= next_ping {
                        next_ping = Instant::now() + HEARTBEAT;
                        if socket.send(Message::Ping(Default::default())).is_err() { break; }
                    }
                    if last_heard.elapsed() > SILENT_LIMIT {
                        eprintln!("hub 超过 {} 秒没有回应（网络切换或 IP 变了？），重新连接", SILENT_LIMIT.as_secs());
                        break;
                    }

                    schedule.dispatch(&jobs_tx);

                    loop {
                        match results_rx.try_recv() {
                            Ok(sample) => if pending.len() < MAX_TASKS { pending.push(sample) },
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                    if !pending.is_empty() {
                        let Ok(payload) = serde_json::to_string(&Up::Ping { r: &pending }) else { break };
                        if socket.send(Message::Text(payload.into())).is_err() { break; }
                        pending.clear();
                    }

                    if Instant::now() >= next_metrics {
                        next_metrics = Instant::now() + Duration::from_secs(interval);
                        match read_metrics(&mut previous, &system) {
                            Ok(mut metrics) => {
                                if Instant::now() >= next_latency {
                                    latency = hub_tcp_latency(&url);
                                    next_latency = Instant::now() + Duration::from_secs(60);
                                }
                                metrics.latency_ms = latency;
                                let Ok(payload) = serde_json::to_string(&Up::Metrics(&metrics)) else { break };
                                if let Err(error) = socket.send(Message::Text(payload.into())) {
                                    eprintln!("连接已断开: {error}");
                                    break;
                                }
                            }
                            Err(error) => eprintln!("采集失败: {error}"),
                        }
                    }

                    // 地址：连上先报一次，之后每 5 分钟看一眼，变了再报（家宽 IPv6 前缀会轮换）。
                    if Instant::now() >= next_facts {
                        next_facts = Instant::now() + Duration::from_secs(300);
                        let facts = local_addresses();
                        if sent_facts.as_ref() != Some(&facts) {
                            let Ok(payload) = serde_json::to_string(&Up::Facts { v4: facts.0.as_deref(), v6: facts.1.as_deref() }) else { break };
                            if socket.send(Message::Text(payload.into())).is_err() { break; }
                            sent_facts = Some(facts);
                        }
                    }

                    // 排队中的 pong 要靠 flush 发出去，否则 hub 侧的保活会误判。
                    if let Err(error) = socket.flush() {
                        if !retryable(&error) { break; }
                    }
                }
            }
            Err(error) => eprintln!("连接失败: {error}"),
        }
        thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_metrics_are_finite() {
        let m = read_metrics(&mut None, &system_info()).unwrap();
        assert!((0.0..=100.0).contains(&m.memory));
        assert!((0.0..=100.0).contains(&m.disk));
        assert!(m.load.is_finite());
    }

    #[test]
    fn picks_only_stable_public_v6() {
        // 地址、接口号、前缀长度、作用域、标志、网卡名——/proc/net/if_inet6 的真实格式
        let text = "\
20010db8000000000000000000000001 02 40 00 80 eth0
24098a1e7c3000010000000000000001 02 40 00 80 eth0
24098a1e7c30000154f2a1b3c4d5e6f7 02 40 00 01 eth0
24098a1e7c3000029999999999999999 02 40 00 20 eth0
fe800000000000000000000000000001 02 40 20 80 eth0
00000000000000000000000000000001 01 80 10 80 lo
";
        let found = stable_v6(text);
        assert_eq!(found.len(), 1, "只留下长期有效的那一个全局地址");
        assert_eq!(found[0].to_string(), "2409:8a1e:7c30:1::1");
    }

    #[test]
    fn public_v4_rules() {
        for private in ["10.0.0.1", "192.168.1.1", "172.16.0.1", "100.64.0.1", "127.0.0.1", "169.254.1.1", "198.18.0.1"] {
            assert!(!public_v4(private.parse().unwrap()), "{private} 不是公网地址");
        }
        assert!(public_v4("1.1.1.1".parse().unwrap()));
        assert!(public_v4("100.128.0.1".parse().unwrap()), "100.64/10 之外的 100.x 是公网");
    }

    #[test]
    fn loopback_is_not_physical() {
        assert!(!physical("lo"));
    }

    #[test]
    fn rejects_malformed_tasks() {
        let tasks: Vec<Task> = serde_json::from_str(
            r#"[{"i":1,"h":"1.1.1.1","p":443,"s":1},{"i":2,"h":"","p":80,"s":60},{"i":3,"h":"a b","p":80,"s":60}]"#,
        ).unwrap();
        let accepted = accept(tasks);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].s, 5, "间隔被夹回下限");
    }

    /// 一个不可达地址必须记成丢包，而不是被静默跳过。
    #[test]
    fn unreachable_target_is_loss() {
        let task = Task { i: 1, h: "127.0.0.1".into(), p: 1, s: 60 };
        assert!(matches!(check(&task), Outcome::Loss));
    }

    #[test]
    fn schedule_dispatches_once_per_interval() {
        let (tx, rx) = mpsc::sync_channel::<Task>(8);
        let mut schedule = Schedule::default();
        schedule.replace(vec![Task { i: 1, h: "127.0.0.1".into(), p: 80, s: 60 }]);
        schedule.due.insert(1, Instant::now());
        schedule.dispatch(&tx);
        schedule.dispatch(&tx);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "同一个间隔内只能派发一次");
    }
}
