//! 站点名称和图标。公开页左上角、浏览器标签页、后台页头和 Telegram 通知前缀都用它。
//!
//! 图标四选一：内置的「机箱」（默认）、内置的「机箱加工具」、上传的图片、表情或单个字。
//! 四样都一直保留，后台单选切换，互不覆盖：切到内置图标不会删掉上传过的图片。
//! 上传只收 PNG / JPG / GIF / WebP / ICO，按文件头识别，不信扩展名；SVG 不收，
//! 因为它能内嵌脚本。下发时再加一层 CSP sandbox，即便有人直接打开图标地址也执行不了任何东西。
use super::*;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use sha2::{Digest, Sha256};
use std::sync::RwLock;

pub(crate) const DEFAULT_NAME: &str = "Avalon";
pub(super) const ICON_MAX: usize = 256 * 1024;
/// 内置图标，256×256，已裁掉多余留白。
const CHASSIS: &[u8] = include_bytes!("../../icons/chassis.png");
const TOOLS: &[u8] = include_bytes!("../../icons/tools.png");
/// 可选的图标，(键, 后台显示的名字)。
pub(super) const CHOICES: [(&str, &str); 4] = [("chassis", "机箱"), ("tools", "机箱加工具"), ("upload", "上传的图片"), ("emoji", "表情")];

/// 名称每个请求都要用（页面标题、后台页头、通知），放内存里，改的时候同步刷新。
static NAME: RwLock<String> = RwLock::new(String::new());

pub(crate) fn name() -> String {
    let current = NAME.read().map(|name| name.clone()).unwrap_or_default();
    if current.is_empty() { DEFAULT_NAME.to_string() } else { current }
}

fn remember(name: String) {
    if let Ok(mut cached) = NAME.write() {
        *cached = name;
    }
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS site_icon (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            mime TEXT NOT NULL,
            data BLOB NOT NULL,
            etag TEXT NOT NULL);",
    )?;
    let stored: Option<String> = conn
        .query_row("SELECT value FROM admin_settings WHERE key='site_name'", [], |r| r.get(0))
        .optional()?;
    remember(stored.unwrap_or_default());
    Ok(())
}

pub(super) fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn short_hash(data: &[u8]) -> String {
    Sha256::digest(data).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 1–30 个字，不含控制字符。存原文，输出时各自转义。
pub(super) fn valid_name(input: &str) -> Option<String> {
    let name = input.trim();
    let count = name.chars().count();
    ((1..=30).contains(&count) && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// 一个表情或一两个字。国旗、肤色、组合表情由好几个码点拼成，所以按字节数限，不按「一个字符」限。
pub(super) fn valid_emoji(input: &str) -> Option<String> {
    let emoji = input.trim();
    let ok = !emoji.is_empty()
        && emoji.len() <= 32
        && emoji.chars().count() <= 8
        && !emoji.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '&' | '"' | '\''));
    ok.then(|| emoji.to_string())
}

fn emoji_svg(emoji: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text x="50" y="55" font-size="82" text-anchor="middle" dominant-baseline="middle">{}</text></svg>"#,
        escape(emoji)
    )
}

/// 按文件头认格式。扩展名和浏览器报的类型都可以随便写，文件头骗不了。
pub(super) fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else {
        None
    }
}

pub(super) fn save_name(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO admin_settings(key,value) VALUES('site_name',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [name],
    )?;
    remember(name.to_string());
    Ok(())
}

fn set_choice(conn: &Connection, choice: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO admin_settings(key,value) VALUES('site_icon_choice',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [choice],
    )?;
    Ok(())
}

/// 存表情并切过去。上传过的图片留着。
pub(super) fn save_emoji(conn: &Connection, emoji: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO admin_settings(key,value) VALUES('site_emoji',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [emoji],
    )?;
    set_choice(conn, "emoji")
}

/// 存上传的图片并切过去。再上传一张就替换这一张。
pub(super) fn save_icon(conn: &Connection, mime: &str, data: &[u8]) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO site_icon(id,mime,data,etag) VALUES (1,?,?,?)",
        params![mime, data, short_hash(data)],
    )?;
    set_choice(conn, "upload")
}

/// 切换图标。选「上传的图片」「表情」要先有东西可用，否则返回 false、什么都不改。
pub(super) fn choose(conn: &Connection, choice: &str) -> rusqlite::Result<bool> {
    let usable = match choice {
        "chassis" | "tools" => true,
        "upload" => uploaded(conn).is_some(),
        "emoji" => load_emoji(conn).is_some(),
        _ => false,
    };
    if usable {
        set_choice(conn, choice)?;
    }
    Ok(usable)
}

