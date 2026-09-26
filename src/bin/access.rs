//! 谁能看到什么。所有请求在路由之前先过这里，决定放行、改写还是回 404。
//!
//! 三个设置：
//! - **展示页域名**：填了以后，这个域名上只有公开页；后台、agent 连接、安装脚本一律 404。
//!   其余域名（主域名）上，公开页只给已登录的人看，没登录一律 404。
//! - **公开页开关**：关掉后展示页域名也全部 404，主域名上照样只有登录的人能看。
//! - **后台地址**：后台页面和后台接口整体搬到 `/<自定义>`，原来的 `/admin` 回 404。
//!   内部路由表不动，这里把 `/<自定义>/…` 改写成 `/admin/…` 再交给路由。
//!   隐藏地址只给已登录的管理员看，任何公开接口里都不出现——存进公开可读的设置里，
//!   任何访客都能读到。
//!
//! 拒绝一律回空的 404，跟「这个地址不存在」一模一样，响应头也一样，扫描器分不出来。
use super::*;
use axum::http::Method;
use std::sync::RwLock;

pub(crate) const DEFAULT_ADMIN: &str = "admin";

/// 这些是公开页、agent、安装脚本自己要用的路径开头，后台不能占。
const RESERVED: [&str; 13] = [
    "api", "assets", "i", "a", "node", "monitor", "health", "favicon.png", "manifest.json",
    "lxgwwenkai.css", "install.sh", "agent", "index.html",
];

struct Settings {
    admin: String,
    display: String,
    public: bool,
}

static SETTINGS: RwLock<Settings> = RwLock::new(Settings { admin: String::new(), display: String::new(), public: true });

/// 后台地址，不带斜杠，例如 `admin` 或 `x7k2`。
pub(crate) fn admin_path() -> String {
    let admin = SETTINGS.read().map(|s| s.admin.clone()).unwrap_or_default();
    if admin.is_empty() { DEFAULT_ADMIN.to_string() } else { admin }
}
/// 后台首页，例如 `/x7k2/`。
pub(crate) fn admin_home() -> String {
    format!("/{}/", admin_path())
}
/// 后台前缀，例如 `/x7k2`，用来改写页面里的链接。
pub(crate) fn admin_base() -> String {
    format!("/{}", admin_path())
}
pub(crate) fn display_domain() -> String {
    SETTINGS.read().map(|s| s.display.clone()).unwrap_or_default()
}
pub(crate) fn public_enabled() -> bool {
    SETTINGS.read().map(|s| s.public).unwrap_or(true)
}

fn read(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM admin_settings WHERE key=?", [key], |r| r.get(0)).optional()
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    let admin = read(conn, "admin_path")?.unwrap_or_default();
    let display = read(conn, "display_domain")?.unwrap_or_default();
    let public = read(conn, "public_page")?.map_or(true, |value| value != "0");
    if let Ok(mut settings) = SETTINGS.write() {
        *settings = Settings { admin, display, public };
    }
    Ok(())
}

