use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use sha2::{Digest, Sha256};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 公开节点列表的缓存。那条 SQL 带五个 LEFT JOIN 和四个相关子查询，而数据每
/// 五秒才变一次：五十个访客轮询它，九成的查询算的是同一个答案。
#[derive(Clone)]
struct App { db_path: Arc<str>, nodes: Arc<Mutex<Option<(Instant, String)>>> }

const NODES_CACHE: Duration = Duration::from_secs(1);

mod access;
mod admin;
mod expiry;
mod ping;
mod site;

/// 监控配置的版本号。后台改动后 +1，每个 agent 连接在下一次循环里发现版本变了就重发
/// 任务表——省掉一套 broadcast 管道，代价是最多晚一个上报周期（默认 5 秒）下发。
pub(crate) static CONFIG_VERSION: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize)]
struct Metrics {
    cpu: f64, memory: f64, disk: f64, rx: f64, tx: f64, load: f64, uptime: u64,
    #[serde(default)] mem_used: u64,
    #[serde(default)] mem_total: u64,
    #[serde(default)] swap_used: u64,
    #[serde(default)] swap_total: u64,
    #[serde(default)] disk_used: u64,
    #[serde(default)] disk_total: u64,
    #[serde(default)] rx_bytes: u64,
    #[serde(default)] tx_bytes: u64,
    #[serde(default)] load5: f64,
    #[serde(default)] load15: f64,
    #[serde(default)] os: String,
    #[serde(default)] kernel: String,
    #[serde(default)] arch: String,
    #[serde(default)] cpu_model: String,
    #[serde(default)] cpu_cores: u32,
    #[serde(default)] latency_ms: Option<f64>,
    #[serde(default)] version: String,
    /// /proc/sys/kernel/random/boot_id。每次开机都会变，用来判断流量计数器是不是同一次开机的。
    #[serde(default)] boot: String,
}

/// agent 上行帧。`t` 区分类型，未知类型直接丢弃而不是断连，方便以后加字段。
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Up {
    #[serde(rename = "m")] Metrics(Metrics),
    #[serde(rename = "p")] Ping { #[serde(default)] r: Vec<ping::Sample> },
    /// 本机的公网地址。hub 不信它报的，落库前自己再校验一遍。
    #[serde(rename = "f")] Facts { #[serde(default)] v4: Option<String>, #[serde(default)] v6: Option<String> },
}

#[derive(Serialize)]
struct Node {
    id: String, sort: usize, name: String, online: bool, last_seen: Option<i64>,
    cpu: f64, memory: f64, disk: f64, rx: f64, tx: f64, load: f64, uptime: u64,
    mem_used: u64, mem_total: u64, swap_used: u64, swap_total: u64,
    disk_used: u64, disk_total: u64, load5: f64, load15: f64,
    os: String, kernel: String, arch: String, cpu_model: String, cpu_cores: u32,
    agent_version: String,
    rx_total: u64, tx_total: u64, day_rx: u64, day_tx: u64, month_rx: u64, month_tx: u64,
    country: String, price: f64, currency: String, billing_cycle: String, expires_at: String,
    traffic_limit: u64, traffic_mode: String, traffic_reset_day: i64,
}

#[derive(Serialize)]
struct Point { ts: i64, cpu: f64, memory: f64, disk: f64, rx: f64, tx: f64 }

#[derive(Deserialize)]
struct HistoryRange { hours: Option<u32> }

fn now() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 }

fn db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    Ok(conn)
}

