//! 资源监控：后台「通知」卡片里的提醒规则。
//!
//! 一套规则管一组机器，一台机器最多归一套（alert_rule_nodes 以 node_id 为主键，数据库自己保证）。
//! 不在任何规则里的机器什么提醒都不发，掉线也不发。
//!
//! 一套规则里的项：
//! - 掉线宽限（分钟）：留空按 5；
//! - 到期提前（天）：1–7，留空不发到期提醒；
//! - 流量（%）：本期用量到这个比例发一次，到 100% 再发一次，每个周期各一次；
//! - CPU / 内存 / 磁盘（%）、负载（%，1 分钟负载 ÷ 核数）：最近 10 个采样（每分钟一个）
//!   里有 8 个超过才算超标，有 8 个回到阈值以下才算恢复，各发一条，中间不重复。
//!
//! 这里只有建表、读规则和纯判断；巡检和发通知在 admin.rs。

use super::*;

pub(super) const DEFAULT_GRACE: i64 = 5;
pub(super) const MAX_GRACE: i64 = 60;
pub(super) const MAX_EXPIRE: i64 = 7;
pub(super) const MAX_LOAD: i64 = 1000;
pub(super) const MAX_RULES: i64 = 50;
/// 看最近几个采样，其中几个超过才算超标（恢复同理）。采样每分钟一个，也就是「10 分钟里 8 分钟」。
pub(super) const WINDOW: usize = 10;
pub(super) const NEED: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Rule {
    pub(super) id: i64,
    pub(super) name: String,
    pub(super) grace: i64,
    pub(super) expire: Option<i64>,
    pub(super) traffic: Option<i64>,
    pub(super) cpu: Option<i64>,
    pub(super) memory: Option<i64>,
    pub(super) disk: Option<i64>,
    pub(super) load: Option<i64>,
}

/// 建表。老库升级上来（以前还没有规则表）时顺手建一套「默认」：
/// 掉线宽限沿用以前的设置，到期提前 7 天，其余留空；以前勾了「掉线/到期通知」的机器都放进去。
/// 升级完和升级前的行为一样。新装的库也会得到这一套，新加的机器自动进排第一的规则。
pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    let fresh: bool = conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='alert_rules')", [], |r| r.get(0))?;
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS alert_rules (id INTEGER PRIMARY KEY, name TEXT NOT NULL, sort INTEGER NOT NULL DEFAULT 0,
           grace INTEGER NOT NULL DEFAULT 5, expire INTEGER, traffic INTEGER, cpu INTEGER, memory INTEGER, disk INTEGER, load INTEGER);
         CREATE TABLE IF NOT EXISTS alert_rule_nodes (node_id TEXT PRIMARY KEY, rule_id INTEGER NOT NULL);",
    )?;
    if fresh {
        let grace = tx
            .query_row("SELECT value FROM admin_settings WHERE key='offline_grace'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|v| v.parse::<i64>().ok())
            .map_or(DEFAULT_GRACE, |m| m.clamp(0, MAX_GRACE));
        tx.execute("INSERT INTO alert_rules(name,sort,grace,expire) VALUES('默认',1,?,7)", [grace])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT OR IGNORE INTO alert_rule_nodes(node_id,rule_id)
             SELECT n.id,? FROM nodes n LEFT JOIN node_config c ON c.node_id=n.id WHERE COALESCE(c.notify,1)=1",
            [id],
        )?;
    }
    tx.commit()
}

fn read(r: &rusqlite::Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: r.get(0)?, name: r.get(1)?, grace: r.get(2)?, expire: r.get(3)?, traffic: r.get(4)?,
        cpu: r.get(5)?, memory: r.get(6)?, disk: r.get(7)?, load: r.get(8)?,
    })
}

/// 全部规则，按后台的顺序。
pub(super) fn all(conn: &Connection) -> rusqlite::Result<Vec<Rule>> {
    let mut stmt = conn.prepare("SELECT id,name,grace,expire,traffic,cpu,memory,disk,load FROM alert_rules ORDER BY sort,id")?;
    let rows = stmt.query_map([], read)?;
    rows.collect()
}