/// 删除上传的图片或表情。删掉的正在用就切回机箱，免得图标变成空的。
pub(super) fn delete(conn: &Connection, what: &str) -> rusqlite::Result<()> {
    match what {
        "upload" => { conn.execute("DELETE FROM site_icon", [])?; }
        "emoji" => { conn.execute("DELETE FROM admin_settings WHERE key='site_emoji'", [])?; }
        _ => return Ok(()),
    }
    if current_choice(conn) == what {
        set_choice(conn, "chassis")?;
    }
    Ok(())
}

/// 旧版的「恢复默认图标」按钮：现在就是切回机箱，上传过的东西不删。
pub(super) fn reset_icon(conn: &Connection) -> rusqlite::Result<()> {
    set_choice(conn, "chassis")
}

fn uploaded(conn: &Connection) -> Option<(String, Vec<u8>, String)> {
    conn.query_row("SELECT mime,data,etag FROM site_icon WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
        .ok()
        .flatten()
}

fn load_emoji(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT value FROM admin_settings WHERE key='site_emoji'", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .filter(|emoji| !emoji.is_empty())
}

/// 当前选中的是哪个。没选过的按旧版的规则推断（有上传用上传，有表情用表情），升级后图标不会突然变。
pub(super) fn current_choice(conn: &Connection) -> String {
    let chosen: Option<String> = conn
        .query_row("SELECT value FROM admin_settings WHERE key='site_icon_choice'", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    match chosen {
        Some(choice) => choice,
        None if uploaded(conn).is_some() => "upload".to_string(),
        None if load_emoji(conn).is_some() => "emoji".to_string(),
        None => "chassis".to_string(),
    }
}

/// 后台给「上传的图片」「表情」两个选项判断要不要显示。
pub(super) fn has_upload(conn: &Connection) -> bool {
    uploaded(conn).is_some()
}
pub(super) fn emoji(conn: &Connection) -> Option<String> {
    load_emoji(conn)
}

/// 某个选项的图：(类型, 内容, ETag)。「上传的图片」「表情」没东西时退回机箱。
fn icon_for(conn: &Connection, choice: &str) -> (String, Vec<u8>, String) {
    let chassis = || ("image/png".to_string(), CHASSIS.to_vec(), "\"chassis-1\"".to_string());
    match choice {
        "tools" => ("image/png".to_string(), TOOLS.to_vec(), "\"tools-1\"".to_string()),
        "upload" => uploaded(conn).map(|(mime, data, etag)| (mime, data, format!("\"{etag}\""))).unwrap_or_else(chassis),
        "emoji" => match load_emoji(conn) {
            Some(emoji) => {
                let svg = emoji_svg(&emoji);
                let etag = format!("\"e{}\"", short_hash(svg.as_bytes()));
                ("image/svg+xml".to_string(), svg.into_bytes(), etag)
            }
            None => chassis(),
        },
        _ => chassis(),
    }
}

/// 把图发出去。带 ETag：浏览器每次问一声，没变就回 304，换了图标刷新即见。
fn send_icon(headers: &HeaderMap, (mime, body, etag): (String, Vec<u8>, String)) -> Response {
    let etag_value = HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"chassis-1\""));
    let fresh = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str());
    let mut response = if fresh { StatusCode::NOT_MODIFIED.into_response() } else { body.into_response() };
    let out = response.headers_mut();
    out.insert(CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/png")));
    out.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    out.insert(ETAG, etag_value);
    out.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    response
}