fn init_db(path: &str) -> rusqlite::Result<()> {
    let conn = db(path)?;
    // 新库还没有 nodes 表。只对新装生效的默认值要在建表之前判断。
    let fresh: bool = conn.query_row("SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='nodes')", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS nodes (id TEXT PRIMARY KEY, name TEXT NOT NULL, token TEXT NOT NULL, public INTEGER NOT NULL DEFAULT 1, last_seen INTEGER);
        CREATE TABLE IF NOT EXISTS samples (node_id TEXT NOT NULL, ts INTEGER NOT NULL, cpu REAL NOT NULL, memory REAL NOT NULL, disk REAL NOT NULL, rx REAL NOT NULL, tx REAL NOT NULL, load REAL NOT NULL, uptime INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS latest (node_id TEXT PRIMARY KEY, cpu REAL NOT NULL, memory REAL NOT NULL, disk REAL NOT NULL, rx REAL NOT NULL, tx REAL NOT NULL, load REAL NOT NULL, uptime INTEGER NOT NULL);
        CREATE UNIQUE INDEX IF NOT EXISTS samples_by_node ON samples(node_id, ts);
        CREATE TABLE IF NOT EXISTS details (node_id TEXT PRIMARY KEY, mem_used INTEGER NOT NULL, mem_total INTEGER NOT NULL,
          swap_used INTEGER NOT NULL, swap_total INTEGER NOT NULL, disk_used INTEGER NOT NULL, disk_total INTEGER NOT NULL,
          load5 REAL NOT NULL, load15 REAL NOT NULL, os TEXT NOT NULL, kernel TEXT NOT NULL, arch TEXT NOT NULL,
          cpu_model TEXT NOT NULL, cpu_cores INTEGER NOT NULL, agent_version TEXT NOT NULL DEFAULT '');
        CREATE TABLE IF NOT EXISTS traffic (node_id TEXT PRIMARY KEY, last_rx INTEGER NOT NULL, last_tx INTEGER NOT NULL,
          total_rx INTEGER NOT NULL, total_tx INTEGER NOT NULL, boot TEXT NOT NULL DEFAULT '');
        CREATE TABLE IF NOT EXISTS traffic_day (node_id TEXT NOT NULL, day INTEGER NOT NULL, rx INTEGER NOT NULL,
          tx INTEGER NOT NULL, PRIMARY KEY(node_id,day));
        CREATE TABLE IF NOT EXISTS node_net (node_id TEXT PRIMARY KEY, ipv4 TEXT NOT NULL DEFAULT '',
          ipv6 TEXT NOT NULL DEFAULT '', observed TEXT NOT NULL DEFAULT '', updated INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS traffic_adjust (node_id TEXT PRIMARY KEY, period INTEGER NOT NULL,
          rx INTEGER NOT NULL, tx INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS node_config (node_id TEXT PRIMARY KEY, country TEXT NOT NULL DEFAULT '',
          display_ip TEXT NOT NULL DEFAULT '', notify INTEGER NOT NULL DEFAULT 1, remark TEXT NOT NULL DEFAULT '',
          price REAL NOT NULL DEFAULT 0, currency TEXT NOT NULL DEFAULT '', billing_cycle TEXT NOT NULL DEFAULT '',
          expires_at TEXT NOT NULL DEFAULT '', traffic_limit INTEGER NOT NULL DEFAULT 0,
          traffic_mode TEXT NOT NULL DEFAULT '', traffic_reset_day INTEGER NOT NULL DEFAULT 1,
          auto_renew INTEGER NOT NULL DEFAULT 1);
        CREATE TABLE IF NOT EXISTS admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS admin_sessions (hash TEXT PRIMARY KEY, expires INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS alert_state (key TEXT PRIMARY KEY, state INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS login_attempts (source TEXT PRIMARY KEY, failures INTEGER NOT NULL,
          first INTEGER NOT NULL, until INTEGER NOT NULL);")?;
    // 新装默认关闭公开页：装好到你设置完之前，谁访问都是 404。
    // 老库没有这一项时照旧按开放算，升级不会让已经在用的公开页突然消失。
    if fresh {
        conn.execute("INSERT OR IGNORE INTO admin_settings(key,value) VALUES('public_page','0')", [])?;
    }
    ping::init(&conn)?;
    site::init(&conn)?;
    access::init(&conn)?;
    migrate(&conn)?;
    Ok(())
}

/// `CREATE TABLE IF NOT EXISTS` 不会给已有的表补列，所以旧库升级上来必须单独加。
/// 少了这一步，老用户升级后会在运行时报 "no such column"，而不是在启动时。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    add_columns(conn, "node_config", &[
        ("country", "TEXT NOT NULL DEFAULT ''"),
        ("display_ip", "TEXT NOT NULL DEFAULT ''"),
        ("notify", "INTEGER NOT NULL DEFAULT 1"),
        ("remark", "TEXT NOT NULL DEFAULT ''"),
        ("price", "REAL NOT NULL DEFAULT 0"),
        ("currency", "TEXT NOT NULL DEFAULT ''"),
        ("billing_cycle", "TEXT NOT NULL DEFAULT ''"),
        ("expires_at", "TEXT NOT NULL DEFAULT ''"),
        ("traffic_limit", "INTEGER NOT NULL DEFAULT 0"),
        ("traffic_mode", "TEXT NOT NULL DEFAULT ''"),
        ("traffic_reset_day", "INTEGER NOT NULL DEFAULT 1"),
        ("auto_renew", "INTEGER NOT NULL DEFAULT 1"),
    ])?;
    add_columns(conn, "details", &[("agent_version", "TEXT NOT NULL DEFAULT ''")])?;
    add_columns(conn, "traffic", &[("boot", "TEXT NOT NULL DEFAULT ''")])?;
    // 排序。老数据全是 0，这时节点按名字、监控按 id 兜底；第一次点上下箭头时整张表重新编号。
    add_columns(conn, "nodes", &[("sort", "INTEGER NOT NULL DEFAULT 0")])?;
    add_columns(conn, "monitors", &[("sort", "INTEGER NOT NULL DEFAULT 0")])?;
    // 登录设备列表用。升级前就登着的会话这几项是空的，显示成"未知"，最多七天自然过期。
    add_columns(conn, "admin_sessions", &[
        ("created", "INTEGER NOT NULL DEFAULT 0"),
        ("source", "TEXT NOT NULL DEFAULT ''"),
        ("device", "TEXT NOT NULL DEFAULT ''"),
    ])?;
    // 删掉的旧数据只是把页标成空闲，文件不会变小。改成增量整理模式，清理任务每小时顺手把空页还给磁盘。
    // 老库要整个 VACUUM 一次才能切过去，只在升级后第一次启动时发生。
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if mode != 2 {
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")?;
    }
    Ok(())
}

fn add_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> rusqlite::Result<()> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, ddl) in columns {
        if !existing.iter().any(|column| column == name) {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ddl}"), [])?;
        }
    }
    Ok(())
}