/// 每台机器归哪套规则：节点 id → 规则。不在表里的就是不归任何规则。
pub(super) fn by_node(conn: &Connection) -> rusqlite::Result<HashMap<String, Rule>> {
    let rules: HashMap<i64, Rule> = all(conn)?.into_iter().map(|rule| (rule.id, rule)).collect();
    let mut stmt = conn.prepare("SELECT node_id,rule_id FROM alert_rule_nodes")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (node, rule) = row?;
        if let Some(rule) = rules.get(&rule) {
            out.insert(node, rule.clone());
        }
    }
    Ok(out)
}

/// 新加的机器放进排第一的那套规则，和以前「新节点默认开通知」一样。一套规则都没有就不放。
pub(super) fn attach_new(conn: &Connection, node: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO alert_rule_nodes(node_id,rule_id) SELECT ?,id FROM alert_rules ORDER BY sort,id LIMIT 1",
        [node],
    )?;
    Ok(())
}

/// 把一套规则的机器写成给定的集合。已经归别的规则的机器插不进来（主键冲突被忽略），
/// 所以就算两个页面同时保存，一台机器也不会同时在两套规则里。
pub(super) fn assign(tx: &Connection, rule: i64, nodes: &[&str]) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM alert_rule_nodes WHERE rule_id=?", [rule])?;
    for node in nodes.iter().take(1000) {
        if node.len() != 16 || !node.bytes().all(|b| b.is_ascii_hexdigit()) { continue; }
        tx.execute(
            "INSERT OR IGNORE INTO alert_rule_nodes(node_id,rule_id) SELECT id,? FROM nodes WHERE id=?",
            params![rule, node],
        )?;
    }
    Ok(())
}

/// 输入框里的数：留空是 None，填了要在范围里。
pub(super) fn number(text: &str, min: i64, max: i64) -> Result<Option<i64>, ()> {
    let text = text.trim();
    if text.is_empty() { return Ok(None); }
    match text.parse::<i64>() {
        Ok(v) if (min..=max).contains(&v) => Ok(Some(v)),
        _ => Err(()),
    }
}

/// 超标 / 恢复的判断。values 是最近的采样（最多 WINDOW 个），was 是上一次的状态。
/// 超标要 NEED 个达到阈值（填 100 就是跑满），恢复要 NEED 个回到阈值以下；够不上就保持原状，
/// 数值在阈值上下晃也不会来回发。采样不够（刚上线、刚重启）时也保持原状。
pub(super) fn judge(was: bool, values: &[f64], limit: f64) -> bool {
    let over = values.iter().filter(|v| **v >= limit).count();
    let under = values.len() - over;
    if was { under < NEED } else { over >= NEED }
}

/// 流量到了哪一级：0 没到，1 到了阈值，2 用完了（100%）。没设额度的是 0。
pub(super) fn traffic_level(used: i64, limit: i64, threshold: i64) -> u8 {
    if limit <= 0 { return 0; }
    let percent = used as f64 * 100.0 / limit as f64;
    if percent >= 100.0 { 2 } else if percent >= threshold as f64 { 1 } else { 0 }
}

/// 按这台机器的计费方式算本期用量，和公开页主题的 monthUsage 一致。
pub(super) fn metered(mode: &str, rx: i64, tx: i64) -> i64 {
    match mode {
        "up" => tx,
        "down" => rx,
        "max" => rx.max(tx),
        _ => rx + tx,
    }
}

