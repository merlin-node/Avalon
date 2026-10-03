use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

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

struct Counters { total: u64, idle: u64, rx: u64, tx: u64, nics: u32, at: Instant }

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

/// 默认路由走的网卡（IPv4 看 /proc/net/route，IPv6 看 /proc/net/ipv6_route）。
/// 只认生效中的路由（RTF_UP），不认拒绝路由（RTF_REJECT，IPv6 的 unreachable 默认路由就是这种，
/// 通常挂在 lo 上），lo 本身也去掉。结果排好序、去过重。
fn default_route_nics(v4: &str, v6: &str) -> Vec<String> {
    const RTF_UP: u32 = 0x0001;
    const RTF_REJECT: u32 = 0x0200;
    let usable = |flags: &str| u32::from_str_radix(flags, 16).is_ok_and(|f| f & RTF_UP != 0 && f & RTF_REJECT == 0);
    let mut nics = Vec::new();
    for line in v4.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
        if cols.len() >= 8 && cols[1] == "00000000" && cols[7] == "00000000" && usable(cols[3]) {
            nics.push(cols[0].to_string());
        }
    }
    for line in v6.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // 目的地址 前缀长度 源地址 源前缀 下一跳 metric refcnt use flags 网卡
        if cols.len() >= 10 && cols[1] == "00" && cols[0].len() == 32 && cols[0].bytes().all(|b| b == b'0') && usable(cols[8]) {
            nics.push(cols[9].to_string());
        }
    }
    nics.retain(|nic| nic != "lo");
    nics.sort();
    nics.dedup();
    nics
}

/// 从 /proc/net/dev 里挑出要统计的网卡，加总收发字节。
///
/// 对照过极简探针和 Komari：它们按网卡名黑名单排除，特殊机器再让用户手动指定网卡。
/// 我们按内核事实判断，不用配置，按顺序取第一条能用的：
/// 1. 真实网卡里走默认路由的那几张。普通 VPS、独服都在这一条；同机房内网的第二张网卡
///    （不走默认路由）不算，免得把商家不计费的内网流量算进来。
/// 2. 有真实网卡、但都不走默认路由：算全部真实网卡。PVE 宿主机（默认路由在网桥 vmbr0 上）、
///    多网卡绑定（默认路由在 bond0 上）是这种，算的是物理口，不重复。
/// 3. 一张真实网卡都没有：LXC、OpenVZ 这类容器，连主网卡 eth0 / venet0 都是虚拟的，
///    算默认路由走的那几张。以前这里一张都不算，流量一直是 0。
/// docker0、veth、网桥、WireGuard 这些虚拟网卡只会在第 3 条里、且正好走默认路由时才算。
///
/// 第三个返回值是所算网卡集合的指纹。集合一变（升级到这个版本、路由变了、容器换了网卡），
/// 计数器的基数就不连续了；它拼进上报的 boot，hub 看到 boot 变了就重新对基线，
/// 不会把两组网卡计数之差当成一瞬间的流量。
fn select_counters(dev: &str, is_physical: impl Fn(&str) -> bool, routed: &[String]) -> (u64, u64, u32) {
    let rows: Vec<(String, u64, u64)> = dev.lines().skip(2).filter_map(|line| {
        let (name, counters) = line.split_once(':')?;
        let name = name.trim();
        if name.is_empty() || name == "lo" { return None; }
        let cols: Vec<&str> = counters.split_whitespace().collect();
        if cols.len() < 9 { return None; }
        Some((name.to_string(), cols[0].parse().unwrap_or(0), cols[8].parse().unwrap_or(0)))
    }).collect();
    let real: Vec<&(String, u64, u64)> = rows.iter().filter(|row| is_physical(row.0.as_str())).collect();
    let real_routed: Vec<&(String, u64, u64)> = real.iter().copied().filter(|row| routed.contains(&row.0)).collect();
    let chosen: Vec<&(String, u64, u64)> = if !real_routed.is_empty() {
        real_routed
    } else if !real.is_empty() {
        real
    } else {
        rows.iter().filter(|row| routed.contains(&row.0)).collect()
    };
    let mut rx = 0_u64;
    let mut tx = 0_u64;
    let mut names: Vec<&str> = Vec::new();
    for row in &chosen {
        rx = rx.saturating_add(row.1);
        tx = tx.saturating_add(row.2);
        names.push(row.0.as_str());
    }
    names.sort_unstable();
    // FNV-1a，只用来比较"集合变没变"，不涉及安全。
    let mut hash: u32 = 0x811c_9dc5;
    for byte in names.join(",").bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (rx, tx, hash)
}

fn network_counters() -> io::Result<(u64, u64, u32)> {
    let dev = fs::read_to_string("/proc/net/dev")?;
    let routed = default_route_nics(
        &fs::read_to_string("/proc/net/route").unwrap_or_default(),
        &fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default(),
    );
    Ok(select_counters(&dev, physical, &routed))
}