fn valid(m: &Metrics) -> bool {
    [m.cpu, m.memory, m.disk, m.rx, m.tx, m.load, m.load5, m.load15].iter().all(|v| v.is_finite() && *v >= 0.0)
        && m.cpu <= 100.0 && m.memory <= 100.0 && m.disk <= 100.0
        && m.uptime <= i64::MAX as u64
        && m.latency_ms.map_or(true,|v| v.is_finite() && (0.0..=10000.0).contains(&v))
        && [m.mem_used,m.mem_total,m.swap_used,m.swap_total,m.disk_used,m.disk_total,m.rx_bytes,m.tx_bytes]
            .iter().all(|v| *v <= i64::MAX as u64)
        && m.os.len() <= 160 && m.kernel.len() <= 160 && m.arch.len() <= 80
        && m.cpu_model.len() <= 200 && m.cpu_cores <= 4096
        && m.version.len() <= 20 && m.version.bytes().all(|b| b.is_ascii_graphic())
        && m.boot.len() <= 64 && m.boot.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn equal_secret(actual: &str, provided: &str) -> bool {
    let a = actual.as_bytes(); let b = provided.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}


async fn theme_asset(Path(path): Path<String>) -> impl IntoResponse {
    let asset: Option<(&'static str, &'static [u8])> = include!(concat!(env!("OUT_DIR"), "/theme_assets.rs"));
    match asset {
        // assets/ 下的文件名带内容指纹，内容一变名字就变，可以放心长期缓存；其余（字体样式表等）
        // 名字固定，必须每次回来问一声。以前一律缓存 1 小时，改了站名浏览器还用着旧 JS，就是这么来的。
        Some((kind, data)) => {
            let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
            ([("content-type", kind), ("cache-control", cache)], data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn health() -> Json<serde_json::Value> { Json(serde_json::json!({"status":"ok"})) }

/// 安装脚本的地址钥匙：由 token 派生，只出现在后台给出的安装命令里，任何公开接口都拿不到。
/// 扫描器访问 `/install.sh` 之类的固定路径只会得到 404；换了 token 钥匙跟着变。
pub(crate) fn install_key(token: &str) -> String {
    Sha256::digest(format!("install:{token}").as_bytes()).iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn bearer(headers: &HeaderMap) -> &str {
    headers.get("authorization").and_then(|h| h.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("")
}

/// 公网 IPv4：去掉私有、回环、链路本地、运营商级 NAT（100.64/10）、基准测试、文档、组播和保留段。
pub(crate) fn public_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_broadcast() || ip.is_documentation()
        || ip.is_unspecified() || ip.is_multicast() || o[0] == 0 || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xC0) == 64) || (o[0] == 198 && (o[1] & 0xFE) == 18))
}
/// 全球单播 IPv6（2000::/3），去掉文档段。
pub(crate) fn public_v6(ip: std::net::Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xE000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
}
/// 只留合法的公网地址，统一成标准写法；别的一律当没报。
fn clean_ip(value: Option<&str>, v6: bool) -> String {
    match value.map(str::trim).and_then(|text| text.parse::<std::net::IpAddr>().ok()) {
        Some(std::net::IpAddr::V4(ip)) if !v6 && public_v4(ip) => ip.to_string(),
        Some(std::net::IpAddr::V6(ip)) if v6 && public_v6(ip) => ip.to_string(),
        _ => String::new(),
    }
}
/// compose 里声明了走 Cloudflare Tunnel。这时 hub 唯一的入口就是 Tunnel，
/// CF-Connecting-IP 由 Cloudflare 边缘填写，才能拿来做登录封禁。
pub(crate) fn behind_tunnel() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("AVALON_CF_TUNNEL").is_ok_and(|v| v == "1"))
}
/// Cloudflare 填的访客地址。只有一个，不用解析；不像地址就当没有。
pub(crate) fn cf_ip(headers: &HeaderMap) -> Option<String> {
    headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok())
        .and_then(|text| text.trim().parse::<std::net::IpAddr>().ok()).map(|ip| ip.to_string())
}
/// hub 看到的节点出口地址，只在 agent 验过 token 之后用。有 Cloudflare 填的地址就用它
/// （开小黄云或走 Tunnel 时，最坏也只是显示错，和极简探针的取舍一样）；
/// 否则取反代追加的 X-Forwarded-For 最右边那个。本机直连时都没有，返回空。
fn observed_ip(headers: &HeaderMap) -> String {
    if let Some(ip) = cf_ip(headers) {
        return ip;
    }
    headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        .and_then(|value| value.rsplit(',').next()).map(str::trim)
        .and_then(|text| text.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string()).unwrap_or_default()
}
/// 后台显示的地址：agent 自己报的公网地址优先；在 NAT 后面报不出来时，
/// 用 hub 看到的出口地址补上。返回 (IPv4, 是否出口地址, IPv6, 是否出口地址)。
pub(crate) fn shown_ips(ipv4: &str, ipv6: &str, observed: &str) -> (String, bool, String, bool) {
    let exit_v4 = clean_ip(Some(observed), false);
    let exit_v6 = clean_ip(Some(observed), true);
    let (v4, v4_exit) = if !ipv4.is_empty() { (ipv4.to_string(), false) } else { (exit_v4.clone(), !exit_v4.is_empty()) };
    let (v6, v6_exit) = if !ipv6.is_empty() { (ipv6.to_string(), false) } else { (exit_v6.clone(), !exit_v6.is_empty()) };
    (v4, v4_exit, v6, v6_exit)
}

/// 节点 ID 和 token 对得上。格式不对、节点不存在、token 不对，统统同一个结果。
fn credentials_ok(db_path: &str, id: &str, token: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())
        && db(db_path).ok()
            .and_then(|conn| conn.query_row("SELECT token FROM nodes WHERE id=?", [id], |r| r.get::<_, String>(0)).optional().ok().flatten())
            .is_some_and(|expected| equal_secret(&expected, token))
}