/// 后台单选框旁边的预览图。只给登录的管理员。
pub(super) async fn preview(Path(choice): Path<String>, State(state): State<App>, headers: HeaderMap) -> Response {
    if !access::admin_view(&state.db_path, &headers) || !CHOICES.iter().any(|(key, _)| *key == choice) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(conn) = db(&state.db_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    send_icon(&headers, icon_for(&conn, &choice))
}

/// `/favicon.png`。路径名沿用主题写死的那个，内容按当前选中的图标走。
pub(super) async fn favicon(State(state): State<App>, headers: HeaderMap) -> Response {
    let icon = match db(&state.db_path) {
        Ok(conn) => icon_for(&conn, &current_choice(&conn)),
        Err(_) => ("image/png".to_string(), CHASSIS.to_vec(), "\"chassis-1\"".to_string()),
    };
    send_icon(&headers, icon)
}

/// 公开页读站点名的地方。后台入口只告诉已登录的管理员：隐藏的后台地址绝不出现在公开接口里，
/// 展示页域名上也不给（那里本来就没有后台）。
pub(super) async fn info(State(state): State<App>, headers: HeaderMap) -> impl IntoResponse {
    let admin_url = access::admin_view(&state.db_path, &headers).then(access::admin_home);
    ([(CACHE_CONTROL, "no-store")], Json(serde_json::json!({ "site_name": name(), "admin_url": admin_url })))
}

/// 首页。把主题里写死的 <title>Monitor</title> 换成站点名，JS 还没跑起来时标签页就是对的。
/// 页面本身不缓存：它引用的 JS 文件名带指纹，页面一换，浏览器就会去取新的那份。
pub(super) async fn home() -> impl IntoResponse {
    let page = include_str!("../../theme/dist/index.html").replacen(
        "<title>Monitor</title>",
        &format!("<title>{}</title>", escape(&name())),
        1,
    );
    ([(CACHE_CONTROL, "no-cache")], Html(page))
}

/// 「添加到主屏幕」时用的名字。
pub(super) async fn manifest() -> impl IntoResponse {
    let name = name();
    let body = serde_json::json!({
        "name": name, "short_name": name, "start_url": "/", "scope": "/",
        "display": "standalone", "background_color": "#31363b",
        "icons": [{ "src": "/favicon.png", "sizes": "any" }]
    });
    ([(CONTENT_TYPE, "application/manifest+json")], body.to_string())
}

/// 最小的 multipart/form-data 解析，只为后台上传图标这一处。为它引入一个 multipart 依赖不值当。
/// 返回 (字段名, 内容) 列表；格式不对返回 None。最多认 8 个字段。
pub(super) fn multipart(content_type: &str, body: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))?
        .trim_matches('"');
    if boundary.is_empty() || boundary.len() > 200 {
        return None;
    }
    let delimiter = format!("--{boundary}").into_bytes();
    let mut rest = &body[find(body, &delimiter)? + delimiter.len()..];
    let mut fields = Vec::new();
    for _ in 0..8 {
        if rest.starts_with(b"--") {
            return Some(fields);
        }
        rest = rest.strip_prefix(b"\r\n")?;
        let end = find(rest, &delimiter)?;
        let part = rest[..end].strip_suffix(b"\r\n")?;
        let split = find(part, b"\r\n\r\n")?;
        let head = std::str::from_utf8(&part[..split]).ok()?;
        let name = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-disposition:"))?
            .split(';')
            .map(str::trim)
            .find_map(|piece| piece.strip_prefix("name="))?
            .trim_matches('"')
            .to_string();
        fields.push((name, part[split + 4..].to_vec()));
        rest = &rest[end + delimiter.len()..];
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_browser_upload() {
        let body = b"------B\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\nabc123\r\n\
------B\r\nContent-Disposition: form-data; name=\"icon\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\r\n\x1a\n\x00\x01\r\n\
------B--\r\n";
        let fields = multipart("multipart/form-data; boundary=----B", body).unwrap();
        assert_eq!(fields[0], ("csrf".to_string(), b"abc123".to_vec()));
        assert_eq!(fields[1].0, "icon", "filename= 不能被当成 name=");
        assert_eq!(sniff(&fields[1].1), Some("image/png"), "文件内容里的 \\r\\n 原样保留");
        assert!(multipart("multipart/form-data", body).is_none(), "没有 boundary");
    }

    #[test]
    fn recognises_images_by_content_not_name() {
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None, "SVG 能内嵌脚本，不收");
        assert_eq!(sniff(b"<html>"), None);
    }

    #[test]
    fn icon_choices_switch_without_losing_anything() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);").unwrap();
        init(&conn).unwrap();
        assert_eq!(current_choice(&conn), "chassis", "默认是机箱");
        assert_eq!(icon_for(&conn, "chassis").1, CHASSIS.to_vec());
        assert!(!choose(&conn, "upload").unwrap(), "还没上传过，不能选");
        save_icon(&conn, "image/png", b"\x89PNG\r\n\x1a\nfake").unwrap();
        assert_eq!(current_choice(&conn), "upload", "上传完自动切过去");
        assert!(choose(&conn, "tools").unwrap());
        assert_eq!(icon_for(&conn, &current_choice(&conn)).1, TOOLS.to_vec());
        assert!(has_upload(&conn), "切到内置图标，上传过的图片还在");
        assert!(choose(&conn, "upload").unwrap(), "随时能切回来");
        assert!(!choose(&conn, "../etc").unwrap());
        delete(&conn, "upload").unwrap();
        assert!(!has_upload(&conn));
        assert_eq!(current_choice(&conn), "chassis", "删掉正在用的，切回机箱");
        save_emoji(&conn, "📡").unwrap();
        assert!(choose(&conn, "tools").unwrap());
        delete(&conn, "emoji").unwrap();
        assert_eq!(current_choice(&conn), "tools", "删掉没在用的，不影响当前图标");
    }

    #[test]
    fn validates_name_and_emoji() {
        assert_eq!(valid_name("  我的面板 "), Some("我的面板".to_string()));
        assert!(valid_name("").is_none());
        assert!(valid_name(&"长".repeat(31)).is_none());
        assert!(valid_emoji("🇯🇵").is_some(), "国旗由两个码点组成");
        assert!(valid_emoji("📡").is_some());
        assert!(valid_emoji("<b>").is_none());
        assert!(emoji_svg("&").contains("&amp;"));
    }
}