/// 同一轮的多条提醒合成一条。Telegram 一条最多 4096 字，条数多了截掉，末尾说一声还有几条。
pub(super) fn bundle(lines: &[String]) -> Option<String> {
    const MAX_LINES: usize = 40;
    if lines.is_empty() { return None; }
    let mut text = lines.iter().take(MAX_LINES).cloned().collect::<Vec<_>>().join("\n");
    if lines.len() > MAX_LINES {
        text.push_str(&format!("\n……另有 {} 条", lines.len() - MAX_LINES));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(over: usize, under: usize) -> Vec<f64> {
        std::iter::repeat(95.0).take(over).chain(std::iter::repeat(10.0).take(under)).collect()
    }

    #[test]
    fn spikes_stay_quiet_and_sustained_load_alerts() {
        assert!(!judge(false, &sample(3, 7), 90.0), "冲高 3 分钟不算");
        assert!(!judge(false, &sample(7, 3), 90.0));
        assert!(judge(false, &sample(8, 2), 90.0), "10 分钟里 8 分钟超才算");
        assert!(!judge(false, &sample(7, 0), 90.0), "采样不够 8 个时不报");
        assert!(judge(false, &[90.0; 10], 90.0), "达到阈值就算，填 100 才能报跑满");
        assert!(!judge(false, &[89.9; 10], 90.0));
    }

    #[test]
    fn recovery_needs_the_same_majority() {
        assert!(judge(true, &sample(3, 7), 90.0), "只有 7 个正常，还算超标，不来回发");
        assert!(!judge(true, &sample(2, 8), 90.0), "8 个正常才算恢复");
        assert!(judge(true, &sample(0, 5), 90.0), "采样不够时保持原状");
    }

    #[test]
    fn traffic_levels() {
        let gb = 1 << 30;
        assert_eq!(traffic_level(79 * gb, 100 * gb, 80), 0);
        assert_eq!(traffic_level(80 * gb, 100 * gb, 80), 1);
        assert_eq!(traffic_level(100 * gb, 100 * gb, 80), 2);
        assert_eq!(traffic_level(99 * gb, 100 * gb, 100), 0, "阈值填 100 就只在用完时发");
        assert_eq!(traffic_level(5 * gb, 0, 80), 0, "没设额度不算");
    }

    #[test]
    fn traffic_follows_the_billing_mode() {
        assert_eq!(metered("sum", 3, 4), 7);
        assert_eq!(metered("", 3, 4), 7);
        assert_eq!(metered("max", 3, 4), 4);
        assert_eq!(metered("up", 3, 4), 4);
        assert_eq!(metered("down", 3, 4), 3);
    }

    #[test]
    fn input_ranges() {
        assert_eq!(number("", 1, 100), Ok(None));
        assert_eq!(number(" 80 ", 1, 100), Ok(Some(80)));
        assert_eq!(number("0", 1, 100), Err(()));
        assert_eq!(number("101", 1, 100), Err(()));
        assert_eq!(number("8.5", 1, 100), Err(()));
    }

    #[test]
    fn many_lines_are_cut() {
        assert_eq!(bundle(&[]), None);
        let lines: Vec<String> = (0..45).map(|i| format!("l{i}")).collect();
        let text = bundle(&lines).unwrap();
        assert!(text.ends_with("……另有 5 条"));
        assert_eq!(text.lines().count(), 41);
    }

    #[test]
    fn upgrade_puts_notified_nodes_into_a_default_rule_and_keeps_one_rule_per_node() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (id TEXT PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE node_config (node_id TEXT PRIMARY KEY, notify INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO nodes VALUES ('aaaaaaaaaaaaaaaa','a'),('bbbbbbbbbbbbbbbb','b'),('cccccccccccccccc','c');
             INSERT INTO node_config VALUES ('bbbbbbbbbbbbbbbb',0);
             INSERT INTO admin_settings VALUES ('offline_grace','3');",
        ).unwrap();
        init(&conn).unwrap();
        let rules = all(&conn).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!((rules[0].name.as_str(), rules[0].grace, rules[0].expire), ("默认", 3, Some(7)));
        let members = by_node(&conn).unwrap();
        assert!(members.contains_key("aaaaaaaaaaaaaaaa"), "没有设置过的按开着通知算");
        assert!(!members.contains_key("bbbbbbbbbbbbbbbb"), "关了通知的不放进去");
        assert!(members.contains_key("cccccccccccccccc"));

        init(&conn).unwrap();
        assert_eq!(all(&conn).unwrap().len(), 1, "再启动不会再建一套");

        conn.execute("INSERT INTO alert_rules(name,sort) VALUES('大鸡',2)", []).unwrap();
        let big = conn.last_insert_rowid();
        assign(&conn, big, &["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb", "not-a-node"]).unwrap();
        let members = by_node(&conn).unwrap();
        assert_eq!(members["aaaaaaaaaaaaaaaa"].name, "默认", "已经归别的规则的机器抢不过来");
        assert_eq!(members["bbbbbbbbbbbbbbbb"].name, "大鸡");

        conn.execute("INSERT INTO nodes VALUES ('dddddddddddddddd','d')", []).unwrap();
        attach_new(&conn, "dddddddddddddddd").unwrap();
        assert_eq!(by_node(&conn).unwrap()["dddddddddddddddd"].name, "默认", "新机器进排第一的规则");
    }
}
