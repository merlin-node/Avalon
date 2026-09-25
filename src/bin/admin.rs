use super::*;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::Form;
use axum::http::header::{COOKIE, HOST, ORIGIN, SET_COOKIE};
use axum::response::{Redirect, Response};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;

const COOKIE_NAME: &str = "pulse_admin";
const SESSION_LIFETIME: i64 = 7 * 86400;
/// admin-setup 建出来的账号。老数据库里没有这一项，也按它算。
pub(super) const DEFAULT_USER: &str = "admin";
const PASSWORD_MIN: usize = 12;
/// 登录满这么久才算旧设备，才能踢人、改账号密码。
const TRUST_AFTER: i64 = 86400;
/// 上传恢复的大小上限。几十台机器、七天历史也就几十 MB；更大的库用命令行恢复。
pub(super) const RESTORE_MAX: usize = 128 * 1024 * 1024;
/// 同一来源地址 15 分钟内错 5 次即锁定该地址。旧版把计数存成一个全局值，
/// 任何人持续错密码就能把管理员自己挡在门外。
const LOGIN_WINDOW: i64 = 900;
const LOGIN_TRIES: i64 = 5;
static LOGIN_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

fn digest(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}
fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes=vec![0;n]; File::open("/dev/urandom")?.read_exact(&mut bytes)?; Ok(bytes)
}
fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
/// 登录限流的分桶依据。取 X-Forwarded-For 最右边那个：它是紧挨着 hub 的那一层
/// 反代自己追加的，客户端伪造不了；反代没透传就退回单一桶，行为与旧版一致。
fn client(headers:&HeaderMap)->String {
    headers.get("x-forwarded-for").and_then(|v|v.to_str().ok())
        .and_then(|value|value.rsplit(',').next()).map(str::trim)
        .filter(|v|!v.is_empty()&&v.len()<=45&&v.bytes().all(|b|b.is_ascii_hexdigit()||b==b'.'||b==b':'))
        .unwrap_or("local").to_string()
}

/// 当前账号。没存过就是 admin：老数据库升级上来时不用迁移，照样进得去。
pub(super) fn username(conn:&Connection)->String {
    let name:String=conn.query_row("SELECT value FROM admin_settings WHERE key='username'",[],|r|r.get(0)).unwrap_or_default();
    if name.is_empty() {DEFAULT_USER.to_string()} else {name}
}
/// 账号只收 ASCII：它要跟输入框里的内容做常量时间比较，多字节字符没有好处。
fn valid_username(input:&str)->Option<String> {
    let name=input.trim();
    let shape=(1..=32).contains(&name.len())
        && name.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'.'||b==b'_'||b==b'-'||b==b'@');
    shape.then(||name.to_string())
}

/// 写账号，顺带换密码（password 为 None 就只改账号）。两种情况都清空所有会话：
/// 凭据变了还留着旧会话，等于改了个寂寞。
fn store_account(path:&str,name:&str,password:Option<&str>)->Result<(),Box<dyn Error>> {
    // argon2 的错误类型不实现 std::error::Error（要开 std 特性才有），
    // 所以不能直接用 ? 转成 Box<dyn Error>，先转成字符串。
    let hash=match password {
        Some(password)=>{
            let salt=SaltString::encode_b64(&random_bytes(16)?).map_err(|e|e.to_string())?;
            Some(Argon2::default().hash_password(password.as_bytes(),&salt).map_err(|e|e.to_string())?.to_string())
        }
        None=>None,
    };
    let mut conn=db(path)?;
    let tx=conn.transaction()?;
    tx.execute("INSERT INTO admin_settings(key,value) VALUES('username',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [name])?;
    if let Some(hash)=&hash {
        tx.execute("INSERT INTO admin_settings(key,value) VALUES('password',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [hash])?;
    }
    tx.execute("DELETE FROM admin_sessions",[])?;
    tx.commit()?;
    Ok(())
}
pub(super) fn set_credentials(path:&str,name:&str,password:&str)->Result<(),Box<dyn Error>> {
    store_account(path,name,Some(password))
}

fn password_ok(conn:&Connection,password:&str)->bool {
    let hash:String=conn.query_row("SELECT value FROM admin_settings WHERE key='password'",[],|r|r.get(0)).unwrap_or_default();
    PasswordHash::new(&hash).ok().is_some_and(|h|Argon2::default().verify_password(password.as_bytes(),&h).is_ok())
}
/// 账号和密码一起判。账号错也照样把 argon2 跑完再合并结果：提前返回的话，
/// 这个请求会明显比密码错的那次快，快慢本身就把「账号错了」说出去了。
fn credentials_ok(conn:&Connection,name:&str,password:&str)->bool {
    let name_ok=equal_secret(&username(conn),name.trim());
    let pass_ok=password_ok(conn,password);
    name_ok&&pass_ok
}

/// 登录限流。后台改密码那个表单也收当前密码，等于第二个登录入口，共用这一套；
/// 不然它就是一条绕开限流慢慢试密码的路。
fn locked(conn:&Connection,source:&str)->bool {
    let until:Option<i64>=conn.query_row("SELECT until FROM login_attempts WHERE source=?",[source],|r|r.get(0)).optional().ok().flatten();
    until.is_some_and(|until|until>now())
}
fn record_failure(conn:&Connection,source:&str) {
    let attempt:Option<(i64,i64)>=conn.query_row("SELECT failures,first FROM login_attempts WHERE source=?",[source],|r|Ok((r.get(0)?,r.get(1)?))).optional().ok().flatten();
    let (failures,first)=match attempt {
        Some((failures,first)) if now()-first<LOGIN_WINDOW => (failures+1,first),
        _ => (1,now()),
    };
    let until=if failures>=LOGIN_TRIES {now()+LOGIN_WINDOW} else {0};
    let _=conn.execute("INSERT INTO login_attempts(source,failures,first,until) VALUES (?,?,?,?) ON CONFLICT(source) DO UPDATE SET failures=excluded.failures,first=excluded.first,until=excluded.until",params![source,failures,first,until]);
    let _=conn.execute("DELETE FROM login_attempts WHERE until<? AND first<?",params![now(),now()-LOGIN_WINDOW]);
}
fn clear_failures(conn:&Connection,source:&str) {
    let _=conn.execute("DELETE FROM login_attempts WHERE source=?",[source]);
}

/// 从浏览器标识粗略认出设备，只为在列表里分得清哪台是哪台，认不出也无妨。
fn device(ua:&str)->String {
    // 顺序有讲究：iPhone 的标识里也有 Mac OS X，安卓的里也有 Linux，Edge 的里也有 Chrome 和 Safari。
    const SYSTEMS:[(&str,&str);6]=[("iPhone","iPhone"),("iPad","iPad"),("Android","Android"),("Mac OS X","Mac"),("Windows","Windows"),("Linux","Linux")];
    const BROWSERS:[(&str,&str);5]=[("Edg/","Edge"),("Firefox/","Firefox"),("CriOS/","Chrome"),("Chrome/","Chrome"),("Safari/","Safari")];
    match (pick_name(&SYSTEMS,ua),pick_name(&BROWSERS,ua)) {
        (Some(system),Some(browser))=>format!("{system} · {browser}"),
        (Some(one),None)|(None,Some(one))=>one.to_string(),
        (None,None)=>"未知设备".to_string(),
    }
}
fn pick_name(table:&[(&'static str,&'static str)],ua:&str)->Option<&'static str> {
    table.iter().find(|(key,_)|ua.contains(*key)).map(|(_,name)|*name)
}

/// 这个会话能不能踢人、改账号密码。登录满 24 小时的可以；不满的，只有在没有比它
/// 更早的会话时才可以（比如刚跑完 admin-setup，只有你一个在线）。
/// 防的是别人偷到密码登进来，反手把你踢掉或者改掉密码。
fn can_manage(conn:&Connection,hash:&str)->bool {
    let created:i64=conn.query_row("SELECT created FROM admin_sessions WHERE hash=?",[hash],|r|r.get(0)).unwrap_or_else(|_|now());
    if now()-created>=TRUST_AFTER {return true;}
    let older:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM admin_sessions WHERE created<? AND expires>?)",params![created,now()],|r|r.get(0)).unwrap_or(true);
    !older
}

