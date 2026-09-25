//! 延迟监控：由 agent 在各自的机器上发起 TCP 探测，hub 只负责下发任务、收结果、画图。
//!
//! 与旧版的区别：旧版在 hub 上连目标，测的是「hub → 目标」，所有节点画出来是同一条线。
//! 现在每个节点各自连接，测的是「这台机器 → 目标」，这才是多节点监控。
use super::*;
use std::collections::HashMap;

/// 单个节点最多运行的监控数，与官方一致。
pub(super) const MAX_PER_NODE: i64 = 64;
/// 监控总数上限。防止后台误操作把整个节点群变成压测工具。
pub(super) const MAX_MONITORS: i64 = 200;
pub(super) const MIN_INTERVAL: i64 = 5;
pub(super) const MAX_INTERVAL: i64 = 3600;
/// 延迟明细保留时长。公开页只画 24 小时，留 3 天足够排查；
/// 100 节点 × 5 监控 × 60 秒间隔约 220 万行、150 MB 左右，再长就要考虑降采样。
pub(super) const RETENTION: i64 = 3 * 86400;
/// 内置探测：agent 到 hub 的握手延迟。没有配置任何监控时，图上也有一条线。
pub(super) const HUB_MONITOR: i64 = 0;

/// 下发给 agent 的任务。字段名取单字母：200 个节点每次重连都要发一遍。
#[derive(Serialize)]
pub(super) struct Task {
    pub(super) i: i64,
    h: String,
    p: u16,
    pub(super) s: i64,
}

/// agent 回报的一次探测结果。`ms` 为 null 表示丢包（超时或被拒绝）。
#[derive(Deserialize)]
pub(super) struct Sample {
    i: i64,
    #[serde(default)]
    ms: Option<f64>,
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS monitors (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            host TEXT NOT NULL,
            port INTEGER NOT NULL,
            interval INTEGER NOT NULL DEFAULT 60,
            auto_join INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1);
         CREATE TABLE IF NOT EXISTS monitor_nodes (
            monitor_id INTEGER NOT NULL,
            node_id TEXT NOT NULL,
            PRIMARY KEY(monitor_id,node_id));
         CREATE INDEX IF NOT EXISTS monitor_nodes_by_node ON monitor_nodes(node_id);
         CREATE TABLE IF NOT EXISTS ping (
            node_id TEXT NOT NULL,
            monitor_id INTEGER NOT NULL,
            ts INTEGER NOT NULL,
            latency REAL NOT NULL,
            PRIMARY KEY(node_id,monitor_id,ts)) WITHOUT ROWID;",
    )
}

/// 这个节点该跑哪些监控。超过上限的直接截断，而不是让 agent 自己扛。
pub(super) fn tasks_for(conn: &Connection, node: &str) -> rusqlite::Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT m.id,m.host,m.port,m.interval FROM monitors m
         JOIN monitor_nodes n ON n.monitor_id=m.id
         WHERE n.node_id=? AND m.enabled=1 ORDER BY m.id LIMIT ?",
    )?;
    // 先落到局部变量再返回：直接把 query_map(...).collect() 当尾表达式，
    // 迭代器这个临时值会比 stmt 活得久，借用检查过不去（E0597）。
    let tasks = stmt.query_map(params![node, MAX_PER_NODE], |r| {
        Ok(Task {
            i: r.get(0)?,
            h: r.get(1)?,
            p: r.get::<_, i64>(2)?.clamp(1, 65535) as u16,
            s: r.get::<_, i64>(3)?.clamp(MIN_INTERVAL, MAX_INTERVAL),
        })
    })?
    .collect();
    tasks
}

/// 新节点接入时，把「新节点自动加入」的监控挂上去。
pub(super) fn attach_auto(conn: &Connection, node: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO monitor_nodes(monitor_id,node_id)
         SELECT id,? FROM monitors WHERE auto_join=1 AND enabled=1
         LIMIT (SELECT ?-COUNT(*) FROM monitor_nodes WHERE node_id=?)",
        params![node, MAX_PER_NODE, node],
    )?;
    Ok(())
}