/// 一键安装脚本。内容是静态的，不拼任何请求头进去：hub 地址由命令行参数传入，脚本自己校验。
async fn install_script(Path(key): Path<String>, State(state): State<App>) -> Response {
    let shaped = key.len() == 24 && key.bytes().all(|b| b.is_ascii_hexdigit());
    let known = shaped && db(&state.db_path).ok().is_some_and(|conn| {
        let Ok(mut stmt) = conn.prepare("SELECT token FROM nodes") else { return false };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else { return false };
        let hit = rows.flatten().any(|token| equal_secret(&install_key(&token), &key));
        hit
    });
    if !known {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([("content-type", "text/x-shellscript; charset=utf-8"), ("cache-control", "no-store")],
        include_str!("../../deploy/install.sh")).into_response()
}

/// 下发 agent 程序，要带节点的 token。文件放在数据库旁边的 agent/ 目录里，文件名走白名单，
/// 路径里的任何东西都不会拼进文件系统路径。
async fn agent_binary(Path((id, name)): Path<(String, String)>, State(state): State<App>, headers: HeaderMap) -> Response {
    if !credentials_ok(&state.db_path, &id, bearer(&headers)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = match name.as_str() {
        "linux-amd64" => "probe-agent-linux-amd64",
        "linux-arm64" => "probe-agent-linux-arm64",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // 先找数据库旁边的 agent/（手动部署时自己放进去的），再找 Docker 镜像自带的那一份。
    let beside = std::path::Path::new(&*state.db_path).parent()
        .map(|parent| parent.join("agent")).unwrap_or_else(|| "agent".into());
    let bundled = std::path::Path::new("/usr/local/lib/probe");
    match [beside.join(file), bundled.join(file)].iter().find_map(|path| std::fs::read(path).ok()) {
        Some(bytes) => ([("content-type", "application/octet-stream"), ("cache-control", "no-store")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 公开接口里的节点编号。真实 ID 是 agent 登录凭据的一半，不往外给；这个编号由它单向派生，
/// 推不回去，但同一台机器始终不变，公开页上的链接不会失效。
pub(crate) fn public_id(id: &str) -> String {
    Sha256::digest(format!("public:{id}").as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 公开编号换回真实 ID。只在公开节点里找：私有节点不管知不知道编号都是 404。
fn resolve_public(conn: &Connection, pid: &str) -> Option<String> {
    if pid.len() != 16 || !pid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut stmt = conn.prepare("SELECT id FROM nodes WHERE public=1").ok()?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).ok()?;
    let found = rows.flatten().find(|id| public_id(id) == pid);
    found
}

/// 节点列表。访客（包括展示页域名上的所有人）拿到的是精简版：没有真实 ID、内核版本和 agent 版本；
/// 这几项只给已登录的管理员。缓存只缓访客那一份，管理员的请求少，直接查。
async fn nodes(State(state): State<App>, headers: HeaderMap) -> Result<Response, StatusCode> {
    let full = access::admin_view(&state.db_path, &headers);
    if !full {
        if let Ok(cache) = state.nodes.lock() {
            if let Some((at, body)) = cache.as_ref() {
                if at.elapsed() < NODES_CACHE { return Ok(json(body.clone())); }
            }
        }
    }
    let list = read_nodes(&state.db_path, full).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = serde_json::to_string(&list).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !full {
        if let Ok(mut cache) = state.nodes.lock() { *cache = Some((Instant::now(), body.clone())); }
    }
    Ok(json(body))
}

fn json(body: String) -> Response {
    ([("content-type", "application/json")], body).into_response()
}

/// 今日和本月按 hub 所在机器的时区结算，与写入时用的基准一致——按 UTC 切，
/// 东八区的人要到早上八点才看到「今日流量」归零。
/// hub 所在时区的今天。
pub(crate) fn local_today(conn: &Connection) -> rusqlite::Result<Option<expiry::Date>> {
    let today: String = conn.query_row("SELECT date('now','localtime')", [], |r| r.get(0))?;
    Ok(expiry::Date::parse(&today))
}

/// 本期（按这台机器的重置日）累计的上下行，不含手工校正。返回 (本期起始日, 下行, 上行)。
pub(crate) fn period_raw(conn: &Connection, id: &str, reset_day: i64, today: expiry::Date) -> rusqlite::Result<(i64, i64, i64)> {
    let start = expiry::period_start(today, reset_day.clamp(1, 31) as u32).number();
    let (rx, tx): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(rx),0),COALESCE(SUM(tx),0) FROM traffic_day WHERE node_id=? AND day>=?",
        params![id, start], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok((start, rx, tx))
}

/// 本期的上下行，含手工校正。校正只对它被设下的那一期有效：进了新周期自动作废，从零重新累计。
/// 公开页和后台都用这一个函数，两边看到的永远是同一个数。
pub(crate) fn period_usage(conn: &Connection, id: &str, reset_day: i64, today: expiry::Date) -> rusqlite::Result<(i64, i64)> {
    let (start, rx, tx) = period_raw(conn, id, reset_day, today)?;
    let adjust: Option<(i64, i64, i64)> = conn.query_row(
        "SELECT period,rx,tx FROM traffic_adjust WHERE node_id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let (rx, tx) = match adjust {
        Some((period, adjust_rx, adjust_tx)) if period == start => (rx + adjust_rx, tx + adjust_tx),
        _ => (rx, tx),
    };
    Ok((rx.max(0), tx.max(0)))
}

fn read_nodes(path: &str, full: bool) -> rusqlite::Result<Vec<Node>> {
    let conn = db(path)?;
    let mut stmt = conn.prepare("SELECT n.id,n.name,n.last_seen,COALESCE(l.cpu,s.cpu),COALESCE(l.memory,s.memory),COALESCE(l.disk,s.disk),COALESCE(l.rx,s.rx),COALESCE(l.tx,s.tx),COALESCE(l.load,s.load),COALESCE(l.uptime,s.uptime),
        d.mem_used,d.mem_total,d.swap_used,d.swap_total,d.disk_used,d.disk_total,d.load5,d.load15,
        d.os,d.kernel,d.arch,d.cpu_model,d.cpu_cores,t.total_rx,t.total_tx,
        (SELECT rx FROM traffic_day WHERE node_id=n.id AND day=CAST(strftime('%s','now','localtime') AS INTEGER)/86400),
        (SELECT tx FROM traffic_day WHERE node_id=n.id AND day=CAST(strftime('%s','now','localtime') AS INTEGER)/86400),
        0,0,
        c.country,c.price,c.currency,c.billing_cycle,c.expires_at,c.traffic_limit,c.traffic_mode,c.traffic_reset_day,d.agent_version
        FROM nodes n LEFT JOIN samples s ON s.rowid = (SELECT rowid FROM samples WHERE node_id=n.id ORDER BY ts DESC,rowid DESC LIMIT 1)
        LEFT JOIN latest l ON l.node_id=n.id
        LEFT JOIN details d ON d.node_id=n.id LEFT JOIN traffic t ON t.node_id=n.id
        LEFT JOIN node_config c ON c.node_id=n.id
        WHERE n.public=1 ORDER BY n.sort,n.name")?;
    let rows = stmt.query_map([], |r| {
        let seen: Option<i64> = r.get(2)?;
        Ok(Node { id:r.get(0)?, sort:0, name:r.get(1)?, online:seen.is_some_and(|t| now()-t < 30), last_seen:seen,
            cpu:r.get::<_,Option<f64>>(3)?.unwrap_or(0.0), memory:r.get::<_,Option<f64>>(4)?.unwrap_or(0.0),
            disk:r.get::<_,Option<f64>>(5)?.unwrap_or(0.0), rx:r.get::<_,Option<f64>>(6)?.unwrap_or(0.0),
            tx:r.get::<_,Option<f64>>(7)?.unwrap_or(0.0), load:r.get::<_,Option<f64>>(8)?.unwrap_or(0.0),
            uptime:r.get::<_,Option<i64>>(9)?.unwrap_or(0).max(0) as u64,
            mem_used:r.get::<_,Option<i64>>(10)?.unwrap_or(0).max(0) as u64,
            mem_total:r.get::<_,Option<i64>>(11)?.unwrap_or(0).max(0) as u64,
            swap_used:r.get::<_,Option<i64>>(12)?.unwrap_or(0).max(0) as u64,
            swap_total:r.get::<_,Option<i64>>(13)?.unwrap_or(0).max(0) as u64,
            disk_used:r.get::<_,Option<i64>>(14)?.unwrap_or(0).max(0) as u64,
            disk_total:r.get::<_,Option<i64>>(15)?.unwrap_or(0).max(0) as u64,
            load5:r.get::<_,Option<f64>>(16)?.unwrap_or(0.0),
            load15:r.get::<_,Option<f64>>(17)?.unwrap_or(0.0),
            os:r.get::<_,Option<String>>(18)?.unwrap_or_default(),
            kernel:r.get::<_,Option<String>>(19)?.unwrap_or_default(),
            arch:r.get::<_,Option<String>>(20)?.unwrap_or_default(),
            cpu_model:r.get::<_,Option<String>>(21)?.unwrap_or_default(),
            cpu_cores:r.get::<_,Option<i64>>(22)?.unwrap_or(0).max(0) as u32,
            agent_version:r.get::<_,Option<String>>(37)?.unwrap_or_default(),
            rx_total:r.get::<_,Option<i64>>(23)?.unwrap_or(0).max(0) as u64,
            tx_total:r.get::<_,Option<i64>>(24)?.unwrap_or(0).max(0) as u64,
            day_rx:r.get::<_,Option<i64>>(25)?.unwrap_or(0).max(0) as u64,
            day_tx:r.get::<_,Option<i64>>(26)?.unwrap_or(0).max(0) as u64,
            month_rx:r.get::<_,Option<i64>>(27)?.unwrap_or(0).max(0) as u64,
            month_tx:r.get::<_,Option<i64>>(28)?.unwrap_or(0).max(0) as u64,
            country:r.get::<_,Option<String>>(29)?.unwrap_or_default(),
            price:r.get::<_,Option<f64>>(30)?.unwrap_or(0.0),
            currency:r.get::<_,Option<String>>(31)?.unwrap_or_default(),
            billing_cycle:r.get::<_,Option<String>>(32)?.unwrap_or_default(),
            expires_at:r.get::<_,Option<String>>(33)?.unwrap_or_default(),
            traffic_limit:r.get::<_,Option<i64>>(34)?.unwrap_or(0).max(0) as u64,
            traffic_mode:r.get::<_,Option<String>>(35)?.unwrap_or_default(),
            traffic_reset_day:r.get::<_,Option<i64>>(36)?.unwrap_or(1) })
    })?;
    let mut list: Vec<Node> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    // 本月流量按每个节点自己的重置日算，不按自然月；日流量表留 35 天，覆盖得了任何一个周期。
    if let Some(today) = local_today(&conn)? {
        for node in &mut list {
            let (rx, tx) = period_usage(&conn, &node.id, node.traffic_reset_day, today)?;
            node.month_rx = rx as u64;
            node.month_tx = tx as u64;
        }
    }
    // 月流量按真实 ID 算完再换成公开编号。
    // 公开页主题按 sort 字段排节点。以前从来没发这个字段，顺序是碰巧跟着接口走的。
    for (index, node) in list.iter_mut().enumerate() {
        node.sort = index + 1;
        node.id = public_id(&node.id);
        if !full {
            // 内核版本对得上已知漏洞就能定向攻击，agent 版本同理。访客看发行版名称就够了。
            node.kernel.clear();
            node.agent_version.clear();
        }
    }
    Ok(list)
}

async fn history(Path(id): Path<String>, Query(range): Query<HistoryRange>, State(state): State<App>) -> Result<Json<Vec<Point>>, StatusCode> {
    let conn = db(&state.db_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(id) = resolve_public(&conn, &id) else { return Err(StatusCode::NOT_FOUND) };
    let hours = match range.hours.unwrap_or(1) { 1 | 6 | 24 | 168 => range.hours.unwrap_or(1), _ => return Err(StatusCode::BAD_REQUEST) };
    let bucket = i64::from(hours) * 3600 / 120;
    let mut stmt = conn.prepare("SELECT MAX(ts),AVG(cpu),AVG(memory),AVG(disk),AVG(rx),AVG(tx) FROM samples
        WHERE node_id=? AND ts>=? GROUP BY ts / ? ORDER BY MAX(ts) DESC LIMIT 120")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = stmt.query_map(params![id, now()-i64::from(hours)*3600, bucket], |r| Ok(Point{ts:r.get(0)?,cpu:r.get(1)?,memory:r.get(2)?,disk:r.get(3)?,rx:r.get(4)?,tx:r.get(5)?}))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut result = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.reverse();
    Ok(Json(result))
}

/// agent 的上报连接。ID 格式不对、token 不对、不是 WebSocket 请求，统统回 404：
/// 以前 token 不对回 401、不是 WebSocket 回 400，等于告诉扫描器「这个路径上有东西」。
async fn agent(Path(id): Path<String>, State(state): State<App>, headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    let token = bearer(&headers).to_string();
    if !credentials_ok(&state.db_path, &id, &token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(ws) = ws else { return StatusCode::NOT_FOUND.into_response() };
    let observed = observed_ip(&headers);
    ws.on_upgrade(move |socket| receive(socket, state.db_path, id, token, observed)).into_response()
}

/// 旧版 agent 直接发裸 Metrics，没有 `t` 字段。留一条兼容路径，hub 可以先升级。
fn parse(payload: &str) -> Option<Up> {
    serde_json::from_str::<Up>(payload).ok()
        .or_else(|| serde_json::from_str::<Metrics>(payload).ok().map(Up::Metrics))
}

/// 这次上报能计入多少流量。只认「同一次开机、计数器只增不减」的那部分，其余情况一律只重新对基线。
///
/// 旧版在计数器变小时把当前读数整个计入。重启时这样算碰巧是对的，但网卡被拔掉、或者同一条安装命令
/// 装到了两台机器上、两个 agent 用同一个 token 来回顶替时，每次都会凭空多出一整个计数器——几百 GB，
/// 而且总流量只增不减，改不回来。两种出错方式的代价差几个数量级：重新对基线最多丢掉开机到首次上报
/// 之间的几十秒。
fn traffic_delta(old_boot: &str, boot: &str, last_rx: i64, last_tx: i64, rx: i64, tx: i64) -> (i64, i64) {
    let same_boot = old_boot == boot;
    if same_boot && rx >= last_rx && tx >= last_tx { (rx - last_rx, tx - last_tx) } else { (0, 0) }
}

fn store_metrics(tx: &Connection, id: &str, at: i64, m: &Metrics, last_sample: &mut i64) -> rusqlite::Result<()> {
    tx.execute("INSERT OR REPLACE INTO latest VALUES (?,?,?,?,?,?,?,?)",
        params![id,m.cpu,m.memory,m.disk,m.rx,m.tx,m.load,m.uptime as i64])?;
    if at - *last_sample >= 60 {
        tx.execute("INSERT OR REPLACE INTO samples VALUES (?,?,?,?,?,?,?,?,?)",
            params![id,at,m.cpu,m.memory,m.disk,m.rx,m.tx,m.load,m.uptime as i64])?;
        if let Some(latency) = m.latency_ms {
            // agent → hub 的握手延迟作为内置监控 0，没配任何监控时也有一条线可看。
            ping::write(tx, id, ping::HUB_MONITOR, at, Some(latency))?;
        }
        *last_sample = at;
    }
    if m.mem_total > 0 {
        tx.execute("INSERT OR REPLACE INTO details VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", params![
            id,m.mem_used as i64,m.mem_total as i64,m.swap_used as i64,
            m.swap_total as i64,m.disk_used as i64,m.disk_total as i64,
            m.load5,m.load15,m.os,m.kernel,m.arch,m.cpu_model,m.cpu_cores as i64,m.version])?;
    }
    if m.rx_bytes > 0 || m.tx_bytes > 0 {
        let old: Option<(i64,i64,i64,i64,String)> = tx.query_row(
            "SELECT last_rx,last_tx,total_rx,total_tx,boot FROM traffic WHERE node_id=?", [id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let (current_rx, current_tx) = (m.rx_bytes as i64, m.tx_bytes as i64);
        let (delta_rx, delta_tx, total_rx, total_tx) = match old {
            Some((last_rx, last_tx, total_rx, total_tx, boot)) => {
                let (rx, tx) = traffic_delta(&boot, &m.boot, last_rx, last_tx, current_rx, current_tx);
                (rx, tx, total_rx.saturating_add(rx), total_tx.saturating_add(tx))
            }
            None => (0, 0, 0, 0),
        };
        tx.execute("INSERT OR REPLACE INTO traffic(node_id,last_rx,last_tx,total_rx,total_tx,boot) VALUES (?,?,?,?,?,?)",
            params![id,current_rx,current_tx,total_rx,total_tx,m.boot])?;
        if delta_rx > 0 || delta_tx > 0 {
            tx.execute("INSERT INTO traffic_day VALUES (?,CAST(strftime('%s','now','localtime') AS INTEGER)/86400,?,?)
                ON CONFLICT(node_id,day) DO UPDATE SET rx=rx+excluded.rx,tx=tx+excluded.tx",
                params![id,delta_rx,delta_tx])?;
        }
    }
    tx.execute("UPDATE nodes SET last_seen=? WHERE id=?",params![at,id])?;
    Ok(())
}

/// 一条 agent 连接的生命周期。连接内复用一个 SQLite 连接：旧版每收一帧就 open 一次，
/// 200 个节点 5 秒一报就是每秒 40 次打开 + WAL 恢复，纯属浪费。
async fn receive(mut socket: WebSocket, db_path: Arc<str>, id: String, token: String, observed: String) {
    let Ok(mut conn) = db(&db_path) else { return };
    // 每次连上都记下出口地址：家宽换了 IP，重连时这里就是新的。
    let _ = conn.execute(
        "INSERT INTO node_net(node_id,observed,updated) VALUES (?,?,?) ON CONFLICT(node_id) DO UPDATE SET observed=excluded.observed,updated=excluded.updated",
        params![id, observed, now()]);
    let mut version = 0_u64;               // 与 CONFIG_VERSION 初值 1 不同，首轮即下发
    let mut allowed: HashMap<i64,i64> = HashMap::new();
    let mut last_ping: HashMap<i64,i64> = HashMap::new();
    let mut last_sample = 0_i64;
    let mut silent = 0_u32;
    loop {
        let current = CONFIG_VERSION.load(Ordering::Relaxed);
        if current != version {
            // 换过 token 就把这条连接踢掉：升级握手只验过一次，不然旧凭据会活得
            // 比它被吊销的那一刻更久。
            let stored: Option<String> = conn.query_row("SELECT token FROM nodes WHERE id=?", [&id], |r| r.get(0)).optional().ok().flatten();
            if !stored.is_some_and(|expected| equal_secret(&expected, &token)) { break }
            let Ok(tasks) = ping::tasks_for(&conn, &id) else { break };
            allowed = tasks.iter().map(|task| (task.i, task.s)).collect();
            last_ping.retain(|monitor,_| allowed.contains_key(monitor));
            let payload = serde_json::json!({"t":"tasks","tasks":tasks}).to_string();
            if socket.send(Message::Text(payload.into())).await.is_err() { break }
            version = current;
        }
        // 超时不是错误：agent 只是这一轮没话说，回到循环顶上看看有没有新任务要下发。
        // 连续四轮没有任何数据就认为对端已经死了，释放连接。
        let Ok(received) = tokio::time::timeout(Duration::from_secs(30), socket.recv()).await else {
            silent += 1;
            if silent >= 4 { break }
            continue;
        };
        silent = 0;
        let Some(Ok(message)) = received else { break };
        let Message::Text(payload) = message else { continue };
        if payload.len() > 8192 { break }
        let Some(up) = parse(&payload) else {
            // 带类型字段、只是这个版本不认识：多半是更新的 agent 加了新的报文，忽略，别断开连接。
            // 否则先升级 agent、后升级 hub 时，节点会反复掉线。
            let typed = serde_json::from_str::<serde_json::Value>(&payload).ok().is_some_and(|v| v.get("t").is_some());
            if typed { continue } else { break }
        };
        let at = now();
        let Ok(tx) = conn.transaction() else { break };
        let stored = match &up {
            Up::Metrics(metrics) => {
                if !valid(metrics) { break }
                store_metrics(&tx, &id, at, metrics, &mut last_sample)
            }
            Up::Ping { r } => ping::store(&tx, &id, at, r, &allowed, &mut last_ping),
            Up::Facts { v4, v6 } => tx.execute(
                "INSERT INTO node_net(node_id,ipv4,ipv6,updated) VALUES (?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET ipv4=excluded.ipv4,ipv6=excluded.ipv6,updated=excluded.updated",
                params![id, clean_ip(v4.as_deref(), false), clean_ip(v6.as_deref(), true), at]).map(|_| ()),
        };
        if stored.is_err() || tx.commit().is_err() { break }
    }
}

/// 清理独立于 agent 连接运行。旧版把清理写在收报文的循环里，所有节点都掉线时
/// 数据库就永远不会被清理——偏偏这正是磁盘要满的时候。
fn start_cleanup(path: Arc<str>) {
    tokio::spawn(async move {
        loop {
            if let Ok(conn) = db(&path) {
                let at = now();
                let _ = conn.execute("DELETE FROM samples WHERE ts<?", [at - 604800]);
                let _ = conn.execute("DELETE FROM ping WHERE ts<?", [at - ping::RETENTION]);
                let _ = conn.execute("DELETE FROM traffic_day WHERE day<CAST(strftime('%s','now','localtime') AS INTEGER)/86400-35", []);
                let _ = conn.execute("DELETE FROM admin_sessions WHERE expires<?", [at]);
                let _ = conn.execute("DELETE FROM login_attempts WHERE until<? AND first<?", params![at, at - admin::LOGIN_WINDOW]);
                // 把删出来的空页还给磁盘，再把 WAL 文件截短，数据库文件不会只涨不跌
                let _ = conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

/// 后台「上传恢复」把新库放在 <库>.restore，然后退出让 Docker（或 systemd）把程序拉起来。
/// 在这里、还没打开任何连接的时候换上去。换下来的旧库连同它的 WAL 留一份 <库>.before-restore，
/// 恢复错了还能换回来。
fn apply_pending_restore(path: &str) -> std::io::Result<()> {
    let pending = format!("{path}.restore");
    if !std::path::Path::new(&pending).exists() {
        return Ok(());
    }
    let old = format!("{path}.before-restore");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{old}{suffix}"));
    }
    if std::path::Path::new(path).exists() {
        std::fs::rename(path, &old)?;
    }
    let _ = std::fs::rename(format!("{path}-wal"), format!("{old}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
    std::fs::rename(&pending, path)?;
    println!("已换上上传的备份，原来的数据库留在 {old}");
    Ok(())
}

fn option(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone())
}

fn new_secret(bytes: usize) -> Result<String, Box<dyn Error>> {
    let mut random = vec![0_u8; bytes]; File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut text = String::with_capacity(bytes*2);
    for b in random { write!(&mut text, "{b:02x}")?; }
    Ok(text)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    // 数据库路径：--db 优先，其次环境变量 AVALON_DB（Docker 镜像里设成 /data/probe.db），最后是当前目录。
    let path = option(&args,"--db").or_else(|| std::env::var("AVALON_DB").ok()).unwrap_or_else(|| "probe.db".to_string());
    match args.get(1).map(String::as_str) {
        Some("add-node") => {
            let name = args.get(2).filter(|v| !v.starts_with("--")).ok_or("用法: probe-hub add-node 名称 [--private] [--db 路径]")?;
            let id = new_secret(8)?; let token = new_secret(32)?;
            init_db(&path)?;
            let conn = db(&path)?;
            conn.execute("INSERT INTO nodes(id,name,token,public,sort) VALUES (?,?,?,?,(SELECT COALESCE(MAX(sort),0)+1 FROM nodes))",params![id,name,token,!args.contains(&"--private".to_string())])?;
            ping::attach_auto(&conn, &id)?;
            println!("节点 ID: {id}\nToken: {token}\n仅显示一次，请妥善保管。");
        }
        Some("backup") => {
            // WAL 模式下直接 cp 可能拿到不一致的快照；VACUUM INTO 出来的是整份可用的库。
            let target = args.get(2).filter(|v| !v.starts_with("--")).ok_or("用法: probe-hub backup 目标路径 [--db 路径]")?;
            if std::path::Path::new(target) == std::path::Path::new(&path) {
                return Err("备份路径不能是数据库本身".into());
            }
            // VACUUM INTO 不肯覆盖已有文件；定期备份到同一个路径时先删掉上一份。
            let _ = std::fs::remove_file(target);
            db(&path)?.execute("VACUUM INTO ?", [target])?;
            println!("已备份到 {target}\n备份里含有管理员密码哈希、节点 token 和 Bot Token，请按凭据保管。");
        }
        Some("access") => {
            init_db(&path)?;
            let display = access::display_domain();
            println!("后台地址：https://你的主域名{}", access::admin_home());
            println!("展示页域名：{}", if display.is_empty() { "未设置（主域名同时做展示页）".to_string() } else { display });
            println!("公开页：{}", if access::public_enabled() { "开放" } else { "关闭" });
        }
        Some("unban") => {
            init_db(&path)?;
            let cleared = db(&path)?.execute("DELETE FROM login_attempts", [])?;
            println!("已解除全部登录封禁（{cleared} 条），马上可以再登录。");
        }
        Some("access-reset") => {
            init_db(&path)?;
            access::reset(&db(&path)?)?;
            println!("已恢复：后台地址 /admin/，不分展示页域名，公开页开放。\n重启后生效：docker compose restart");
        }
        Some("admin-setup") => {
            init_db(&path)?;
            let password = new_secret(16)?;
            admin::set_credentials(&path,admin::DEFAULT_USER,&password)?;
            println!("管理员账号: {}\n管理员密码（仅显示一次）: {password}\n请访问 https://你的主域名{} 登录，登录后在「账号」卡片里可以把两样都改掉。", admin::DEFAULT_USER, access::admin_home());
        }
        Some("serve") => {
            apply_pending_restore(&path)?;
            init_db(&path)?;
            admin::start_checks(Arc::from(path.clone()));
            start_cleanup(Arc::from(path.clone()));
            let listen = option(&args,"--listen").unwrap_or_else(|| "127.0.0.1:9911".to_string());
            let app = App { db_path: Arc::from(path), nodes: Arc::new(Mutex::new(None)) };
            let router = Router::new()
                .route("/", get(site::home)).route("/monitor",get(site::home)).route("/node/{id}",get(site::home)).route("/health",get(health))
                .route("/i/{key}",get(install_script))
                .route("/favicon.png",get(site::favicon)).route("/manifest.json",get(site::manifest)).route("/api/site",get(site::info)).route("/a/{id}/{name}",get(agent_binary))
                .route("/admin",get(|| async { Redirect::permanent(&access::admin_home()) }))
                .route("/admin/icon",get(site::favicon))
                .route("/admin/icon/{choice}",get(site::preview))
                .route("/admin/access",post(admin::save_access))
                .route("/admin/",get(admin::page))
                .route("/admin/login",post(admin::login)).route("/admin/logout",post(admin::logout))
                .route("/admin/account",post(admin::save_account))
                .route("/admin/sessions",post(admin::kick))
                .route("/admin/backup",post(admin::download_backup))
                .route("/admin/backup/restore",post(admin::restore_backup).layer(DefaultBodyLimit::max(admin::RESTORE_MAX)))
                .route("/admin/nodes",post(admin::add_node))
                .route("/admin/nodes/{id}",post(admin::edit_node))
                .route("/admin/nodes/{id}/move",post(admin::move_node))
                .route("/admin/monitors",post(admin::add_monitor))
                .route("/admin/monitors/{id}",post(admin::edit_monitor))
                .route("/admin/monitors/{id}/move",post(admin::move_monitor))
                .route("/admin/settings",post(admin::save_settings))
                .route("/admin/site",post(admin::save_site))
                .route("/admin/site/icon",post(admin::upload_icon))
                .route("/admin/test",post(admin::test_telegram))
                .route("/api/nodes",get(nodes)).route("/api/nodes/{id}/history",get(history))
                .route("/api/nodes/{id}/ping",get(ping::history))
                .route("/api/agent/{id}",get(agent))
                .route("/{*path}",get(theme_asset))
                .with_state(app.clone());
            // 访问判定包在整个路由外面：它要在路由之前把隐藏的后台地址改写回 /admin。
            // 放进 Router::layer 的话，改写发生在路由之后，就晚了。
            let service = tower_layer::Layer::layer(&middleware::from_fn_with_state(app, access::gate), router);
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("probe-hub listening on {listen}");
            axum::serve(listener, axum::ServiceExt::<Request>::into_make_service(service)).await?;
        }
        _ => { eprintln!("用法: probe-hub serve [--listen 地址] [--db 路径]\n       probe-hub add-node 名称 [--private] [--db 路径]\n       probe-hub admin-setup [--db 路径]\n       probe-hub backup 目标路径 [--db 路径]\n       probe-hub access [--db 路径]        查看后台地址和访问设置\n       probe-hub access-reset [--db 路径]  忘了后台地址或把自己关在外面时用\n       probe-hub unban [--db 路径]         解除登录封禁"); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_invalid_metrics() {
        let m: Metrics = serde_json::from_str(r#"{"cpu":101,"memory":10,"disk":20,"rx":0,"tx":0,"load":0,"uptime":1}"#).unwrap();
        assert!(!valid(&m));
        assert!(equal_secret("abc","abc"));
        assert!(!equal_secret("abc","ab"));
    }

    #[test]
    fn reads_both_frame_shapes() {
        let tagged = r#"{"t":"m","cpu":1,"memory":2,"disk":3,"rx":0,"tx":0,"load":0,"uptime":9}"#;
        assert!(matches!(parse(tagged), Some(Up::Metrics(_))));
        let legacy = r#"{"cpu":1,"memory":2,"disk":3,"rx":0,"tx":0,"load":0,"uptime":9}"#;
        assert!(matches!(parse(legacy), Some(Up::Metrics(_))));
        let results = r#"{"t":"p","r":[{"i":3,"ms":12.5},{"i":4,"ms":null}]}"#;
        assert!(matches!(parse(results), Some(Up::Ping{..})));
        assert!(parse("{\"t\":\"nope\"}").is_none());
    }

    #[test]
    fn traffic_counts_only_growth_within_one_boot() {
        assert_eq!(traffic_delta("a", "a", 100, 50, 160, 70), (60, 20), "同一次开机，正常累加");
        assert_eq!(traffic_delta("a", "b", 900_000, 900_000, 5_000, 4_000), (0, 0), "重启过：只重新对基线");
        assert_eq!(traffic_delta("a", "a", 900_000, 50, 10, 70), (0, 0), "网卡消失、读数变小：不把整个计数器算进去");
        assert_eq!(traffic_delta("", "", 100, 100, 150, 120), (50, 20), "旧版 agent 不报开机 ID，照样能累加");
    }

    #[test]
    fn addresses_are_validated_and_fall_back_to_the_exit() {
        assert_eq!(clean_ip(Some("192.168.1.2"), false), "", "私有地址不算");
        assert_eq!(clean_ip(Some("1.2.3.4"), true), "", "报在错误的族里不算");
        assert_eq!(clean_ip(Some("2409:8a1e:7c30:0001::1"), true), "2409:8a1e:7c30:1::1", "统一成标准写法");
        assert_eq!(clean_ip(Some("<script>"), false), "");
        let (v4, exit, v6, _) = shown_ips("", "2409::1", "203.0.113.9");
        assert_eq!((v4.as_str(), exit), ("", false), "出口地址是文档段，不算公网");
        assert_eq!(v6, "2409::1");
        let (v4, exit, _, _) = shown_ips("", "", "8.8.8.8");
        assert_eq!((v4.as_str(), exit), ("8.8.8.8", true), "家宽在 NAT 后面：用出口地址，并标明来源");
        let (v4, exit, _, _) = shown_ips("1.1.1.1", "", "8.8.8.8");
        assert_eq!((v4.as_str(), exit), ("1.1.1.1", false), "本机报了公网地址就用本机的");
    }

    #[test]
    fn unknown_frame_types_are_recognised() {
        assert!(parse(r#"{"t":"zz","x":1}"#).is_none(), "新报文这个版本不认识");
        assert!(serde_json::from_str::<serde_json::Value>(r#"{"t":"zz"}"#).unwrap().get("t").is_some(), "但能认出它带类型字段，于是忽略而不是断开");
        assert!(matches!(parse(r#"{"t":"f","v4":"1.1.1.1"}"#), Some(Up::Facts { .. })));
    }

    #[test]
    fn new_installs_start_with_the_public_page_closed() {
        let path = std::env::temp_dir().join(format!("avalon-fresh-{}.db", std::process::id())).to_str().unwrap().to_string();
        for suffix in ["", "-wal", "-shm"] { let _ = std::fs::remove_file(format!("{path}{suffix}")); }
        init_db(&path).unwrap();
        let read = || db(&path).unwrap().query_row("SELECT value FROM admin_settings WHERE key='public_page'", [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(read(), "0", "新装默认关闭");
        db(&path).unwrap().execute("UPDATE admin_settings SET value='1' WHERE key='public_page'", []).unwrap();
        init_db(&path).unwrap();
        assert_eq!(read(), "1", "打开之后再启动不会被改回去");
        for suffix in ["", "-wal", "-shm"] { let _ = std::fs::remove_file(format!("{path}{suffix}")); }
    }

    #[test]
    fn public_ids_are_stable_and_not_the_real_id() {
        let id = "0123456789abcdef";
        assert_eq!(public_id(id), public_id(id), "同一台机器编号不变，公开链接不失效");
        assert_ne!(public_id(id), id);
        assert_eq!(public_id(id).len(), 16);
        assert_ne!(public_id(id), public_id("0123456789abcdee"));
    }

    /// 一个节点只能写它自己被分配的监控，且写入按间隔限速。
    #[test]
    fn rejects_unassigned_and_flooded_results() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE ping (node_id TEXT,monitor_id INTEGER,ts INTEGER,latency REAL,PRIMARY KEY(node_id,monitor_id,ts));").unwrap();
        let samples: Vec<ping::Sample> = serde_json::from_str(r#"[{"i":1,"ms":5},{"i":99,"ms":5}]"#).unwrap();
        let allowed = HashMap::from([(1_i64, 60_i64)]);
        let mut last = HashMap::new();
        ping::store(&conn, "abc", 1000, &samples, &allowed, &mut last).unwrap();
        ping::store(&conn, "abc", 1005, &samples, &allowed, &mut last).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM ping", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "未分配的监控和超频的重复上报都不该入库");
    }
}