fn read_metrics(previous: &mut Option<Counters>, system: &SystemInfo) -> io::Result<Metrics> {
    let (total, idle) = cpu_counters()?;
    let (rx, tx, nics) = network_counters()?;
    let now = Instant::now();
    let (cpu, rx_rate, tx_rate) = if let Some(old) = previous {
        let delta = total.saturating_sub(old.total);
        let elapsed = now.duration_since(old.at).as_secs_f64().max(0.001);
        // 所算的网卡变了，两次读数不能相减，这一次网速记 0。
        let same = old.nics == nics;
        (if delta > 0 { 100.0 * (1.0 - idle.saturating_sub(old.idle) as f64 / delta as f64) } else { 0.0 },
         if same { rx.saturating_sub(old.rx) as f64 / elapsed } else { 0.0 },
         if same { tx.saturating_sub(old.tx) as f64 / elapsed } else { 0.0 })
    } else { (0.0, 0.0, 0.0) };
    *previous = Some(Counters { total, idle, rx, tx, nics, at: now });
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
        cpu_model:system.cpu_model.clone(),cpu_cores:system.cpu_cores,
        version:env!("CARGO_PKG_VERSION"),
        // 开机 id 后面拼上所算网卡集合的指纹（共 45 个字符，hub 那边只收十六进制和 -、最长 64）。
        boot:format!("{}-{nics:08x}",system.boot.chars().take(36).collect::<String>()) })
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

type HubSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// 连 hub，每一步都有时限：TCP 连接 10 秒，TLS 和 WebSocket 握手 15 秒。
/// 以前用 tungstenite::connect，它的连接和握手都不设超时：网络断在握手中途、对面又没回 RST 时
/// 会一直卡住。2026-09-26 大鸡断网恢复后，本机被控就这样卡了 9 分钟，手动重启才回来。
/// 本机没有公网 IPv4（在 NAT 后面，比如双栈家宽）时先试 IPv4：双栈机器默认优先 IPv6，
/// hub 就只看得到 v6，家宽的出口 IPv4 永远报不出来（极简探针也是这么修的）。
fn open_hub(url: &url::Url, token: &str) -> Result<HubSocket, Box<dyn Error>> {
    let host = url.host_str().ok_or("hub 地址缺少主机名")?;
    let port = url.port_or_known_default().ok_or("hub 地址缺少端口")?;
    let addresses = dial_order((host, port).to_socket_addrs()?.collect(), local_addresses().0.is_none());
    let mut last: Box<dyn Error> = "解析不到 hub 的地址".into();
    for address in addresses {
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse()?);
        match dial(address, request) {
            Ok(socket) => return Ok(socket),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// 拨号顺序：在 NAT 后面时 IPv4 排前面，其余保持系统给的顺序（排序是稳定的）。
fn dial_order(mut addresses: Vec<SocketAddr>, prefer_v4: bool) -> Vec<SocketAddr> {
    if prefer_v4 {
        addresses.sort_by_key(|address| !address.is_ipv4());
    }
    addresses
}

fn dial(address: SocketAddr, request: tungstenite::handshake::client::Request) -> Result<HubSocket, Box<dyn Error>> {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    // 读写都有时限，握手卡住 15 秒就放弃、换下一个地址。连上之后主循环会把读超时换成自己的；
    // 写超时一直保留，对面死掉时发送也不会永远卡住。
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let (socket, _) = tungstenite::client_tls(request, stream).map_err(|error| error.to_string())?;
    Ok(socket)
}

enum Outcome {
    /// 握手用时，毫秒。
    Latency(f64),
    /// 超时或被拒绝。
    Loss,
    /// 这一轮不记录：解析太慢，测出来的不是目标的延迟。
    Skip,
}

/// 只测公网地址。主控万一被人拿下，对方能让被控去连的也只有公网：本机（127.0.0.1）、
/// 所在的内网（10.x、192.168.x……）、云厂商的元数据地址（169.254.169.254）一律不连，
/// 被控不会变成扫它自己内网的工具。看的是解析出来的地址，域名指向内网也一样挡住。
fn public_target(address: &SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

/// 一次探测。解析时间不计入延迟：只有连接那一段才是要测的东西。
fn check(task: &Task) -> Outcome {
    let started = Instant::now();
    let Ok(addresses) = (task.h.as_str(), task.p).to_socket_addrs() else { return Outcome::Loss };
    if started.elapsed() > RESOLVE_BUDGET { return Outcome::Skip; }
    for address in addresses.filter(public_target).take(MAX_ADDRESSES) {
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

    loop {
        match open_hub(&url, &token) {
            Ok(mut socket) => {
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
                            Ok(metrics) => {
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

    /// 强制走 IPv4 时要自己拼端口，wss 默认 443 得靠 url 库认出来。
    #[test]
    fn hub_urls_have_a_port() {
        let url = url::Url::parse("wss://hub.example.com/api/agent/0123456789abcdef").unwrap();
        assert_eq!(url.port_or_known_default(), Some(443));
    }

    #[test]
    fn nat_hosts_try_ipv4_first() {
        let v6: SocketAddr = "[2606:4700::1]:443".parse().unwrap();
        let v4a: SocketAddr = "104.21.0.1:443".parse().unwrap();
        let v4b: SocketAddr = "172.67.0.1:443".parse().unwrap();
        assert_eq!(dial_order(vec![v6, v4a, v4b], true), vec![v4a, v4b, v6], "NAT 后面：IPv4 先，其余顺序不变");
        assert_eq!(dial_order(vec![v6, v4a], false), vec![v6, v4a], "有公网 IPv4：按系统给的顺序");
    }

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

    const DEV: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0
  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
  eth1: 70 7 0 0 0 0 0 0 80 8 0 0 0 0 0 0
docker0: 300 3 0 0 0 0 0 0 400 4 0 0 0 0 0 0
vethab12: 300 3 0 0 0 0 0 0 400 4 0 0 0 0 0 0
 vmbr0: 900 9 0 0 0 0 0 0 900 9 0 0 0 0 0 0
";
    fn real(name: &str) -> bool { name == "eth0" || name == "eth1" }
    fn routes(names: &[&str]) -> Vec<String> { names.iter().map(|n| n.to_string()).collect() }

    #[test]
    fn vps_counts_the_routed_real_nic_only() {
        // 公网 eth0 走默认路由；eth1 是同机房内网，docker0、veth 是虚拟的，都不算
        assert_eq!(select_counters(DEV, real, &routes(&["eth0"])).0, 1000);
        assert_eq!(select_counters(DEV, real, &routes(&["eth0"])).1, 2000);
        // 两张都走默认路由（双线）就都算
        let (rx, tx, _) = select_counters(DEV, real, &routes(&["eth0", "eth1"]));
        assert_eq!((rx, tx), (1070, 2080));
    }

    #[test]
    fn bridged_hosts_count_all_real_nics() {
        // PVE：默认路由在网桥 vmbr0 上，真实网卡都不走默认路由，算全部真实网卡，不算网桥
        let (rx, tx, _) = select_counters(DEV, real, &routes(&["vmbr0"]));
        assert_eq!((rx, tx), (1070, 2080));
        let (rx, _, _) = select_counters(DEV, real, &[]);
        assert_eq!(rx, 1070, "没有默认路由时也一样");
    }

    #[test]
    fn containers_fall_back_to_the_default_route() {
        // LXC 里 eth0 也是虚拟网卡：一张真实网卡都没有，改算默认路由走的那张
        let (rx, tx, _) = select_counters(DEV, |_| false, &routes(&["eth0"]));
        assert_eq!((rx, tx), (1000, 2000));
        let (rx, tx, _) = select_counters(DEV, |_| false, &[]);
        assert_eq!((rx, tx), (0, 0), "连默认路由都没有就没法算");
        let (rx, _, _) = select_counters(DEV, |_| false, &routes(&["ppp9"]));
        assert_eq!(rx, 0, "路由里的网卡在 /proc/net/dev 里没有也不出错");
    }

    #[test]
    fn nic_set_changes_the_fingerprint() {
        let (_, _, vps) = select_counters(DEV, real, &routes(&["eth0"]));
        let (_, _, lxc) = select_counters(DEV, |_| false, &routes(&["eth0"]));
        let (_, _, both) = select_counters(DEV, real, &routes(&["eth0", "eth1"]));
        assert_eq!(vps, lxc, "算的是同一张网卡，指纹一样");
        assert_ne!(vps, both, "网卡集合变了，指纹跟着变，hub 会重新对基线");
    }

    #[test]
    fn reads_default_routes() {
        let v4 = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0
eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
eth2\t00000000\t0101A8C0\t0002\t0\t0\t0\t00000000\t0\t0\t0
";
        let v6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003 eth1
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200 lo
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 00000400 00000001 00000000 00000201 eth3
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001 eth0
";
        // eth2 的路由没有 UP，eth3 是拒绝路由，lo 不算
        assert_eq!(default_route_nics(v4, v6), routes(&["eth0", "eth1"]));
        assert!(default_route_nics("", "").is_empty(), "读不到路由表时是空的");
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

    /// 本机明明有端口开着也不连：非公网地址一律当丢包，不去探。
    #[test]
    fn private_targets_are_never_dialed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = Task { i: 1, h: "127.0.0.1".into(), p: port, s: 60 };
        assert!(matches!(check(&task), Outcome::Loss));
        for private in ["10.0.0.1:80", "192.168.1.1:80", "169.254.169.254:80", "[::1]:80", "[fe80::1]:80"] {
            assert!(!public_target(&private.parse().unwrap()), "{private} 不该连");
        }
        assert!(public_target(&"1.1.1.1:443".parse().unwrap()));
        assert!(public_target(&"[2606:4700:4700::1111]:443".parse().unwrap()));
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