/// 写入一批结果。时间戳用 hub 的时钟：agent 的时钟不可信，也不该信。
///
/// `allowed` 是这个节点当前被分配的监控（id → 间隔），不在里面的一律丢弃——
/// 否则一个被攻破的 agent 可以往任意节点的曲线里灌数据。`last` 记录每个监控上次
/// 落库的时间，按间隔的一半限速，防止 agent 高频刷写把库撑爆。
pub(super) fn store(
    tx: &Connection,
    node: &str,
    at: i64,
    samples: &[Sample],
    allowed: &HashMap<i64, i64>,
    last: &mut HashMap<i64, i64>,
) -> rusqlite::Result<()> {
    for sample in samples.iter().take(MAX_PER_NODE as usize) {
        let Some(interval) = allowed.get(&sample.i) else { continue };
        if sample.ms.is_some_and(|v| !v.is_finite() || !(0.0..=10_000.0).contains(&v)) {
            continue;
        }
        let gap = (interval / 2).max(2);
        if last.get(&sample.i).is_some_and(|previous| at - previous < gap) {
            continue;
        }
        last.insert(sample.i, at);
        write(tx, node, sample.i, at, sample.ms)?;
    }
    Ok(())
}

/// 单条写入。`-1` 表示丢包；用负数而不是 NULL，聚合时少一层判空。
pub(super) fn write(
    tx: &Connection,
    node: &str,
    monitor: i64,
    at: i64,
    latency: Option<f64>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO ping(node_id,monitor_id,ts,latency) VALUES (?,?,?,?)",
        params![node, monitor, at, latency.unwrap_or(-1.0)],
    )?;
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct Window {
    hours: Option<u32>,
    points: Option<u32>,
}

/// 公开图表接口。只给监控名称和延迟，目标地址和运行节点留在后台。
pub(super) async fn history(
    Path(id): Path<String>,
    Query(window): Query<Window>,
    State(state): State<App>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 「Agent → Hub」那条线反映的是各节点到 hub 的网络距离，等于变相透露 hub 在哪。只给管理员看。
    let full = access::admin_view(&state.db_path, &headers);
    let hours = match window.hours.unwrap_or(24) {
        hours @ (1 | 6 | 24 | 168) => i64::from(hours),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let points = i64::from(window.points.unwrap_or(720).clamp(60, 1440));
    let conn = db(&state.db_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(id) = resolve_public(&conn, &id) else { return Err(StatusCode::NOT_FOUND) };

    // 一个桶至少 30 秒：更细的桶只会让手机多画几千个点，看不出区别。
    let bucket = (hours * 3600 / points).max(30);
    let mut stmt = conn
        .prepare(
            "SELECT monitor_id,MAX(ts),
                AVG(CASE WHEN latency>=0 THEN latency END),
                MIN(CASE WHEN latency>=0 THEN latency END),
                MAX(CASE WHEN latency>=0 THEN latency END),
                SUM(latency<0),COUNT(*)
             FROM ping WHERE node_id=? AND ts>=?
             GROUP BY monitor_id, ts/? ORDER BY monitor_id, 2",
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = stmt
        .query_map(params![id, now() - hours * 3600, bucket], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let round = |v: f64| (v * 10.0).round() / 10.0;
    let mut series = Vec::new();
    // 整个窗口的丢包率必须在这里算：每个桶的 loss 已经是百分比，前端再平均会把
    // 只有一个样本的桶和有十二个样本的桶算得一样重。
    let mut totals: HashMap<i64, (i64, i64)> = HashMap::new();
    for row in rows {
        let Ok((monitor, ts, average, low, high, lost, count)) = row else { continue };
        if monitor == HUB_MONITOR && !full {
            continue;
        }
        let entry = totals.entry(monitor).or_insert((0, 0));
        entry.0 += lost;
        entry.1 += count;
        let mut point = serde_json::json!({
            "task_id": monitor, "ts": ts, "latency": average.map(round)
        });
        if let (Some(low), Some(high)) = (low, high) {
            if high - low > 0.05 {
                point["band"] = serde_json::json!([round(low), round(high)]);
            }
        }
        if lost > 0 && count > 0 {
            point["loss"] = serde_json::json!(round(100.0 * lost as f64 / count as f64));
        }
        series.push(point);
    }

    let mut names: HashMap<i64, String> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id,name FROM monitors") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
            for row in rows.flatten() {
                names.insert(row.0, row.1);
            }
        };
    }
    let mut probes = serde_json::Map::new();
    let mut loss = serde_json::Map::new();
    for (monitor, (lost, count)) in &totals {
        let name = if *monitor == HUB_MONITOR {
            "Agent → Hub".to_string()
        } else {
            names.get(monitor).cloned().unwrap_or_else(|| format!("监控 {monitor}"))
        };
        probes.insert(monitor.to_string(), name.into());
        if *count > 0 {
            loss.insert(monitor.to_string(), (100.0 * *lost as f64 / *count as f64).into());
        }
    }
    Ok(Json(serde_json::json!({
        "metrics": [], "ping": series, "probes": probes, "loss": loss
    })))
}