/// 当前请求的登录会话。登录 cookie 从这一版起对全站有效（主域名上登录过才能看展示页），
/// 旧版发的是只对 /admin 有效的，两份可能同时存在，所以挨个看，哪份有效用哪份。
pub(super) fn session(headers:&HeaderMap,conn:&Connection) -> Option<(String,String)> {
    let cookies=headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').filter_map(|v|v.trim().split_once('=')).filter(|(key,_)|*key==COOKIE_NAME).find_map(|(_,token)|{
        if token.len()!=64 || !token.bytes().all(|b|b.is_ascii_hexdigit()) {return None;}
        let stored: Option<i64>=conn.query_row("SELECT expires FROM admin_sessions WHERE hash=?",[digest(token)],|r|r.get(0)).optional().ok().flatten();
        (stored?>now()).then(||(digest(token),digest(&format!("{token}:csrf"))))
    })
}
/// 这个请求实际访问的站点，可能有两个来源。有的反代（面板生成的 Caddy 配置常见）
/// 会把 Host 改写成上游地址 127.0.0.1:9911，这时原始域名在 X-Forwarded-Host 里——
/// Caddy 默认就会带上。hub 只监听回环，这两个头只可能由本机反代写入；而跨站的
/// 表单提交设不了自定义请求头，所以多认一个 X-Forwarded-Host 不削弱 CSRF 防护。
pub(super) fn request_hosts(headers:&HeaderMap)->Vec<String> {
    ["x-forwarded-host",HOST.as_str()].iter()
        .filter_map(|name|headers.get(*name).and_then(|v|v.to_str().ok()))
        // 经过多层代理时是逗号分隔的列表，第一个是浏览器最初访问的。
        .filter_map(|value|value.split(',').next())
        .map(|host|{let host=host.trim().to_ascii_lowercase(); host.strip_suffix(":443").map(str::to_string).unwrap_or(host)})
        .collect()
}
/// 同源检查：浏览器声明的来源（Origin）必须就是它正在访问的站点。
/// 除了 https 之外也接受回环地址上的 http，否则本机联调根本进不了后台。
fn same_origin(headers:&HeaderMap)->bool {
    if headers.get("sec-fetch-site").and_then(|v|v.to_str().ok()).is_some_and(|v|v=="cross-site") {return false;}
    let Some(origin)=headers.get(ORIGIN).and_then(|v|v.to_str().ok()) else {return false;};
    let origin=origin.to_ascii_lowercase();
    let hosts=request_hosts(headers);
    if let Some(host)=origin.strip_prefix("https://") {
        let host=host.strip_suffix(":443").unwrap_or(host);
        return hosts.iter().any(|candidate|candidate==host);
    }
    if let Some(host)=origin.strip_prefix("http://") {
        let loopback=host.starts_with("127.0.0.1")||host.starts_with("localhost")||host.starts_with("[::1]");
        return loopback && hosts.iter().any(|candidate|candidate==host);
    }
    false
}
fn authorized(headers:&HeaderMap,conn:&Connection,csrf:&str)->bool {
    same_origin(headers) && session(headers,conn).is_some_and(|(_,expected)|equal_secret(&expected,csrf))
}
fn esc(s:&str)->String {s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;").replace('\'',"&#39;")}
fn frame(body:&str)->Html<String> {
    let site=site::escape(&site::name());
    // 页面里的后台链接和表单一律写成 /admin/…，在这里换成当前的后台地址。
    // 图标也走后台地址下的那份：用了展示页域名时，没登录的人在主域名上拿不到公开的 /favicon.png。
    let base=access::admin_base();
    let body=body.replace("\"/admin/",&format!("\"{base}/")).replace("'/admin/",&format!("'{base}/"));
    Html(format!(r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{site} · 管理</title><style>
:root{{color-scheme:light}}*{{box-sizing:border-box}}body{{margin:0;background:#f8fafb;color:#242b30;font:14px/1.6 system-ui,-apple-system,sans-serif}}
header{{background:#fff;border-bottom:1px solid #d9dfe3;padding:14px max(16px,calc((100vw - 1050px)/2));display:flex;align-items:center;gap:24px}}a{{color:#386a9c}}header a{{text-decoration:none}}main{{max-width:1050px;margin:28px auto;padding:0 16px 60px}}h1{{font-size:21px;margin:0 0 15px}}h2{{font-size:16px;margin:0 0 12px}}.card{{background:#fff;border:1px solid #d9dfe3;padding:20px;margin:12px 0}}.card>summary{{font-size:16px;font-weight:700;margin:0}}.card[open]>summary{{margin:0 0 12px}}.row{{display:flex;gap:8px;align-items:flex-start}}.row>details{{flex:1;min-width:0}}.move{{display:flex;gap:4px;margin:0;flex:none}}.move button{{padding:2px 10px;min-width:34px}}.ghost{{visibility:hidden}}.grow{{flex:1;min-width:0}}button:disabled{{opacity:.45;cursor:not-allowed}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(215px,1fr));gap:12px}}label{{display:block;color:#626e76;font-size:13px}}input,select{{display:block;width:100%;border:1px solid #cdd6dc;border-radius:0;padding:9px 10px;background:#fff;color:#242b30;font:inherit;margin-top:5px}}input[type=checkbox]{{width:auto;display:inline-block;margin-right:5px}}input[type=radio]{{width:auto;display:inline-block;margin:0 5px 0 0;vertical-align:-1px}}.icons{{display:flex;flex-wrap:wrap;gap:10px;margin-top:6px}}.icon-choice{{display:flex;flex-direction:column;align-items:center;gap:6px;border:1px solid #d9dfe3;background:#fff;padding:10px 12px;cursor:pointer;min-width:96px;color:#242b30}}.icon-choice img{{object-fit:contain}}.upload{{margin-top:22px;padding-top:16px;border-top:1px dashed #e3e8eb}}button.link{{background:none;color:#b04747;padding:0;font-size:13px}}.icon-choice:has(input:checked){{border-color:#346d9b;box-shadow:0 0 0 1px #346d9b}}button{{background:#346d9b;color:white;border:0;border-radius:0;padding:10px 16px;cursor:pointer;font:inherit}}button.secondary{{background:#e8edef;color:#26333b}}button.danger{{background:#fff;color:#b04747;border:1px solid #e0c4c4}}form{{margin:0}}.actions{{display:flex;gap:10px;align-items:center;margin-top:15px;flex-wrap:wrap}}small,.muted{{color:#6d797f}}.node{{border-top:1px solid #e3e8eb;padding:15px 0}}.node:first-child{{border-top:0}}details>summary{{cursor:pointer;font-weight:650;font-size:15px}}code{{overflow-wrap:anywhere}}.ok{{color:#329356}}.down{{color:#c05252}}.picker{{display:grid;grid-template-columns:repeat(auto-fill,minmax(180px,1fr));gap:4px 12px;max-height:230px;overflow:auto;border:1px solid #e3e8eb;padding:10px;margin-top:6px}}.picker label{{color:#242b30}}.brand{{display:flex;align-items:center;gap:8px}}.brand img{{object-fit:contain}}.cmd{{display:block;white-space:pre-wrap;word-break:break-all;background:#f2f5f7;border:1px solid #d9dfe3;padding:12px;margin:10px 0;font:13px/1.55 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color:#242b30;-webkit-user-select:all;user-select:all}}@media(max-width:600px){{main{{margin:12px auto}}.card{{padding:14px}}}}</style><header><strong class="brand"><img src="{base}/icon" alt="" width="20" height="20">{site}</strong><a href="/">首页</a><span class="muted">管理</span></header><main>{body}</main></html>"#))
}
fn failure(code:StatusCode,message:&str)->Response {(code,frame(&format!("<p>{}</p><p><a href='/admin/'>返回管理</a></p>",esc(message)))).into_response()}

/// 一张可折叠的卡片。用的是浏览器自带的 details，不需要 JS。
fn card(id:&str,title:&str,open:bool,body:&str)->String {
    format!("<details class=\"card\" id=\"{id}\"{}><summary>{title}</summary>{body}</details>",if open {" open"} else {""})
}
/// 保存完回后台首页。带上 ?open=… 让刚动过的那张卡片自己展开，同名的 #锚点
/// 再让浏览器滚到它那儿——保存一次就得重新点开一遍，太烦。
fn back(open:&str)->Response {back_to(open,open)}
/// 展开 open 那张卡片，滚到 anchor 那一行。排序时用：卡片要开着，那一行本身不用展开。
fn back_to(open:&str,anchor:&str)->Response {
    Redirect::to(&format!("{}?open={open}#{anchor}",access::admin_home())).into_response()
}

/// 每行右侧的 ↑↓。到头的那一个换成看不见的占位，免得上下两行的按钮错开。
fn mover(action:&str,csrf:&str,index:usize,count:usize)->String {
    let up=if index>0 {r#"<button class="secondary" name="dir" value="up" aria-label="上移">↑</button>"#} else {r#"<button class="secondary ghost" disabled>↑</button>"#};
    let down=if index+1<count {r#"<button class="secondary" name="dir" value="down" aria-label="下移">↓</button>"#} else {r#"<button class="secondary ghost" disabled>↓</button>"#};
    format!(r#"<form class="move" method="post" action="{action}"><input type="hidden" name="csrf" value="{csrf}">{up}{down}</form>"#)
}
/// 把 target 和上面（或下面）那个对调，返回新的整张顺序；到头了或找不到就是 None。
fn swapped<T:PartialEq+Clone>(order:&[T],target:&T,up:bool)->Option<Vec<T>> {
    let i=order.iter().position(|x|x==target)?;
    let j=if up {i.checked_sub(1)?} else {i+1};
    if j>=order.len() {return None;}
    let mut next=order.to_vec();
    next.swap(i,j);
    Some(next)
}
/// 移动一行，然后在同一个事务里把整张表按 1、2、3…重新编号。
/// 只改两行的 sort 不够：老数据全是 0，两个 0 对调还是 0。
/// table 和 order_by 只来自代码里的常量，不来自请求。
fn move_row(conn:&mut Connection,table:&str,order_by:&str,id:&str,up:bool)->rusqlite::Result<bool> {
    let tx=conn.transaction()?;
    let order:Vec<String>={
        let mut stmt=tx.prepare(&format!("SELECT CAST(id AS TEXT) FROM {table} ORDER BY {order_by}"))?;
        let rows=stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let Some(next)=swapped(&order,&id.to_string(),up) else {return Ok(false)};
    for (index,row) in next.iter().enumerate() {
        tx.execute(&format!("UPDATE {table} SET sort=? WHERE id=?"),params![index as i64+1,row])?;
    }
    tx.commit()?;
    Ok(true)
}
/// ?open= 的值最后要写进 HTML 的 id 和 open 判断里，先把字符集收死。
fn opened(value:Option<&str>)->String {
    let value=value.unwrap_or("");
    let shape=value.len()<=40 && value.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'-');
    if shape {value.to_string()} else {String::new()}
}

/// axum 的 Form 走 serde_urlencoded，同名键只保留一个，装不下一组复选框。
/// 监控表单因此自己解析 body；form_urlencoded 本来就在依赖树里（url 的依赖）。
fn fields(body:&str)->Vec<(String,String)> {
    form_urlencoded::parse(body.as_bytes()).into_owned().take(400).collect()
}
fn one<'a>(fields:&'a [(String,String)],key:&str)->&'a str {
    fields.iter().find(|(k,_)|k==key).map(|(_,v)|v.as_str()).unwrap_or("")
}
fn many<'a>(fields:&'a [(String,String)],key:&str)->Vec<&'a str> {
    fields.iter().filter(|(k,_)|k==key).map(|(_,v)|v.as_str()).collect()
}

/// `host:port`，IPv6 写成 `[2606:4700:4700::1111]:443`。
/// 方括号外不允许出现冒号：否则 "1.1.1.1:443" 会被整个当成主机名存进去，
/// 这正是旧版 valid_host 的漏洞。
fn parse_target(input:&str)->Option<(String,u16)> {
    let input=input.trim();
    let (host,port)=if let Some(rest)=input.strip_prefix('[') {
        let (host,tail)=rest.split_once(']')?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {return None;}
        (host.to_string(),tail.strip_prefix(':')?)
    } else {
        let (host,port)=input.rsplit_once(':')?;
        if host.contains(':') {return None;}
        (host.to_string(),port)
    };
    let port:u16=port.parse().ok()?;
    let shape=!host.is_empty() && host.len()<=253
        && host.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'.'||b==b'-'||b==b':')
        && !host.contains("..") && !host.starts_with('-') && !host.ends_with('-');
    (port>0 && shape).then_some((host,port))
}

/// 改账号和密码。要先输当前密码；新密码留空表示只改账号。
/// 登录设备。本机不给踢出按钮，要走就用退出登录；新设备的按钮是灰的，服务端也会拒绝。
fn sessions_section(conn:&Connection,csrf:&str,current:&str,manage:bool)->String {
    let disabled=if manage {""} else {" disabled"};
    let shown=|value:&str|if value.is_empty() {"未知".to_string()} else {esc(value)};
    let mut body=String::new();
    let mut others=0;
    if let Ok(mut stmt)=conn.prepare("SELECT rowid,hash,device,source,CASE WHEN created>0 THEN strftime('%m-%d %H:%M',created,'unixepoch','localtime') ELSE '' END FROM admin_sessions WHERE expires>? ORDER BY created DESC") {
        if let Ok(rows)=stmt.query_map([now()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?))) {
            for (id,hash,name,source,time) in rows.flatten() {
                let action=if hash==current {"<small>本机</small>".to_string()} else {
                    others+=1;
                    format!(r#"<form method="post" action="/admin/sessions"><input type="hidden" name="csrf" value="{csrf}"><button class="danger" name="target" value="{id}"{disabled}>踢出</button></form>"#)
                };
                body.push_str(&format!(r#"<div class="node row"><span class="grow">{} <small>· {} · {}</small></span>{action}</div>"#,shown(&name),shown(&source),shown(&time)));
            }
        };
    }
    if others>0 {
        let hint=if manage {""} else {"<small>登录满 24 小时后可用</small>"};
        body.push_str(&format!(r#"<form method="post" action="/admin/sessions"><input type="hidden" name="csrf" value="{csrf}"><div class="actions"><button class="danger" name="target" value="others"{disabled}>踢出其他全部</button>{hint}</div></form>"#));
    }
    body
}

/// 下载和上传恢复，和 Komari 一样：新机器用同一个域名、传上旧备份，被控机一台都不用动。
fn backup_section(csrf:&str)->String {
    format!(r#"<form method="post" action="/admin/backup"><input type="hidden" name="csrf" value="{csrf}"><div class="actions"><button>下载备份</button></div></form>
<form class="upload" method="post" action="/admin/backup/restore" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{csrf}">
<label>上传恢复<input type="file" name="backup" accept=".db" required></label>
<div class="actions"><label><input type="checkbox" name="confirm" value="1" required>覆盖现有全部数据</label><button class="danger">恢复</button></div></form>
<p class="muted">备份里有节点 token 和 Bot Token，按密码保管。恢复后用备份里的账号密码登录，后台地址也变回备份里的。</p>"#)
}

fn account_section(conn:&Connection,csrf:&str,manage:bool)->String {
    format!(r#"<form method="post" action="/admin/account"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>账号<input name="username" value="{}" maxlength="32" autocomplete="username" required></label>
<label>当前密码<input type="password" name="current" autocomplete="current-password" required></label>
<label>新密码<input type="password" name="password" minlength="{PASSWORD_MIN}" maxlength="128" autocomplete="new-password"></label>
<label>确认新密码<input type="password" name="confirm" minlength="{PASSWORD_MIN}" maxlength="128" autocomplete="new-password"></label></div>
<div class="actions"><button{}>保存</button><small>{}</small></div></form>"#,
        esc(&username(conn)),if manage {""} else {" disabled"},if manage {"保存后需重新登录"} else {"登录满 24 小时后可用"})
}

/// 站点名称和图标。图标单选切换；上传的图片和表情可以删除，删掉的若正在用就切回机箱。
fn site_section(conn:&Connection,csrf:&str)->String {
    let current=site::current_choice(conn);
    let emoji=site::emoji(conn);
    let mut choices=String::new();
    for (key,label) in site::CHOICES {
        let available=match key {"upload"=>site::has_upload(conn),"emoji"=>emoji.is_some(),_=>true};
        if !available {continue}
        let checked=if key==current {" checked"} else {""};
        let delete=if matches!(key,"upload"|"emoji") {format!(r#"<button class="link" name="delete" value="{key}">删除</button>"#)} else {String::new()};
        choices.push_str(&format!(r#"<label class="icon-choice"><img src="/admin/icon/{key}" alt="" width="40" height="40"><span><input type="radio" name="icon" value="{key}"{checked}>{label}</span>{delete}</label>"#));
    }
    format!(r#"<form method="post" action="/admin/site"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>站点名称<input name="site_name" value="{}" maxlength="30" required></label>
<label>表情图标<input name="emoji" maxlength="16" placeholder="📡"></label></div>
<div class="actions"><button>保存</button></div></form>
<div class="node"><form method="post" action="/admin/site"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="action" value="icon">
<div class="icons">{choices}</div><div class="actions"><button>使用</button></div></form>
<form class="upload" method="post" action="/admin/site/icon" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{csrf}">
<label>上传图片<input type="file" name="icon" accept="image/png,image/jpeg,image/gif,image/webp,image/x-icon" required></label>
<div class="actions"><button class="secondary">上传</button></div></form></div>"#,
        esc(&site::name()))
}

/// 访问控制：后台地址、展示页域名、公开页开关。
fn access_section(csrf:&str,here:&str)->String {
    let checked=if access::public_enabled() {"checked"} else {""};
    format!(r#"<form method="post" action="/admin/access"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>后台地址<input name="admin_path" value="{}" maxlength="48" required></label>
<label>展示页域名<input name="display_domain" value="{}" maxlength="253" placeholder="status.example.com"></label></div>
<div class="actions"><label><input type="checkbox" name="public" value="1" {checked}>开放公开状态页</label><button>保存</button></div></form>
<p class="muted">主域名 <code>{}</code>　忘记后台地址：<code>docker compose exec avalon probe-hub access</code></p>"#,
        esc(&access::admin_path()),esc(&access::display_domain()),esc(here))
}

/// 面板对外的地址，拼进安装命令里给人复制。取浏览器正在访问的那个域名；
/// 字符集收得很死，这串东西最后是要贴进 shell 执行的。
fn public_base(headers:&HeaderMap)->String {
    let host=request_hosts(headers).into_iter().next().unwrap_or_default();
    let safe=!host.is_empty()&&host.len()<=255&&host.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'.'||b==b'-'||b==b':');
    let host=if safe {host} else {"hub.example.com".to_string()};
    let scheme=if host.starts_with("127.0.0.1")||host.starts_with("localhost") {"http"} else {"https"};
    format!("{scheme}://{host}")
}
/// 脚本地址里的钥匙由 token 派生，扫描器猜不到；换了 token，旧命令随之失效。
fn install_command(base:&str,id:&str,token:&str)->String {
    format!("curl -fsSL {base}/i/{} | sh -s -- --server {base} --id {id} --token {token}",install_key(token))
}
/// 卸载不找 hub 要脚本：节点删掉以后钥匙就没了，而且卸载本来也用不着网络。
fn uninstall_command()->String {
    "systemctl disable --now probe-agent; rm -rf /etc/systemd/system/probe-agent.service /etc/systemd/system/probe-agent.service.d /usr/local/bin/probe-agent /etc/linux-probe; systemctl daemon-reload; userdel probe-agent".to_string()
}

/// 自动识别到的地址，只在后台显示。家宽在 NAT 后面时用的是 hub 看到的出口地址，会标出来。
fn ip_line(conn:&Connection,id:&str)->String {
    let row:Option<(String,String,String)>=conn.query_row("SELECT ipv4,ipv6,observed FROM node_net WHERE node_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().ok().flatten();
    let Some((ipv4,ipv6,observed))=row else {return "IP：还没连上过".to_string()};
    let (v4,v4_exit,v6,v6_exit)=shown_ips(&ipv4,&ipv6,&observed);
    let part=|ip:&str,exit:bool| if ip.is_empty() {"无".to_string()} else if exit {format!("{}（出口地址）",esc(ip))} else {esc(ip)};
    format!("IPv4：{}　IPv6：{}",part(&v4,v4_exit),part(&v6,v6_exit))
}

const GIB:f64=1024.0*1024.0*1024.0;

/// 表单里的 GB 数换成字节（按 1024 进制，和公开页显示一致：填 500 就显示 500G）。
/// 留空是 None；填了但不是 0–1000000 之间的数是 Err。
fn parse_gb(input:&str)->Result<Option<i64>,()> {
    let text=input.trim();
    if text.is_empty() {return Ok(None);}
    let value:f64=text.parse().map_err(|_|())?;
    if !value.is_finite()||!(0.0..=1_000_000.0).contains(&value) {return Err(());}
    Ok(Some((value*GIB).round() as i64))
}
fn gb(bytes:i64)->String {
    let text=format!("{:.2}",bytes.max(0) as f64/GIB);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// 计算方式下拉框。四种计算方式，键名是公开页主题认的那几个。
fn mode_picker(current:&str)->String {
    let current=if current.is_empty() {"sum"} else {current};
    let mut html=String::from("<select name=\"traffic_mode\">");
    for (key,label) in [("sum","上下行相加"),("max","取较大值"),("up","仅上行"),("down","仅下行")] {
        let selected=if key==current {" selected"} else {""};
        html.push_str(&format!("<option value=\"{key}\"{selected}>{label}</option>"));
    }
    html.push_str("</select>");
    html
}

/// 流量校正：只改填了的项。
/// - 总流量直接改累计值；基线不动，下一次上报接着往上加。
/// - 本期流量记成「目标值 − 本期实际累计」的偏移，挂在这一期上；进了下一期偏移自动作废，从零重新累计。
fn apply_fixes(conn:&Connection,id:&str,reset_day:i64,fixes:[Option<i64>;4])->rusqlite::Result<()> {
    let [total_rx,total_tx,month_rx,month_tx]=fixes;
    if total_rx.is_some()||total_tx.is_some() {
        let current:Option<(i64,i64)>=conn.query_row("SELECT total_rx,total_tx FROM traffic WHERE node_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (old_rx,old_tx)=current.unwrap_or((0,0));
        let (new_rx,new_tx)=(total_rx.unwrap_or(old_rx),total_tx.unwrap_or(old_tx));
        if current.is_some() {
            conn.execute("UPDATE traffic SET total_rx=?,total_tx=? WHERE node_id=?",params![new_rx,new_tx,id])?;
        } else {
            // 还没上报过：先记下累计值。开机 ID 填一个永远对不上的值，第一次上报只对基线，不会把整个计数器算进去。
            conn.execute("INSERT INTO traffic(node_id,last_rx,last_tx,total_rx,total_tx,boot) VALUES (?,0,0,?,?,'manual')",params![id,new_rx,new_tx])?;
        }
    }
    if month_rx.is_some()||month_tx.is_some() {
        let Some(today)=local_today(conn)? else {return Ok(())};
        let (start,raw_rx,raw_tx)=period_raw(conn,id,reset_day,today)?;
        let (now_rx,now_tx)=period_usage(conn,id,reset_day,today)?;
        let (target_rx,target_tx)=(month_rx.unwrap_or(now_rx),month_tx.unwrap_or(now_tx));
        conn.execute("INSERT OR REPLACE INTO traffic_adjust(node_id,period,rx,tx) VALUES (?,?,?,?)",params![id,start,target_rx-raw_rx,target_tx-raw_tx])?;
    }
    Ok(())
}

/// 计费周期下拉框。键名和公开页主题认的一致；以前手填过的非标准值也保留成一个选项，
/// 不然旧数据一保存就被清掉。自动续期只认标准周期。
fn cycle_picker(current:&str)->String {
    let mut html=String::from("<select name=\"billing_cycle\"><option value=\"\">未设置</option>");
    let known=expiry::CYCLES.iter().any(|(key,_)|*key==current);
    for (key,label) in expiry::CYCLES {
        let selected=if key==current {" selected"} else {""};
        html.push_str(&format!("<option value=\"{key}\"{selected}>{label}</option>"));
    }
    if !current.is_empty()&&!known {
        html.push_str(&format!("<option value=\"{0}\" selected>{0}（不会自动续期）</option>",esc(current)));
    }
    html.push_str("</select>");
    html
}

fn node_picker(conn:&Connection,chosen:&HashSet<String>)->String {
    let Ok(mut stmt)=conn.prepare("SELECT id,name FROM nodes ORDER BY sort,name") else {return String::new()};
    let Ok(rows)=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))) else {return String::new()};
    let mut html=String::from("<div class=\"picker\">");
    for (id,name) in rows.flatten() {
        let checked=if chosen.contains(&id) {"checked"} else {""};
        html.push_str(&format!("<label><input type=\"checkbox\" name=\"nodes\" value=\"{}\" {checked}>{}</label>",esc(&id),esc(&name)));
    }
    html.push_str("</div>");
    html
}

fn monitor_form(conn:&Connection,csrf:&str,id:Option<i64>,name:&str,target:&str,interval:i64,auto:bool,chosen:&HashSet<String>)->String {
    let action=id.map(|id|format!("/admin/monitors/{id}")).unwrap_or_else(||"/admin/monitors".into());
    let submit=if id.is_some() {"保存监控"} else {"添加监控"};
    let remove=id.map(|_|"<button class=\"danger\" name=\"delete\" value=\"1\">删除监控</button>").unwrap_or("");
    format!(r#"<form method="post" action="{action}"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>名称<input name="name" value="{}" maxlength="40" required></label>
<label>目标地址 host:port<input name="target" value="{}" maxlength="270" placeholder="1.1.1.1:443" required></label>
<label>间隔（秒）<input name="interval" type="number" min="{}" max="{}" value="{interval}"></label></div>
<p class="muted" style="margin:14px 0 0">运行节点</p>{}
<div class="actions"><label><input type="checkbox" name="auto_join" value="1" {}>新节点自动加入</label><button>{submit}</button>{remove}</div></form>"#,
        esc(name),esc(target),ping::MIN_INTERVAL,ping::MAX_INTERVAL,
        node_picker(conn,chosen),if auto {"checked"} else {""})
}

fn monitors_section(conn:&Connection,csrf:&str,open:&str)->String {
    let mut body=String::new();
    let Ok(mut stmt)=conn.prepare("SELECT id,name,host,port,interval,auto_join FROM monitors ORDER BY sort,id") else {return body};
    let Ok(rows)=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?))) else {return body};
    let monitors:Vec<_>=rows.flatten().collect();
    if monitors.is_empty() {body.push_str("<p class=\"muted\">还没有监控。</p>");}
    let count=monitors.len();
    for (index,(id,name,host,port,interval,auto)) in monitors.into_iter().enumerate() {
        let mut chosen:HashSet<String>=HashSet::new();
        if let Ok(mut stmt)=conn.prepare("SELECT node_id FROM monitor_nodes WHERE monitor_id=?") {
            if let Ok(rows)=stmt.query_map([id],|r|r.get::<_,String>(0)) {chosen.extend(rows.flatten());};
        }
        let target=if host.contains(':') {format!("[{host}]:{port}")} else {format!("{host}:{port}")};
        let anchor=format!("m-{id}");
        body.push_str(&format!("<div class=\"node row\"><details id=\"{anchor}\"{}><summary>{} <small>· {} · {interval}s · {} 个节点</small></summary>{}</details>{}</div>",
            if open==anchor {" open"} else {""},
            esc(&name),esc(&target),chosen.len(),monitor_form(conn,csrf,Some(id),&name,&target,interval,auto==1,&chosen),
            mover(&format!("/admin/monitors/{id}/move"),csrf,index,count)));
    }
    body
}

#[derive(Deserialize)] pub(super) struct Panel {open:Option<String>}
pub(super) async fn page(State(state):State<App>,headers:HeaderMap,Query(query):Query<Panel>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let Some((current,csrf))=session(&headers,&conn) else {
        let configured:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM admin_settings WHERE key='password')",[],|r|r.get(0)).unwrap_or(false);
        let tip=if configured {"请输入 admin-setup 给出的账号和密码。"} else {"先在 VPS 执行：runuser -u probe -- /usr/local/bin/probe-hub admin-setup --db /var/lib/linux-probe/probe.db"};
        return frame(&format!("<section class='card'><h1>管理员登录</h1><p>{}</p><form method='post' action='/admin/login'><label>账号<input name='username' maxlength='32' autocomplete='username' required></label><label>密码<input type='password' name='password' autocomplete='current-password' required></label><div class='actions'><button>登录</button></div></form></section>",esc(tip))).into_response();
    };
    let open=opened(query.open.as_deref());
    let manage=can_manage(&conn,&current);
    let bot:bool=conn.query_row("SELECT value!='' FROM admin_settings WHERE key='bot_token'",[],|r|r.get(0)).unwrap_or(false);
    let chat:String=conn.query_row("SELECT value FROM admin_settings WHERE key='chat_id'",[],|r|r.get(0)).unwrap_or_default();
    let base=public_base(&headers);
    let mut body=String::new();
    let mut stmt=match conn.prepare("SELECT n.id,n.name,n.public,n.last_seen,n.token,c.country,c.display_ip,c.notify,c.remark,c.price,c.currency,c.billing_cycle,c.expires_at,c.traffic_limit,c.traffic_mode,c.traffic_reset_day,(SELECT COUNT(*) FROM monitor_nodes m WHERE m.node_id=n.id),COALESCE(c.auto_renew,1),COALESCE(t.total_rx,0),COALESCE(t.total_tx,0) FROM nodes n LEFT JOIN node_config c ON n.id=c.node_id LEFT JOIN traffic t ON t.node_id=n.id ORDER BY n.sort,n.name") {Ok(s)=>s,Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?.unwrap_or_default(),r.get::<_,Option<String>>(6)?.unwrap_or_default(),r.get::<_,Option<i64>>(7)?.unwrap_or(1),r.get::<_,Option<String>>(8)?.unwrap_or_default(),r.get::<_,Option<f64>>(9)?.unwrap_or(0.0),r.get::<_,Option<String>>(10)?.unwrap_or_default(),r.get::<_,Option<String>>(11)?.unwrap_or_default(),r.get::<_,Option<String>>(12)?.unwrap_or_default(),r.get::<_,Option<i64>>(13)?.unwrap_or(0),r.get::<_,Option<String>>(14)?.unwrap_or_default(),r.get::<_,Option<i64>>(15)?.unwrap_or(1),r.get::<_,i64>(16)?,r.get::<_,i64>(17)?,r.get::<_,i64>(18)?,r.get::<_,i64>(19)?)));
    let today=local_today(&conn).ok().flatten();
    let rows:Vec<_>=match rows {Ok(rows)=>rows.flatten().collect(),Err(_)=>Vec::new()};
    let count=rows.len();
    for (index,row) in rows.into_iter().enumerate() {
        let (id,name,public,seen,token,country,ip,notify,remark,price,currency,cycle,expires,limit,mode,reset,monitors,auto_renew,total_rx,total_tx)=row;
        let (month_rx,month_tx)=today.and_then(|today|period_usage(&conn,&id,reset,today).ok()).unwrap_or((0,0));
        let limit_text=if limit>0 {gb(limit)} else {String::new()};
        let cycle_select=cycle_picker(&cycle);
        let is_online=seen.is_some_and(|v|now()-v<30);
        let status=if is_online {"<span class='ok'>在线</span>"} else {"<span class='down'>离线</span>"};
        let anchor=format!("n-{id}");
        let unfold=if open==anchor {" open"} else {""};
        let arrows=mover(&format!("/admin/nodes/{}/move",esc(&id)),&csrf,index,count);
        body.push_str(&format!(r#"<div class="node row"><details id="{anchor}"{unfold}><summary>{status}　{} <small>· {monitors} 个监控 · {}</small></summary><p class="muted">{}</p><p class="muted">安装命令</p><code class="cmd">{}</code><p class="muted">卸载：<code>{}</code></p><form method="post" action="/admin/nodes/{}"><input type="hidden" name="csrf" value="{}"><div class="grid">
<label>名称<input name="name" value="{}" maxlength="80" required></label><label>国家/地区代码<input name="country" value="{}" maxlength="2" placeholder="HK"></label><label>手填 IP<input name="display_ip" value="{}" maxlength="100"></label><label>备注<input name="remark" value="{}" maxlength="300"></label><label>价格<input name="price" type="number" min="0" step="0.01" value="{}"></label><label>货币<input name="currency" value="{}" maxlength="8" placeholder="$"></label><label>计费周期{}</label><label>到期日期<input name="expires_at" type="date" value="{}"></label><label>每月额度（GB）<input name="traffic_limit" type="number" min="0" step="0.01" value="{}" placeholder="不限"></label><label>计算方式{}</label><label>流量重置日<input name="traffic_reset_day" type="number" min="1" max="31" value="{}"></label></div>
<details><summary class="muted">流量校正</summary><div class="grid">
<label>总下行（GB）<input name="fix_total_rx" type="number" min="0" step="0.01" placeholder="现在 {}"></label><label>总上行（GB）<input name="fix_total_tx" type="number" min="0" step="0.01" placeholder="现在 {}"></label>
<label>本期下行（GB）<input name="fix_month_rx" type="number" min="0" step="0.01" placeholder="现在 {}"></label><label>本期上行（GB）<input name="fix_month_tx" type="number" min="0" step="0.01" placeholder="现在 {}"></label></div></details><div class="actions"><label><input type="checkbox" name="public" value="1" {}>公开显示</label><label><input type="checkbox" name="notify" value="1" {}>掉线/到期通知</label><label><input type="checkbox" name="auto_renew" value="1" {}>到期后仍在线自动续期</label><button>保存节点</button><button class="secondary" name="action" value="rotate" formnovalidate>重新生成 Token</button><label><input type="checkbox" name="confirm" value="1">确认</label><button class="danger" name="action" value="delete" formnovalidate>删除节点</button></div></form></details>{arrows}</div>"#,
        esc(&name),if public==1 {"公开"} else {"私有"},ip_line(&conn,&id),esc(&install_command(&base,&id,&token)),esc(&uninstall_command()),esc(&id),csrf,esc(&name),esc(&country),esc(&ip),esc(&remark),price,esc(&currency),cycle_select,esc(&expires),limit_text,mode_picker(&mode),reset,gb(total_rx),gb(total_tx),gb(month_rx),gb(month_tx),if public==1 {"checked"} else {""},if notify==1 {"checked"} else {""},if auto_renew==1 {"checked"} else {""}));
    }
    if count==0 {body.push_str("<p class=\"muted\">还没有节点。</p>");}
    let add_node=format!(r#"<form method="post" action="/admin/nodes"><input type="hidden" name="csrf" value="{csrf}"><label>节点名称<input name="name" maxlength="80" required></label><div class="actions"><button>创建并显示 Agent 凭据</button></div></form>"#);
    let telegram=format!(r#"<form method="post" action="/admin/settings"><input type="hidden" name="csrf" value="{csrf}"><div class="grid"><label>Bot Token<input type="password" name="bot_token" autocomplete="off" placeholder="{}"></label><label>Chat ID<input name="chat_id" value="{}" maxlength="80"></label></div><div class="actions"><button>保存通知设置</button><label><input type="checkbox" name="clear_token" value="1">清除 Token</label></div></form><form method="post" action="/admin/test"><input type="hidden" name="csrf" value="{csrf}"><div class="actions"><button class="secondary">发送测试通知</button></div></form>"#,
        if bot {"已保存，留空则不修改"} else {"123456:ABC..."},esc(&chat));
    let here=request_hosts(&headers).into_iter().next().unwrap_or_default();
    // 常看的排前面，默认也只展开「节点」；设一次就不动的几张收在下面。
    let page=format!("<h1>节点管理</h1>{}{}{}{}{}{}{}{}{}{}<form method=\"post\" action=\"/admin/logout\"><input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><button class=\"secondary\">退出登录</button></form>",
        card("add-node","添加节点",open=="add-node",&add_node),
        card("nodes","节点",open.is_empty()||open=="nodes"||open.starts_with("n-"),&body),
        card("add-monitor","添加监控",open=="add-monitor",&monitor_form(&conn,&csrf,None,"","",60,false,&HashSet::new())),
        card("monitors","延迟监控",open=="monitors"||open.starts_with("m-"),&monitors_section(&conn,&csrf,&open)),
        card("site","站点",open=="site",&site_section(&conn,&csrf)),
        card("access","访问控制",open=="access",&access_section(&csrf,&here)),
        card("telegram","Telegram 通知",open=="telegram",&telegram),
        card("account","账号",open=="account",&account_section(&conn,&csrf,manage)),
        card("sessions","登录设备",open=="sessions",&sessions_section(&conn,&csrf,&current,manage)),
        card("backup","备份",open=="backup",&backup_section(&csrf)));
    frame(&page).into_response()
}

#[derive(Deserialize)] pub(super) struct Login { username:String, password:String }
pub(super) async fn login(State(state):State<App>,headers:HeaderMap,Form(form):Form<Login>)->Response {
    if !same_origin(&headers) {return failure(StatusCode::FORBIDDEN,"请求来源不正确");}
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let source=client(&headers);
    if locked(&conn,&source) {return failure(StatusCode::TOO_MANY_REQUESTS,"该地址登录失败次数过多，请十五分钟后再试");}
    // argon2 是故意算得慢的，同时只放一个进去；排队而不是直接拒绝，
    // 否则两个人同时点一下登录就有一个吃 429。
    let Ok(Ok(permit))=tokio::time::timeout(Duration::from_secs(3),LOGIN_GATE.acquire()).await else {
        return failure(StatusCode::TOO_MANY_REQUESTS,"登录请求太多，请稍后重试");
    };
    let verified=credentials_ok(&conn,&form.username,&form.password);
    drop(permit);
    if !verified {
        record_failure(&conn,&source);
        return failure(StatusCode::UNAUTHORIZED,"账号或密码错误");
    }
    clear_failures(&conn,&source);
    let Ok(raw)=random_bytes(32) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let token=hex(&raw);
    let ua=headers.get(axum::http::header::USER_AGENT).and_then(|v|v.to_str().ok()).unwrap_or("");
    let label=device(ua);
    if conn.execute("INSERT INTO admin_sessions(hash,expires,created,source,device) VALUES (?,?,?,?,?)",params![digest(&token),now()+SESSION_LIFETIME,now(),source,label]).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    // 每次登录都发一条。不带后台地址：那是秘密，不该经过第三方。发不出去也不影响登录。
    let path=state.db_path.to_string();
    let text=format!("后台有新的登录\n设备：{label}\nIP：{source}");
    tokio::spawn(async move {let _=send_telegram(&path,&text).await;});
    let mut response=Redirect::to(&access::admin_home()).into_response();
    let cookie=format!("{COOKIE_NAME}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={SESSION_LIFETIME}");
    if let Ok(v)=cookie.parse(){response.headers_mut().insert(SET_COOKIE,v);} response
}
#[derive(Deserialize)] pub(super) struct Csrf {csrf:String}
pub(super) async fn logout(State(state):State<App>,headers:HeaderMap,Form(form):Form<Csrf>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf) {return StatusCode::FORBIDDEN.into_response();}
    if let Some((hash,_))=session(&headers,&conn) {let _=conn.execute("DELETE FROM admin_sessions WHERE hash=?",[hash]);}
    let mut response=Redirect::to(&access::admin_home()).into_response();
    // 新旧两种作用范围的 cookie 都清掉。
    for scope in ["/","/admin"] {
        if let Ok(v)=format!("{COOKIE_NAME}=; HttpOnly; Secure; SameSite=Strict; Path={scope}; Max-Age=0").parse() {response.headers_mut().append(SET_COOKIE,v);}
    }
    response
}
#[derive(Deserialize)]pub(super) struct Account {csrf:String,current:String,username:String,password:String,confirm:String}
pub(super) async fn save_account(State(state):State<App>,headers:HeaderMap,Form(form):Form<Account>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let Some((current,_))=session(&headers,&conn) else {return StatusCode::FORBIDDEN.into_response()};
    if !can_manage(&conn,&current) {return failure(StatusCode::FORBIDDEN,"这台设备登录还不满 24 小时，不能改账号密码");}
    let Some(name)=valid_username(&form.username) else {
        return failure(StatusCode::BAD_REQUEST,"账号需要 1–32 位字母、数字或 . _ - @");
    };
    let change=!form.password.is_empty();
    if change {
        if form.password.chars().count()<PASSWORD_MIN {return failure(StatusCode::BAD_REQUEST,&format!("新密码至少 {PASSWORD_MIN} 位"));}
        if form.password.len()>128 {return failure(StatusCode::BAD_REQUEST,"新密码太长");}
        if form.password!=form.confirm {return failure(StatusCode::BAD_REQUEST,"两次输入的新密码不一样");}
    }
    // 这个表单收当前密码，等于第二个登录入口，所以限流和那把信号量一样都要走。
    let source=client(&headers);
    if locked(&conn,&source) {return failure(StatusCode::TOO_MANY_REQUESTS,"该地址失败次数过多，请十五分钟后再试");}
    let Ok(Ok(permit))=tokio::time::timeout(Duration::from_secs(3),LOGIN_GATE.acquire()).await else {
        return failure(StatusCode::TOO_MANY_REQUESTS,"请求太多，请稍后重试");
    };
    let current_ok=password_ok(&conn,&form.current);
    drop(permit);
    if !current_ok {
        record_failure(&conn,&source);
        return failure(StatusCode::UNAUTHORIZED,"当前密码不对");
    }
    clear_failures(&conn,&source);
    drop(conn);
    if store_account(&state.db_path,&name,change.then_some(form.password.as_str())).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    frame(&format!("<section class='card'><h1>已保存</h1><p>{}所有登录会话已注销。</p><p><a href='/admin/'>重新登录</a></p></section>",
        if change {"账号和密码都改了，"} else {"账号改了，"})).into_response()
}

#[derive(Deserialize)]pub(super) struct Move {csrf:String,dir:String}
fn direction(dir:&str)->Option<bool> {
    match dir {"up"=>Some(true),"down"=>Some(false),_=>None}
}
pub(super) async fn move_node(Path(id):Path<String>,State(state):State<App>,headers:HeaderMap,Form(form):Form<Move>)->Response {
    // id 会拼进跳转地址的锚点里，先把形状卡死。
    if id.len()!=16||!id.bytes().all(|b|b.is_ascii_hexdigit()) {return StatusCode::NOT_FOUND.into_response();}
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let Some(up)=direction(&form.dir) else {return StatusCode::BAD_REQUEST.into_response()};
    if move_row(&mut conn,"nodes","sort,name",&id,up).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back_to("nodes",&format!("n-{id}"))
}
/// 只影响后台和公开页的展示顺序，agent 的探测任务照旧按 id 下发，不用通知 agent。
pub(super) async fn move_monitor(Path(id):Path<i64>,State(state):State<App>,headers:HeaderMap,Form(form):Form<Move>)->Response {
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let Some(up)=direction(&form.dir) else {return StatusCode::BAD_REQUEST.into_response()};
    if move_row(&mut conn,"monitors","sort,id",&id.to_string(),up).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back_to("monitors",&format!("m-{id}"))
}

/// 发一条 Telegram，不等结果。
fn notify(path:&str,text:String) {
    let path=path.to_string();
    tokio::spawn(async move {let _=send_telegram(&path,&text).await;});
}

pub(super) async fn download_backup(State(state):State<App>,headers:HeaderMap,Form(form):Form<Csrf>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let (Ok(tag),Ok(stamp))=(new_secret(8),conn.query_row("SELECT strftime('%Y%m%d-%H%M','now','localtime')",[],|r|r.get::<_,String>(0))) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // WAL 模式下直接读文件可能拿到不一致的快照；VACUUM INTO 出来的是一整份可用的库。
    let temp=format!("{}.export-{tag}",state.db_path);
    let made=conn.execute("VACUUM INTO ?",[&temp]);
    drop(conn);
    let data=made.ok().and_then(|_|std::fs::read(&temp).ok());
    let _=std::fs::remove_file(&temp);
    let Some(data)=data else {return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    notify(&state.db_path,"后台下载了一份备份".to_string());
    let disposition=format!("attachment; filename=\"avalon-backup-{stamp}.db\"");
    ([(axum::http::header::CONTENT_TYPE,"application/octet-stream".to_string()),(axum::http::header::CONTENT_DISPOSITION,disposition)],data).into_response()
}

fn part<'a>(parts:&'a [(String,Vec<u8>)],name:&str)->Option<&'a [u8]> {
    parts.iter().find(|(key,_)|key==name).map(|(_,value)|value.as_slice())
}
/// 上传的得是完好的、我们自己的库才换上去。换之前清空里面的登录会话：
/// 恢复以后所有设备都用备份里的账号密码重新登录。
fn check_backup(path:&str)->Result<(),&'static str> {
    let conn=Connection::open(path).map_err(|_|"打不开这个文件")?;
    let health:String=conn.query_row("PRAGMA quick_check",[],|r|r.get(0)).map_err(|_|"文件损坏")?;
    if health!="ok" {return Err("文件损坏");}
    let ours:i64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('nodes','admin_settings','admin_sessions')",[],|r|r.get(0)).map_err(|_|"文件损坏")?;
    // 我们的库里没有触发器和视图，有就不是我们导出的，不收。
    let extra:i64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type IN ('trigger','view')",[],|r|r.get(0)).map_err(|_|"文件损坏")?;
    if ours!=3||extra>0 {return Err("这不是 Avalon 的备份文件");}
    conn.execute("DELETE FROM admin_sessions",[]).map_err(|_|"文件损坏")?;
    Ok(())
}

pub(super) async fn restore_backup(State(state):State<App>,headers:HeaderMap,body:axum::body::Bytes)->Response {
    let content_type=headers.get(axum::http::header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).unwrap_or("");
    let Some(parts)=site::multipart(content_type,&body) else {return failure(StatusCode::BAD_REQUEST,"上传的数据格式不对，请重试");};
    let csrf=part(&parts,"csrf").map(|v|String::from_utf8_lossy(v).into_owned()).unwrap_or_default();
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&csrf){return StatusCode::FORBIDDEN.into_response();}
    drop(conn);
    if part(&parts,"confirm").is_none() {return failure(StatusCode::BAD_REQUEST,"请先勾选「覆盖现有全部数据」");}
    let Some(data)=part(&parts,"backup").filter(|data|!data.is_empty()) else {return failure(StatusCode::BAD_REQUEST,"请先选择备份文件");};
    if !data.starts_with(b"SQLite format 3\0") {return failure(StatusCode::BAD_REQUEST,"这不是 Avalon 的备份文件");}
    let pending=format!("{}.restore",state.db_path);
    if std::fs::write(&pending,data).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    if let Err(message)=check_backup(&pending) {
        let _=std::fs::remove_file(&pending);
        return failure(StatusCode::BAD_REQUEST,message);
    }
    // 回完这一页再退出。Docker 的 restart 策略（或 systemd 的 Restart=）会把程序拉起来，启动时换上新库。
    // 用非 0 退出码：systemd 只配了 Restart=on-failure 的也会重启。
    let path=state.db_path.to_string();
    tokio::spawn(async move {
        let _=send_telegram(&path,"后台上传了备份，hub 正在重启换上它").await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        std::process::exit(1);
    });
    frame("<section class='card'><h1>正在恢复</h1><p>hub 正在重启换上备份，大约十秒后刷新。账号密码用备份里的，后台地址也变回备份里的那个。</p></section>").into_response()
}

#[derive(Deserialize)]pub(super) struct Kick {csrf:String,target:String}
pub(super) async fn kick(State(state):State<App>,headers:HeaderMap,Form(form):Form<Kick>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let Some((current,_))=session(&headers,&conn) else {return StatusCode::FORBIDDEN.into_response()};
    if !can_manage(&conn,&current) {return failure(StatusCode::FORBIDDEN,"这台设备登录还不满 24 小时，不能踢出别的设备");}
    // 本机永远踢不掉自己，要走用退出登录。
    let result=if form.target=="others" {
        conn.execute("DELETE FROM admin_sessions WHERE hash<>?",[&current])
    } else {
        let Ok(id)=form.target.parse::<i64>() else {return StatusCode::BAD_REQUEST.into_response()};
        conn.execute("DELETE FROM admin_sessions WHERE rowid=? AND hash<>?",params![id,current])
    };
    if result.is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back("sessions")
}

#[derive(Deserialize)]pub(super) struct NewNode {csrf:String,name:String}
pub(super) async fn add_node(State(state):State<App>,headers:HeaderMap,Form(form):Form<NewNode>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf) {return StatusCode::FORBIDDEN.into_response();}
    let name=form.name.trim();if name.is_empty()||name.len()>80 {return failure(StatusCode::BAD_REQUEST,"节点名称长度需在 1–80 字节之间");}
    let (Ok(id),Ok(token))=(new_secret(8),new_secret(32)) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if conn.execute("INSERT INTO nodes(id,name,token,public,sort) VALUES (?,?,?,1,(SELECT COALESCE(MAX(sort),0)+1 FROM nodes))",params![id,name,token]).is_err(){return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    let _=ping::attach_auto(&conn,&id);
    CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
    let command=install_command(&public_base(&headers),&id,&token);
    frame(&format!("<section class='card'><h1>节点已创建</h1><p>在要监控的机器上用 root 执行：</p><code class='cmd'>{}</code><details><summary class='muted'>节点 ID 和 Token</summary><p>节点 ID：<code>{}</code></p><p>Token：<code>{}</code></p></details><p><a href='/admin/'>返回管理</a></p></section>",esc(&command),esc(&id),esc(&token))).into_response()
}
#[derive(Deserialize)]pub(super) struct EditNode {
    csrf:String,name:String,country:String,display_ip:String,remark:String,
    price:f64,currency:String,billing_cycle:String,expires_at:String,traffic_limit:String,traffic_mode:String,traffic_reset_day:i64,
    fix_total_rx:Option<String>,fix_total_tx:Option<String>,fix_month_rx:Option<String>,fix_month_tx:Option<String>,
    public:Option<String>,notify:Option<String>,auto_renew:Option<String>,action:Option<String>,confirm:Option<String>,
}
pub(super) async fn edit_node(Path(id):Path<String>,State(state):State<App>,headers:HeaderMap,Form(form):Form<EditNode>)->Response {
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    match form.action.as_deref() {
        Some("rotate")=>return rotate_token(&conn,&id,&public_base(&headers)),
        Some("delete")=>return delete_node(&mut conn,&id,form.confirm.is_some()),
        _=>(),
    }
    let name=form.name.trim();
    let Ok(limit)=parse_gb(&form.traffic_limit) else {return failure(StatusCode::BAD_REQUEST,"每月额度要填 GB 数，例如 500；不限就留空");};
    let fixes=[&form.fix_total_rx,&form.fix_total_tx,&form.fix_month_rx,&form.fix_month_tx].map(|value|parse_gb(value.as_deref().unwrap_or("")));
    if fixes.iter().any(|fix|fix.is_err()) {return failure(StatusCode::BAD_REQUEST,"流量校正要填 GB 数，不改的项留空");}
    let fixes=fixes.map(|fix|fix.unwrap_or(None));
    let mode=form.traffic_mode.trim();
    if !matches!(mode,""|"sum"|"max"|"up"|"down") {return failure(StatusCode::BAD_REQUEST,"计算方式不对");}
    if name.is_empty()||name.len()>80||form.country.len()>2||!form.country.trim().bytes().all(|b|b.is_ascii_alphabetic())||form.display_ip.len()>100||form.remark.len()>300||form.currency.trim().chars().count()>8||form.currency.chars().any(char::is_control)||form.billing_cycle.len()>20||form.expires_at.len()>10||form.traffic_mode.len()>20||!(1..=31).contains(&form.traffic_reset_day)||!form.price.is_finite()||form.price<0.0||form.price>1_000_000.0||(!form.expires_at.trim().is_empty()&&expiry::Date::parse(&form.expires_at).is_none()) {
        return failure(StatusCode::BAD_REQUEST,"节点设置无效");
    }
    let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    match tx.execute("UPDATE nodes SET name=?,public=? WHERE id=?",params![name,form.public.is_some() as i32,id]) {Ok(1)=>(),Ok(_)=>return StatusCode::NOT_FOUND.into_response(),Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let result=tx.execute("INSERT INTO node_config(node_id,country,display_ip,notify,remark,price,currency,billing_cycle,expires_at,traffic_limit,traffic_mode,traffic_reset_day,auto_renew) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET country=excluded.country,display_ip=excluded.display_ip,notify=excluded.notify,remark=excluded.remark,price=excluded.price,currency=excluded.currency,billing_cycle=excluded.billing_cycle,expires_at=excluded.expires_at,traffic_limit=excluded.traffic_limit,traffic_mode=excluded.traffic_mode,traffic_reset_day=excluded.traffic_reset_day,auto_renew=excluded.auto_renew",params![id,form.country.trim().to_uppercase(),form.display_ip.trim(),form.notify.is_some() as i32,form.remark.trim(),form.price,form.currency.trim(),form.billing_cycle.trim(),form.expires_at.trim(),limit.unwrap_or(0),mode,form.traffic_reset_day,form.auto_renew.is_some() as i32]);
    if result.is_err()||apply_fixes(&tx,&id,form.traffic_reset_day,fixes).is_err()||tx.commit().is_err(){return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back(&format!("n-{id}"))
}

/// 换 token。旧凭据立刻失效，包括正连着的那条：hub 在下一轮循环里发现 token
/// 变了就把连接踢掉，agent 拿旧 token 重连会吃到 401。
fn rotate_token(conn:&Connection,id:&str,base:&str)->Response {
    let Ok(token)=new_secret(32) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    match conn.execute("UPDATE nodes SET token=? WHERE id=?",params![token,id]) {
        Ok(1)=>(),Ok(_)=>return StatusCode::NOT_FOUND.into_response(),Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
    frame(&format!("<section class='card'><h1>Token 已更换</h1><p>旧 token 已失效，那台机器上的 agent 已被断开。在它上面重新执行一次安装命令就换上了新 token：</p><code class='cmd'>{}</code><p><a href='/admin/'>返回管理</a></p></section>",esc(&install_command(base,id,&token)))).into_response()
}

/// 删除节点，连同它的全部历史。不可恢复，所以要先勾确认。
fn delete_node(conn:&mut Connection,id:&str,confirmed:bool)->Response {
    if !confirmed {return failure(StatusCode::BAD_REQUEST,"删除节点会一并删掉它的全部历史数据，请先勾选「确认」再点删除");}
    let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let tables=["samples","latest","details","traffic","traffic_day","traffic_adjust","node_net","ping","monitor_nodes","node_config"];
    for table in tables {
        if tx.execute(&format!("DELETE FROM {table} WHERE node_id=?"),[id]).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let removed=tx.execute("DELETE FROM alert_state WHERE key IN (?,?)",[format!("node:{id}"),format!("expire:{id}")])
        .and_then(|_|tx.execute("DELETE FROM nodes WHERE id=?",[id]));
    match removed {Ok(1)=>(),Ok(_)=>return StatusCode::NOT_FOUND.into_response(),Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response()}
    if tx.commit().is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
    // 只删节点的话，那台机器上的 agent 会带着作废的 token 一直重连，所以顺手给出卸载命令。
    frame(&format!("<section class='card'><h1>节点已删除</h1><p>那台机器上的 agent 还在运行，在它上面用 root 执行这一条卸载：</p><code class='cmd'>{}</code><p><a href='/admin/'>返回管理</a></p></section>",esc(&uninstall_command()))).into_response()
}

/// 把一个监控的运行节点写成给定的集合。节点必须真实存在，且单节点不超过上限——
/// 上限在同一个事务里校验，超了整笔回滚，不会出现改了一半的状态。
fn assign(tx:&Connection,monitor:i64,nodes:&[&str])->Result<bool,rusqlite::Error> {
    tx.execute("DELETE FROM monitor_nodes WHERE monitor_id=?",[monitor])?;
    for node in nodes.iter().take(1000) {
        if node.len()!=16||!node.bytes().all(|b|b.is_ascii_hexdigit()) {continue;}
        tx.execute("INSERT OR IGNORE INTO monitor_nodes(monitor_id,node_id) SELECT ?,id FROM nodes WHERE id=?",params![monitor,node])?;
    }
    let worst:i64=tx.query_row("SELECT COALESCE(MAX(c),0) FROM (SELECT COUNT(*) c FROM monitor_nodes GROUP BY node_id)",[],|r|r.get(0))?;
    Ok(worst<=ping::MAX_PER_NODE)
}

pub(super) async fn add_monitor(State(state):State<App>,headers:HeaderMap,body:String)->Response {
    let fields=fields(&body);
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,one(&fields,"csrf")){return StatusCode::FORBIDDEN.into_response();}
    let name=one(&fields,"name").trim().to_string();
    let Some((host,port))=parse_target(one(&fields,"target")) else {return failure(StatusCode::BAD_REQUEST,"目标地址要写成 host:port，IPv6 写成 [地址]:端口");};
    let interval:i64=one(&fields,"interval").parse().unwrap_or(60);
    if name.is_empty()||name.chars().count()>40||!(ping::MIN_INTERVAL..=ping::MAX_INTERVAL).contains(&interval) {
        return failure(StatusCode::BAD_REQUEST,"名称需要 1–40 个字，间隔需要在 5–3600 秒之间");
    }
    let total:i64=conn.query_row("SELECT COUNT(*) FROM monitors",[],|r|r.get(0)).unwrap_or(0);
    if total>=ping::MAX_MONITORS {return failure(StatusCode::BAD_REQUEST,"监控数量已达上限");}
    let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if tx.execute("INSERT INTO monitors(name,host,port,interval,auto_join,enabled,sort) VALUES (?,?,?,?,?,1,(SELECT COALESCE(MAX(sort),0)+1 FROM monitors))",
        params![name,host,port,interval,!one(&fields,"auto_join").is_empty() as i32]).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let id=tx.last_insert_rowid();
    match assign(&tx,id,&many(&fields,"nodes")) {
        Ok(true)=>(),
        Ok(false)=>return failure(StatusCode::BAD_REQUEST,&format!("有节点的监控数会超过 {} 个上限",ping::MAX_PER_NODE)),
        Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    if tx.commit().is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
    back(&format!("m-{id}"))
}

pub(super) async fn edit_monitor(Path(id):Path<i64>,State(state):State<App>,headers:HeaderMap,body:String)->Response {
    let fields=fields(&body);
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,one(&fields,"csrf")){return StatusCode::FORBIDDEN.into_response();}
    if !one(&fields,"delete").is_empty() {
        let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
        let removed=tx.execute("DELETE FROM monitors WHERE id=?",[id])
            .and_then(|_|tx.execute("DELETE FROM monitor_nodes WHERE monitor_id=?",[id]))
            .and_then(|_|tx.execute("DELETE FROM ping WHERE monitor_id=?",[id]));
        if removed.is_err()||tx.commit().is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
        CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
        return back("monitors");
    }
    let name=one(&fields,"name").trim().to_string();
    let Some((host,port))=parse_target(one(&fields,"target")) else {return failure(StatusCode::BAD_REQUEST,"目标地址要写成 host:port，IPv6 写成 [地址]:端口");};
    let interval:i64=one(&fields,"interval").parse().unwrap_or(60);
    if name.is_empty()||name.chars().count()>40||!(ping::MIN_INTERVAL..=ping::MAX_INTERVAL).contains(&interval) {
        return failure(StatusCode::BAD_REQUEST,"名称需要 1–40 个字，间隔需要在 5–3600 秒之间");
    }
    let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    match tx.execute("UPDATE monitors SET name=?,host=?,port=?,interval=?,auto_join=? WHERE id=?",
        params![name,host,port,interval,!one(&fields,"auto_join").is_empty() as i32,id]) {
        Ok(1)=>(),Ok(_)=>return StatusCode::NOT_FOUND.into_response(),Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    match assign(&tx,id,&many(&fields,"nodes")) {
        Ok(true)=>(),
        Ok(false)=>return failure(StatusCode::BAD_REQUEST,&format!("有节点的监控数会超过 {} 个上限",ping::MAX_PER_NODE)),
        Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    if tx.commit().is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    CONFIG_VERSION.fetch_add(1,Ordering::Relaxed);
    back(&format!("m-{id}"))
}

pub(super) async fn save_access(State(state):State<App>,headers:HeaderMap,body:String)->Response {
    let fields=fields(&body);
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,one(&fields,"csrf")){return StatusCode::FORBIDDEN.into_response();}
    let Some(path)=access::valid_admin_path(one(&fields,"admin_path")) else {
        return failure(StatusCode::BAD_REQUEST,"后台地址需要 4–48 位小写字母、数字、- 或 _，而且不能用 api、assets 这类保留名");
    };
    let Some(display)=access::valid_domain(one(&fields,"display_domain")) else {
        return failure(StatusCode::BAD_REQUEST,"展示页域名只填域名本身，例如 status.example.com");
    };
    // 防止把自己锁在外面：展示页域名上没有后台，要是填了正在用的这个域名，保存完后台就打不开了。
    let here=request_hosts(&headers).into_iter().next().unwrap_or_default();
    if !display.is_empty()&&display==here {
        return failure(StatusCode::BAD_REQUEST,"展示页域名不能是你现在正在用的这个域名，否则保存后后台会立刻打不开。展示页请另用一个域名");
    }
    if access::save(&conn,&path,&display,!one(&fields,"public").is_empty()).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back("access")
}

pub(super) async fn save_site(State(state):State<App>,headers:HeaderMap,body:String)->Response {
    let fields=fields(&body);
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,one(&fields,"csrf")){return StatusCode::FORBIDDEN.into_response();}
    // 选项旁的「删除」按钮和「使用」在同一个表单里，按钮的 name 是 delete。
    if let Some(what)=Some(one(&fields,"delete")).filter(|v|!v.is_empty()) {
        return match site::delete(&conn,what) {
            Ok(())=>back("site"),
            Err(_)=>StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match one(&fields,"action") {
        "icon"=>return match site::choose(&conn,one(&fields,"icon")) {
            Ok(true)=>back("site"),
            Ok(false)=>failure(StatusCode::BAD_REQUEST,"这个图标现在用不了：请先上传图片或设置表情"),
            Err(_)=>StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        "reset_icon"=>{
            if site::reset_icon(&conn).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
            return back("site");
        }
        _=>{}
    }
    let Some(name)=site::valid_name(one(&fields,"site_name")) else {return failure(StatusCode::BAD_REQUEST,"站点名称需要 1–30 个字");};
    let emoji=one(&fields,"emoji").trim();
    let emoji=if emoji.is_empty() {None} else {
        match site::valid_emoji(emoji) {Some(e)=>Some(e),None=>return failure(StatusCode::BAD_REQUEST,"图标只能是一个表情或一两个字")}
    };
    if site::save_name(&conn,&name).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    if let Some(emoji)=emoji {
        if site::save_emoji(&conn,&emoji).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    }
    back("site")
}

pub(super) async fn upload_icon(State(state):State<App>,headers:HeaderMap,body:axum::body::Bytes)->Response {
    let content_type=headers.get(axum::http::header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).unwrap_or("");
    let Some(parts)=site::multipart(content_type,&body) else {return failure(StatusCode::BAD_REQUEST,"上传的数据格式不对，请重试");};
    let csrf=parts.iter().find(|(name,_)|name=="csrf").map(|(_,value)|String::from_utf8_lossy(value).into_owned()).unwrap_or_default();
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&csrf){return StatusCode::FORBIDDEN.into_response();}
    let Some((_,data))=parts.iter().find(|(name,_)|name=="icon") else {return failure(StatusCode::BAD_REQUEST,"请先选择一张图片");};
    if data.is_empty() {return failure(StatusCode::BAD_REQUEST,"请先选择一张图片");}
    if data.len()>site::ICON_MAX {return failure(StatusCode::BAD_REQUEST,"图片不能超过 256 KB");}
    let Some(mime)=site::sniff(data) else {return failure(StatusCode::BAD_REQUEST,"只支持 PNG、JPG、GIF、WebP、ICO。SVG 能内嵌脚本，不收");};
    if site::save_icon(&conn,mime,data).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back("site")
}

#[derive(Deserialize)]pub(super) struct Settings {csrf:String,bot_token:String,chat_id:String,clear_token:Option<String>}
pub(super) async fn save_settings(State(state):State<App>,headers:HeaderMap,Form(form):Form<Settings>)->Response {
    let Ok(mut conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    let token=form.bot_token.trim();let chat=form.chat_id.trim();
    if chat.len()>80||!chat.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_'||b==b'@')||token.len()>128||!token.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_'||b==b':') {
        return failure(StatusCode::BAD_REQUEST,"Telegram 设置格式有误");
    }
    let Ok(tx)=conn.transaction() else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !token.is_empty()||form.clear_token.is_some(){let new=if form.clear_token.is_some(){""}else{token};if tx.execute("INSERT INTO admin_settings(key,value) VALUES('bot_token',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[new]).is_err(){return StatusCode::INTERNAL_SERVER_ERROR.into_response();}}
    if tx.execute("INSERT INTO admin_settings(key,value) VALUES('chat_id',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[chat]).is_err()||tx.commit().is_err(){return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
    back("telegram")
}
pub(super) async fn test_telegram(State(state):State<App>,headers:HeaderMap,Form(form):Form<Csrf>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf){return StatusCode::FORBIDDEN.into_response();}
    drop(conn);
    let result=send_telegram(&state.db_path,"Telegram 测试通知").await;
    let msg=match result{Ok(())=>"已发送测试消息，请检查 Telegram。".to_string(),Err(err)=>format!("发送失败：{err}")};
    frame(&format!("<section class='card'><h1>{}</h1><a href='/admin/'>返回管理</a></section>",esc(&msg))).into_response()
}
async fn send_telegram(path:&str,text:&str)->Result<(),String>{
    let conn=db(path).map_err(|e|e.to_string())?;
    let token:String=conn.query_row("SELECT value FROM admin_settings WHERE key='bot_token'",[],|r|r.get(0)).unwrap_or_default();
    let chat:String=conn.query_row("SELECT value FROM admin_settings WHERE key='chat_id'",[],|r|r.get(0)).unwrap_or_default();
    drop(conn);
    if token.is_empty()||chat.is_empty(){return Err("请先保存 Bot Token 和 Chat ID".into());}
    let client=reqwest::Client::builder().timeout(Duration::from_secs(8)).build().map_err(|e|e.to_string())?;
    let response=client.post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&serde_json::json!({"chat_id":chat,"text":format!("{}：{text}",site::name())})).send().await.map_err(|_|"无法连接 Telegram API".to_string())?;
    if !response.status().is_success(){return Err(format!("Telegram API 返回 {}，请核对 Bot 与 Chat ID",response.status()));}
    Ok(())
}

/// 掉线/恢复通知。TCP 探测已经交给 agent，这里不再自己连任何目标。
pub(super) fn start_checks(path:Arc<str>){
    tokio::spawn(async move {
        loop {
            if let Err(e)=check_round(&path).await {eprintln!("probe checks: {e}");}
            if let Err(e)=expiry_round(&path).await {eprintln!("expiry checks: {e}");}
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}
async fn check_round(path:&str)->Result<(),Box<dyn Error>>{
    let nodes:Vec<(String,String,Option<i64>,i64)>= {
        let conn=db(path)?;
        let mut stmt=conn.prepare("SELECT n.id,n.name,n.last_seen,COALESCE(c.notify,1) FROM nodes n LEFT JOIN node_config c ON n.id=c.node_id")?;
        let rows=stmt.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id,name,seen,notify) in nodes {
        let online=seen.is_some_and(|v|now()-v<60);
        let Some(state)=transition(path,&format!("node:{id}"),online)? else {continue};
        if notify==0 {continue}
        let msg=format!("节点 {name} {}",if state {"已上线"}else{"已离线"});
        if let Err(e)=send_telegram(path,&msg).await {
            if !e.contains("请先保存") {eprintln!("Telegram notification failed: {e}");}
        }
    }
    Ok(())
}
/// 到期提醒与自动续期。跟掉线检查一起每 30 秒跑一轮，但只在 hub 本地时间
/// REMIND_HOUR 点之后动作。提醒同一节点同一天只发一次，发过的日子记在
/// alert_state 里，hub 重启也不会重发；续期则是改完日期立刻通知。
async fn expiry_round(path:&str)->Result<(),Box<dyn Error>>{
    let (today,hour,nodes)={
        let conn=db(path)?;
        let (today,hour):(String,i64)=conn.query_row("SELECT date('now','localtime'),CAST(strftime('%H','now','localtime') AS INTEGER)",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let mut stmt=conn.prepare("SELECT n.id,n.name,n.last_seen,c.notify,c.expires_at,c.billing_cycle,c.auto_renew FROM nodes n JOIN node_config c ON c.node_id=n.id WHERE c.expires_at!=''")?;
        let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,i64>(6)?)))?;
        let nodes:Vec<(String,String,Option<i64>,i64,String,String,i64)>=rows.collect::<rusqlite::Result<Vec<_>>>()?;
        (today,hour,nodes)
    };
    if hour<expiry::REMIND_HOUR {return Ok(());}
    let Some(today)=expiry::Date::parse(&today) else {return Ok(())};
    for (id,name,seen,notify,expires,cycle,auto) in nodes {
        let Some(date)=expiry::Date::parse(&expires) else {continue};
        let online=seen.is_some_and(|v|now()-v<60);
        match expiry::decide(date,today,&cycle,auto==1,online) {
            expiry::Action::Nothing=>{}
            expiry::Action::Renew(until)=>{
                // 以旧日期为条件更新：管理员恰好在这一刻手动改了日期，就以他改的为准。
                let changed=db(path)?.execute("UPDATE node_config SET expires_at=? WHERE node_id=? AND expires_at=?",params![until.to_string(),id,expires])?;
                if changed==1&&notify==1 {
                    let text=format!("节点 {name} 已过到期日且仍在线，已按{}自动续期至 {until}",expiry::cycle_label(&cycle));
                    if let Err(e)=send_telegram(path,&text).await {
                        if !e.contains("请先保存") {eprintln!("Telegram notification failed: {e}");}
                    }
                }
            }
            expiry::Action::Remind(left)=>{
                if notify==0 {continue}
                let key=format!("expire:{id}");
                let stamp=today.number();
                let sent:Option<i64>=db(path)?.query_row("SELECT state FROM alert_state WHERE key=?",[&key],|r|r.get(0)).optional()?;
                if sent==Some(stamp) {continue}
                let renewable=auto==1&&expiry::cycle_months(&cycle).is_some();
                let delivered=match send_telegram(path,&reminder(&name,date,left,renewable)).await {
                    Ok(())=>true,
                    // 没配 Telegram 就当今天已处理，免得每 30 秒空转一次。
                    Err(e) if e.contains("请先保存")=>true,
                    // 网络错误不记，下一轮再试。
                    Err(e)=>{eprintln!("Telegram notification failed: {e}");false}
                };
                if delivered {
                    db(path)?.execute("INSERT INTO alert_state(key,state) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET state=excluded.state",params![key,stamp])?;
                }
            }
        }
    }
    Ok(())
}

fn reminder(name:&str,date:expiry::Date,left:i64,renewable:bool)->String {
    let tail=if renewable {"。到期后仍在线会自动续期"} else {"，记得续费"};
    match left {
        0=>format!("节点 {name} 今天到期（{date}）{tail}"),
        l if l>0=>format!("节点 {name} 将于 {date} 到期，还剩 {l} 天{tail}"),
        l=>format!("节点 {name} 已过期 {} 天（{date}），但仍在线。续费后请在后台更新到期日期",-l),
    }
}

fn transition(path:&str,key:&str,value:bool)->rusqlite::Result<Option<bool>>{
    let conn=db(path)?;
    let old:Option<i64>=conn.query_row("SELECT state FROM alert_state WHERE key=?",[key],|r|r.get(0)).optional()?;
    conn.execute("INSERT INTO alert_state(key,state) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET state=excluded.state",params![key,value as i64])?;
    Ok(old.and_then(|o| (o!=value as i64).then_some(value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_behind_rewriting_proxy() {
        let mut headers=HeaderMap::new();
        headers.insert(ORIGIN,"https://status.example.com".parse().unwrap());
        headers.insert(HOST,"127.0.0.1:9911".parse().unwrap());
        assert!(!same_origin(&headers),"只有被改写的 Host、没有原始域名时拒绝");
        headers.insert("x-forwarded-host","status.example.com".parse().unwrap());
        assert!(same_origin(&headers),"反代把原始域名放在 X-Forwarded-Host 里时放行");
        headers.insert(ORIGIN,"https://evil.example".parse().unwrap());
        assert!(!same_origin(&headers),"别的站点提交过来一律拒绝");
        headers.insert(ORIGIN,"https://status.example.com".parse().unwrap());
        headers.insert("sec-fetch-site","cross-site".parse().unwrap());
        assert!(!same_origin(&headers));
    }

    #[test]
    fn origin_direct() {
        let mut headers=HeaderMap::new();
        headers.insert(ORIGIN,"https://Status.Example.com".parse().unwrap());
        headers.insert(HOST,"status.example.com:443".parse().unwrap());
        assert!(same_origin(&headers),"大小写和默认端口不影响判断");
        headers.insert(ORIGIN,"http://status.example.com".parse().unwrap());
        assert!(!same_origin(&headers),"公网域名不接受明文 http");
    }

    #[test]
    fn install_command_uses_the_visited_domain() {
        let mut headers=HeaderMap::new();
        headers.insert(HOST,"127.0.0.1:9911".parse().unwrap());
        headers.insert("x-forwarded-host","status.example.com".parse().unwrap());
        let base=public_base(&headers);
        assert_eq!(base,"https://status.example.com");
        let command=install_command(&base,"0123456789abcdef","ab");
        assert!(command.starts_with(&format!("curl -fsSL https://status.example.com/i/{} | sh -s -- --server https://status.example.com",install_key("ab"))));
        assert!(!command.contains("install.sh"),"不再有固定的脚本地址");
        assert_ne!(install_key("ab"),install_key("ac"),"换 token 钥匙跟着变");
        headers.insert("x-forwarded-host","evil.com;reboot".parse().unwrap());
        assert_eq!(public_base(&headers),"https://hub.example.com","带 shell 元字符的主机名不进命令");
    }

    #[test]
    fn traffic_numbers_in_gb() {
        assert_eq!(parse_gb(""),Ok(None),"留空=不改");
        assert_eq!(parse_gb("500"),Ok(Some(500*1024*1024*1024)));
        assert_eq!(parse_gb("1.5"),Ok(Some(1_610_612_736)));
        assert!(parse_gb("-1").is_err());
        assert!(parse_gb("abc").is_err());
        assert_eq!(gb(500*1024*1024*1024),"500");
        assert_eq!(gb(1_610_612_736),"1.5");
        assert!(mode_picker("").contains("value=\"sum\" selected"),"没设过默认上下行相加");
    }

    #[test]
    fn traffic_correction_applies_to_this_period_only() {
        let conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE traffic (node_id TEXT PRIMARY KEY,last_rx INTEGER,last_tx INTEGER,total_rx INTEGER,total_tx INTEGER,boot TEXT);
            CREATE TABLE traffic_day (node_id TEXT,day INTEGER,rx INTEGER,tx INTEGER,PRIMARY KEY(node_id,day));
            CREATE TABLE traffic_adjust (node_id TEXT PRIMARY KEY,period INTEGER,rx INTEGER,tx INTEGER);").unwrap();
        let today=local_today(&conn).unwrap().unwrap();
        conn.execute("INSERT INTO traffic_day VALUES ('n',?,100,40)",[today.number()]).unwrap();
        apply_fixes(&conn,"n",1,[Some(9000),None,Some(250),None]).unwrap();
        assert_eq!(period_usage(&conn,"n",1,today).unwrap(),(250,40),"只改下行，上行不动");
        let total:i64=conn.query_row("SELECT total_rx FROM traffic WHERE node_id='n'",[],|r|r.get(0)).unwrap();
        assert_eq!(total,9000);
        conn.execute("UPDATE traffic_adjust SET period=period-40",[]).unwrap();
        assert_eq!(period_usage(&conn,"n",1,today).unwrap(),(100,40),"上一期的校正不带进这一期");
    }

    #[test]
    fn reminder_wording() {
        let date=expiry::Date::parse("2026-10-01").unwrap();
        assert!(reminder("东京",date,3,true).contains("还剩 3 天。到期后仍在线会自动续期"));
        assert!(reminder("东京",date,0,false).contains("今天到期（2026-10-01），记得续费"));
        assert!(reminder("东京",date,-2,false).contains("已过期 2 天"));
    }

    /// 老数据库里没有 username 这一项，不写迁移也要能进得去。
    #[test]
    fn account_defaults_to_admin() {
        let conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);").unwrap();
        assert_eq!(username(&conn),"admin","没存过账号时按 admin 算");
        conn.execute("INSERT INTO admin_settings VALUES ('username','')",[]).unwrap();
        assert_eq!(username(&conn),"admin","存成空的也回退到 admin");
        conn.execute("UPDATE admin_settings SET value='merlin' WHERE key='username'",[]).unwrap();
        assert_eq!(username(&conn),"merlin");
    }

    #[test]
    fn account_shape() {
        assert_eq!(valid_username("  merlin  "),Some("merlin".into()),"前后空格去掉");
        assert_eq!(valid_username("a.b_c-d@e"),Some("a.b_c-d@e".into()));
        assert_eq!(valid_username(""),None);
        assert_eq!(valid_username("管理员"),None,"只收 ASCII");
        assert_eq!(valid_username(&"a".repeat(33)),None,"最长 32 位");
    }

    /// 账号和密码都要对。两样错哪一样，调用方拿到的都是同一个 false，
    /// 登录页也就只有「账号或密码错误」一句话可说。
    #[test]
    fn both_halves_must_match() {
        let conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);").unwrap();
        let salt=SaltString::encode_b64(&[7_u8;16]).unwrap();
        let hash=Argon2::default().hash_password(b"correct horse battery",&salt).unwrap().to_string();
        conn.execute("INSERT INTO admin_settings VALUES ('username','merlin')",[]).unwrap();
        conn.execute("INSERT INTO admin_settings VALUES ('password',?)",[&hash]).unwrap();
        assert!(credentials_ok(&conn,"merlin","correct horse battery"));
        assert!(credentials_ok(&conn," merlin ","correct horse battery"),"账号两边的空格不算数");
        assert!(!credentials_ok(&conn,"admin","correct horse battery"),"账号不对");
        assert!(!credentials_ok(&conn,"merlin","correct horse"),"密码不对");
        assert!(!credentials_ok(&conn,"admin","correct horse"),"两样都不对");
        assert!(password_ok(&conn,"correct horse battery"));
        assert!(!password_ok(&conn,""),"空密码进不来");
    }

    #[test]
    fn only_our_own_backups_are_accepted() {
        let dir=std::env::temp_dir();
        let good=dir.join(format!("avalon-test-good-{}.db",std::process::id())).to_str().unwrap().to_string();
        let _=std::fs::remove_file(&good);
        Connection::open(&good).unwrap().execute_batch("CREATE TABLE nodes (id TEXT); CREATE TABLE admin_settings (key TEXT, value TEXT);
            CREATE TABLE admin_sessions (hash TEXT, expires INTEGER); INSERT INTO admin_sessions VALUES ('x',1);").unwrap();
        assert!(check_backup(&good).is_ok());
        let left:i64=Connection::open(&good).unwrap().query_row("SELECT COUNT(*) FROM admin_sessions",[],|r|r.get(0)).unwrap();
        assert_eq!(left,0,"换上去之前清空登录会话");
        Connection::open(&good).unwrap().execute_batch("CREATE TRIGGER t AFTER INSERT ON nodes BEGIN DELETE FROM nodes; END;").unwrap();
        assert!(check_backup(&good).is_err(),"带触发器的不收");
        let _=std::fs::remove_file(&good);
        let other=dir.join(format!("avalon-test-other-{}.db",std::process::id())).to_str().unwrap().to_string();
        let _=std::fs::remove_file(&other);
        Connection::open(&other).unwrap().execute_batch("CREATE TABLE something (x INTEGER);").unwrap();
        assert!(check_backup(&other).is_err(),"别的库不收");
        let _=std::fs::remove_file(&other);
    }

    #[test]
    fn device_names() {
        assert_eq!(device("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1"),"iPhone · Safari");
        assert_eq!(device("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0"),"Windows · Edge");
        assert_eq!(device("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36"),"Android · Chrome");
        assert_eq!(device(""),"未知设备");
    }

    /// 偷到密码的人刚登进来，不能反手把你踢掉。
    #[test]
    fn new_devices_cannot_kick() {
        let conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE admin_sessions (hash TEXT PRIMARY KEY, expires INTEGER NOT NULL, created INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL DEFAULT '', device TEXT NOT NULL DEFAULT '');").unwrap();
        let later=now()+3600;
        conn.execute("INSERT INTO admin_sessions(hash,expires,created) VALUES ('fresh',?,?)",params![later,now()-60]).unwrap();
        assert!(can_manage(&conn,"fresh"),"只有它一个在线，刚登录也能管");
        conn.execute("INSERT INTO admin_sessions(hash,expires,created) VALUES ('old',?,?)",params![later,now()-2*86400]).unwrap();
        assert!(!can_manage(&conn,"fresh"),"有更早的设备在，新设备不能踢人");
        assert!(can_manage(&conn,"old"),"满 24 小时的可以");
        conn.execute("INSERT INTO admin_sessions(hash,expires,created) VALUES ('legacy',?,0)",[later]).unwrap();
        assert!(can_manage(&conn,"legacy"),"升级前就登录着的算旧设备");
    }

    #[test]
    fn swapping_neighbours() {
        let order=["a","b","c"];
        assert_eq!(swapped(&order,&"b",true),Some(vec!["b","a","c"]));
        assert_eq!(swapped(&order,&"b",false),Some(vec!["a","c","b"]));
        assert_eq!(swapped(&order,&"a",true),None,"第一个不能再上移");
        assert_eq!(swapped(&order,&"c",false),None,"最后一个不能再下移");
        assert_eq!(swapped(&order,&"x",true),None,"不存在的不动");
    }

    /// 老数据 sort 全是 0。第一次移动就要把整张表按当前顺序编好号，不然两个 0 对调还是 0。
    #[test]
    fn first_move_renumbers_everything() {
        let mut conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE nodes (id TEXT PRIMARY KEY, name TEXT NOT NULL, sort INTEGER NOT NULL DEFAULT 0);
            INSERT INTO nodes(id,name) VALUES ('z','c'),('x','a'),('y','b');").unwrap();
        assert!(move_row(&mut conn,"nodes","sort,name","z",true).unwrap());
        let order:Vec<(String,i64)>=conn.prepare("SELECT id,sort FROM nodes ORDER BY sort").unwrap()
            .query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(order,vec![(String::from("x"),1),(String::from("z"),2),(String::from("y"),3)]);
        assert!(!move_row(&mut conn,"nodes","sort,name","x",true).unwrap(),"第一个再上移什么都不做");
    }

    /// ?open= 的值会进到 HTML 的 id 里，别的字符一律当没传。
    #[test]
    fn open_parameter_is_fenced_in() {
        assert_eq!(opened(Some("site")),"site");
        assert_eq!(opened(Some("n-0123456789abcdef")),"n-0123456789abcdef");
        assert_eq!(opened(Some("\"><script>")),"");
        assert_eq!(opened(Some("SITE")),"");
        assert_eq!(opened(None),"");
        assert!(card("site","站点",false,"x").contains("<details class=\"card\" id=\"site\">"));
        assert!(card("site","站点",true,"x").contains("id=\"site\" open>"),"带上 open 属性才是展开的");
    }

    #[test]
    fn target_parsing() {
        assert_eq!(parse_target("1.1.1.1:443"),Some(("1.1.1.1".into(),443)));
        assert_eq!(parse_target("[2606:4700:4700::1111]:443"),Some(("2606:4700:4700::1111".into(),443)));
        assert_eq!(parse_target("example.com:80"),Some(("example.com".into(),80)));
        assert_eq!(parse_target("1.1.1.1"),None,"没有端口");
        assert_eq!(parse_target("1.1.1.1:0"),None,"端口 0");
        assert_eq!(parse_target("2606:4700::1111:443"),None,"未加方括号的 IPv6");
        assert_eq!(parse_target("../etc:80"),None);
    }
}