pub(super) fn save(conn: &Connection, admin: &str, display: &str, public: bool) -> rusqlite::Result<()> {
    for (key, value) in [("admin_path", admin), ("display_domain", display), ("public_page", if public { "1" } else { "0" })] {
        conn.execute(
            "INSERT INTO admin_settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )?;
    }
    init(conn)
}

/// 命令行的逃生口：忘了后台地址、或者把自己关在外面时，恢复成 /admin、不分域名、公开页打开。
pub(super) fn reset(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM admin_settings WHERE key IN ('admin_path','display_domain','public_page')", [])?;
    init(conn)
}

/// 4–48 位小写字母、数字、`-`、`_`，一级路径，不能占用保留路径。
pub(super) fn valid_admin_path(input: &str) -> Option<String> {
    let path = input.trim().trim_matches('/').to_ascii_lowercase();
    let shape = (4..=48).contains(&path.len())
        && path.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    (shape && !RESERVED.contains(&path.as_str())).then_some(path)
}

/// 空表示不分域名。允许顺手粘进来的 `https://` 和末尾斜杠。
pub(super) fn valid_domain(input: &str) -> Option<String> {
    let domain = input.trim().to_ascii_lowercase();
    let domain = domain.strip_prefix("https://").or_else(|| domain.strip_prefix("http://")).unwrap_or(&domain);
    let domain = domain.trim_end_matches('/').to_string();
    if domain.is_empty() {
        return Some(domain);
    }
    let shape = domain.len() <= 253
        && domain.contains('.')
        && domain.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
        && !domain.contains("..");
    shape.then_some(domain)
}

#[derive(Debug, PartialEq)]
enum Route {
    /// 后台，带上改写后的内部路径。
    Admin(String),
    /// 用了自定义后台地址时，字面上的 /admin。
    BlockedAdmin,
    Health,
    Agent,
    Install,
    /// 公开页和它的接口、静态文件，以及一切未知路径——默认按公开内容对待，没权限就 404。
    Public,
}

fn classify(path: &str, admin: &str) -> Route {
    let prefix = format!("/{admin}");
    if path == prefix || path.starts_with(&format!("{prefix}/")) {
        return Route::Admin(format!("/admin{}", &path[prefix.len()..]));
    }
    if path == "/admin" || path.starts_with("/admin/") {
        return Route::BlockedAdmin;
    }
    if path == "/health" {
        return Route::Health;
    }
    if path.starts_with("/api/agent/") {
        return Route::Agent;
    }
    if path.starts_with("/i/") || path.starts_with("/a/") {
        return Route::Install;
    }
    Route::Public
}

/// 每个请求的访问判定。
pub(super) async fn gate(State(state): State<App>, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let host = admin::request_hosts(request.headers()).into_iter().next().unwrap_or_default();
    let display = display_domain();
    let on_display = !display.is_empty() && host == display;
    // 经过反代或 Tunnel 的请求一定带其中一个头；都没有才是容器里的健康检查。
    let local = !request.headers().contains_key("x-forwarded-for") && !request.headers().contains_key("cf-connecting-ip");
    let route = classify(&path, &admin_path());
    let allowed = match route {
        Route::Admin(internal) => {
            if on_display {
                false
            } else {
                let query = request.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
                match format!("{internal}{query}").parse() {
                    Ok(uri) => {
                        *request.uri_mut() = uri;
                        true
                    }
                    Err(_) => false,
                }
            }
        }
        Route::BlockedAdmin => false,
        Route::Health => local,
        // 这两类只有 GET：别的方法回 405 也算露了底，统一 404。凭据对不对由各自的处理函数判断，不对同样 404。
        Route::Agent | Route::Install => !on_display && request.method() == Method::GET,
        // 公开内容只读。别的方法回 405 等于承认「这个路径存在」。
        Route::Public if request.method() != Method::GET && request.method() != Method::HEAD => false,
        Route::Public => {
            if on_display {
                public_enabled()
            } else if display.is_empty() && public_enabled() {
                true
            } else {
                logged_in(&state, request.headers())
            }
        }
    };
    let admin = allowed && request.uri().path().starts_with("/admin");
    let mut response = if allowed { next.run(request).await } else { StatusCode::NOT_FOUND.into_response() };
    // 后台以外，所有 4xx 都换成和「不存在」一模一样的空 404。框架自带的英文报错、400、405
    // 都会透露后面跑的是什么程序、哪个路径是真的。
    if !admin && response.status().is_client_error() {
        response = StatusCode::NOT_FOUND.into_response();
    }
    harden(&mut response, admin);
    response
}

/// 这个请求是不是来自已登录的管理员。展示页域名上永远按访客对待，哪怕带着登录状态。
pub(crate) fn admin_view(db_path: &str, headers: &HeaderMap) -> bool {
    let host = admin::request_hosts(headers).into_iter().next().unwrap_or_default();
    let display = display_domain();
    if !display.is_empty() && host == display {
        return false;
    }
    db(db_path).ok().is_some_and(|conn| admin::session(headers, &conn).is_some())
}

fn logged_in(state: &App, headers: &HeaderMap) -> bool {
    db(&state.db_path).ok().is_some_and(|conn| admin::session(headers, &conn).is_some())
}

/// 所有响应（包括上面直接回的 404）都带同一组头，免得「被拦下的」和「真不存在的」看起来不一样。
/// 后台页面上有节点 token，额外禁止缓存和被 iframe 嵌入。
fn harden(response: &mut Response, admin: bool) {
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    // 空 404 没有内容类型，又带着上面的 nosniff，Safari 会把它当成文件弹出下载。
    // 统一标成纯文本，浏览器显示一片空白。
    if !headers.contains_key(axum::http::header::CONTENT_TYPE) {
        headers.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    }
    // 只走 HTTPS。hub 永远在 HTTPS 反代后面；本机用 http 联调时浏览器会忽略这个头。
    headers.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    if admin {
        headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
        headers.insert("cache-control", HeaderValue::from_static("no-store"));
        // 后台一行脚本都没有，干脆禁止执行任何脚本：哪天有东西被注入进页面，浏览器也不会跑它。
        // 只允许本站的图片、页面里的内联样式、提交给本站的表单。
        headers.insert("content-security-policy", HeaderValue::from_static(
            "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_404_is_plain_text_not_a_download() {
        let mut missing = StatusCode::NOT_FOUND.into_response();
        harden(&mut missing, false);
        assert_eq!(missing.headers().get("content-type").unwrap(), "text/plain; charset=utf-8");
        let mut page = ([("content-type", "text/html; charset=utf-8")], "x").into_response();
        harden(&mut page, true);
        assert_eq!(page.headers().get("content-type").unwrap(), "text/html; charset=utf-8", "已有的不覆盖");
    }

    #[test]
    fn admin_pages_cannot_run_scripts() {
        let mut page = StatusCode::OK.into_response();
        harden(&mut page, true);
        let csp = page.headers().get("content-security-policy").unwrap().to_str().unwrap();
        assert!(csp.contains("default-src 'none'") && !csp.contains("script-src"), "没有任何地方放行脚本");
        assert!(page.headers().contains_key("strict-transport-security"));
        let mut public = StatusCode::OK.into_response();
        harden(&mut public, false);
        assert!(!public.headers().contains_key("content-security-policy"), "公开页的主题要跑脚本，不加");
    }

    #[test]
    fn hidden_admin_path_rewrites_and_blocks_the_old_one() {
        assert_eq!(classify("/x7k2", "x7k2"), Route::Admin("/admin".into()));
        assert_eq!(classify("/x7k2/nodes/abc", "x7k2"), Route::Admin("/admin/nodes/abc".into()));
        assert_eq!(classify("/admin/", "x7k2"), Route::BlockedAdmin, "原来的地址失效");
        assert_eq!(classify("/admin/login", "x7k2"), Route::BlockedAdmin, "后台接口跟着搬走，不留原地");
        assert_eq!(classify("/x7k2x", "x7k2"), Route::Public, "前缀要整段匹配");
        assert_eq!(classify("/admin/", "admin"), Route::Admin("/admin/".into()), "默认地址照常");
        assert_eq!(classify("/wp-login.php", "x7k2"), Route::Public, "未知路径按公开内容对待，没权限就 404");
        assert_eq!(classify("/api/agent/abc", "x7k2"), Route::Agent);
        assert_eq!(classify("/i/abc", "x7k2"), Route::Install);
    }

    #[test]
    fn validates_settings() {
        assert_eq!(valid_admin_path(" /X7k2-Ops/ "), Some("x7k2-ops".into()));
        assert!(valid_admin_path("abc").is_none(), "太短");
        assert!(valid_admin_path("api").is_none());
        assert!(valid_admin_path("assets").is_none(), "保留路径");
        assert!(valid_admin_path("a/b").is_none(), "只允许一级");
        assert_eq!(valid_domain("https://Status.Example.com/"), Some("status.example.com".into()));
        assert_eq!(valid_domain(""), Some(String::new()));
        assert!(valid_domain("localhost").is_none());
        assert!(valid_domain("a..b.com").is_none());
        assert!(valid_domain("evil.com/x").is_none());
    }
}
