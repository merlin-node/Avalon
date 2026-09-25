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

pub(super) fn set_password(path: &str, password: &str) -> Result<(), Box<dyn Error>> {
    // argon2 的错误类型不实现 std::error::Error（要开 std 特性才有），
    // 所以不能直接用 ? 转成 Box<dyn Error>，先转成字符串。
    let salt=SaltString::encode_b64(&random_bytes(16)?).map_err(|e|e.to_string())?;
    let hash=Argon2::default().hash_password(password.as_bytes(),&salt).map_err(|e|e.to_string())?.to_string();
    let mut conn=db(path)?;
    let tx=conn.transaction()?;
    tx.execute("INSERT INTO admin_settings(key,value) VALUES('password',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&hash])?;
    tx.execute("DELETE FROM admin_sessions",[])?;
    tx.commit()?;
    Ok(())
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
header{{background:#fff;border-bottom:1px solid #d9dfe3;padding:14px max(16px,calc((100vw - 1050px)/2));display:flex;align-items:center;gap:24px}}a{{color:#386a9c}}header a{{text-decoration:none}}main{{max-width:1050px;margin:28px auto;padding:0 16px 60px}}h1{{font-size:21px;margin:0 0 15px}}h2{{font-size:16px;margin:0 0 12px}}.card{{background:#fff;border:1px solid #d9dfe3;padding:20px;margin:12px 0}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(215px,1fr));gap:12px}}label{{display:block;color:#626e76;font-size:13px}}input,select{{display:block;width:100%;border:1px solid #cdd6dc;border-radius:0;padding:9px 10px;background:#fff;color:#242b30;font:inherit;margin-top:5px}}input[type=checkbox]{{width:auto;display:inline-block;margin-right:5px}}input[type=radio]{{width:auto;display:inline-block;margin:0 5px 0 0;vertical-align:-1px}}.icons{{display:flex;flex-wrap:wrap;gap:10px;margin-top:6px}}.icon-choice{{display:flex;flex-direction:column;align-items:center;gap:6px;border:1px solid #d9dfe3;background:#fff;padding:10px 12px;cursor:pointer;min-width:96px;color:#242b30}}.icon-choice img{{object-fit:contain}}.upload{{margin-top:22px;padding-top:16px;border-top:1px dashed #e3e8eb}}button.link{{background:none;color:#b04747;padding:0;font-size:13px}}.icon-choice:has(input:checked){{border-color:#346d9b;box-shadow:0 0 0 1px #346d9b}}button{{background:#346d9b;color:white;border:0;border-radius:0;padding:10px 16px;cursor:pointer;font:inherit}}button.secondary{{background:#e8edef;color:#26333b}}button.danger{{background:#fff;color:#b04747;border:1px solid #e0c4c4}}form{{margin:0}}.actions{{display:flex;gap:10px;align-items:center;margin-top:15px;flex-wrap:wrap}}small,.muted{{color:#6d797f}}.node{{border-top:1px solid #e3e8eb;padding:15px 0}}.node:first-child{{border-top:0}}details>summary{{cursor:pointer;font-weight:650;font-size:15px}}code{{overflow-wrap:anywhere}}.ok{{color:#329356}}.down{{color:#c05252}}.picker{{display:grid;grid-template-columns:repeat(auto-fill,minmax(180px,1fr));gap:4px 12px;max-height:230px;overflow:auto;border:1px solid #e3e8eb;padding:10px;margin-top:6px}}.picker label{{color:#242b30}}.brand{{display:flex;align-items:center;gap:8px}}.brand img{{object-fit:contain}}.cmd{{display:block;white-space:pre-wrap;word-break:break-all;background:#f2f5f7;border:1px solid #d9dfe3;padding:12px;margin:10px 0;font:13px/1.55 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color:#242b30;-webkit-user-select:all;user-select:all}}@media(max-width:600px){{main{{margin:12px auto}}.card{{padding:14px}}}}</style><header><strong class="brand"><img src="{base}/icon" alt="" width="20" height="20">{site}</strong><a href="/">首页</a><span class="muted">管理</span></header><main>{body}</main></html>"#))
}
fn failure(code:StatusCode,message:&str)->Response {(code,frame(&format!("<p>{}</p><p><a href='/admin/'>返回管理</a></p>",esc(message)))).into_response()}

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
    format!(r#"<section class="card"><h2>站点</h2><form method="post" action="/admin/site"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>站点名称<input name="site_name" value="{}" maxlength="30" required></label>
<label>表情图标<input name="emoji" maxlength="16" placeholder="📡"></label></div>
<div class="actions"><button>保存</button></div></form>
<div class="node"><form method="post" action="/admin/site"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="action" value="icon">
<div class="icons">{choices}</div><div class="actions"><button>使用</button></div></form>
<form class="upload" method="post" action="/admin/site/icon" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{csrf}">
<label>上传图片<input type="file" name="icon" accept="image/png,image/jpeg,image/gif,image/webp,image/x-icon" required></label>
<div class="actions"><button class="secondary">上传</button></div></form></div></section>"#,
        esc(&site::name()))
}

/// 访问控制：后台地址、展示页域名、公开页开关。
fn access_section(csrf:&str,here:&str)->String {
    let checked=if access::public_enabled() {"checked"} else {""};
    format!(r#"<section class="card"><h2>访问控制</h2>
<form method="post" action="/admin/access"><input type="hidden" name="csrf" value="{csrf}"><div class="grid">
<label>后台地址<input name="admin_path" value="{}" maxlength="48" required></label>
<label>展示页域名<input name="display_domain" value="{}" maxlength="253" placeholder="status.example.com"></label></div>
<div class="actions"><label><input type="checkbox" name="public" value="1" {checked}>开放公开状态页</label><button>保存</button></div></form>
<p class="muted">主域名 <code>{}</code>　忘记后台地址：<code>docker compose exec avalon probe-hub access</code></p></section>"#,
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
    let Ok(mut stmt)=conn.prepare("SELECT id,name FROM nodes ORDER BY name") else {return String::new()};
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

fn monitors_section(conn:&Connection,csrf:&str)->String {
    let mut body=String::from("<section class=\"card\"><h2>延迟监控</h2>");
    let Ok(mut stmt)=conn.prepare("SELECT id,name,host,port,interval,auto_join FROM monitors ORDER BY id") else {return body};
    let Ok(rows)=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?))) else {return body};
    let monitors:Vec<_>=rows.flatten().collect();
    if monitors.is_empty() {body.push_str("<p class=\"muted\">还没有监控。</p>");}
    for (id,name,host,port,interval,auto) in monitors {
        let mut chosen:HashSet<String>=HashSet::new();
        if let Ok(mut stmt)=conn.prepare("SELECT node_id FROM monitor_nodes WHERE monitor_id=?") {
            if let Ok(rows)=stmt.query_map([id],|r|r.get::<_,String>(0)) {chosen.extend(rows.flatten());};
        }
        let target=if host.contains(':') {format!("[{host}]:{port}")} else {format!("{host}:{port}")};
        body.push_str(&format!("<div class=\"node\"><details><summary>{} <small>· {} · {interval}s · {} 个节点</small></summary>{}</details></div>",
            esc(&name),esc(&target),chosen.len(),monitor_form(conn,csrf,Some(id),&name,&target,interval,auto==1,&chosen)));
    }
    body.push_str(&format!("</section><section class=\"card\"><h2>添加监控</h2>{}</section>",
        monitor_form(conn,csrf,None,"","",60,false,&HashSet::new())));
    body
}

pub(super) async fn page(State(state):State<App>,headers:HeaderMap)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let Some((_,csrf))=session(&headers,&conn) else {
        let configured:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM admin_settings WHERE key='password')",[],|r|r.get(0)).unwrap_or(false);
        let tip=if configured {"请输入在 VPS 上运行 admin-setup 得到的密码。"} else {"先在 VPS 执行：runuser -u probe -- /usr/local/bin/probe-hub admin-setup --db /var/lib/linux-probe/probe.db"};
        return frame(&format!("<section class='card'><h1>管理员登录</h1><p>{}</p><form method='post' action='/admin/login'><label>密码<input type='password' name='password' autocomplete='current-password' required></label><div class='actions'><button>登录</button></div></form></section>",esc(tip))).into_response();
    };
    let bot:bool=conn.query_row("SELECT value!='' FROM admin_settings WHERE key='bot_token'",[],|r|r.get(0)).unwrap_or(false);
    let chat:String=conn.query_row("SELECT value FROM admin_settings WHERE key='chat_id'",[],|r|r.get(0)).unwrap_or_default();
    let base=public_base(&headers);
    let mut body=format!("<h1>节点管理</h1>{}{}<section class='card'><h2>节点</h2>",site_section(&conn,&csrf),access_section(&csrf,&request_hosts(&headers).into_iter().next().unwrap_or_default()));
    let mut stmt=match conn.prepare("SELECT n.id,n.name,n.public,n.last_seen,n.token,c.country,c.display_ip,c.notify,c.remark,c.price,c.currency,c.billing_cycle,c.expires_at,c.traffic_limit,c.traffic_mode,c.traffic_reset_day,(SELECT COUNT(*) FROM monitor_nodes m WHERE m.node_id=n.id),COALESCE(c.auto_renew,1),COALESCE(t.total_rx,0),COALESCE(t.total_tx,0) FROM nodes n LEFT JOIN node_config c ON n.id=c.node_id LEFT JOIN traffic t ON t.node_id=n.id ORDER BY n.name") {Ok(s)=>s,Err(_)=>return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?.unwrap_or_default(),r.get::<_,Option<String>>(6)?.unwrap_or_default(),r.get::<_,Option<i64>>(7)?.unwrap_or(1),r.get::<_,Option<String>>(8)?.unwrap_or_default(),r.get::<_,Option<f64>>(9)?.unwrap_or(0.0),r.get::<_,Option<String>>(10)?.unwrap_or_default(),r.get::<_,Option<String>>(11)?.unwrap_or_default(),r.get::<_,Option<String>>(12)?.unwrap_or_default(),r.get::<_,Option<i64>>(13)?.unwrap_or(0),r.get::<_,Option<String>>(14)?.unwrap_or_default(),r.get::<_,Option<i64>>(15)?.unwrap_or(1),r.get::<_,i64>(16)?,r.get::<_,i64>(17)?,r.get::<_,i64>(18)?,r.get::<_,i64>(19)?)));
    let today=local_today(&conn).ok().flatten();
    if let Ok(rows)=rows { for row in rows.flatten() {
        let (id,name,public,seen,token,country,ip,notify,remark,price,currency,cycle,expires,limit,mode,reset,monitors,auto_renew,total_rx,total_tx)=row;
        let (month_rx,month_tx)=today.and_then(|today|period_usage(&conn,&id,reset,today).ok()).unwrap_or((0,0));
        let limit_text=if limit>0 {gb(limit)} else {String::new()};
        let cycle_select=cycle_picker(&cycle);
        let is_online=seen.is_some_and(|v|now()-v<30);
        let status=if is_online {"<span class='ok'>在线</span>"} else {"<span class='down'>离线</span>"};
        body.push_str(&format!(r#"<div class="node"><details><summary>{status}　{} <small>· {monitors} 个监控 · {}</small></summary><p class="muted">{}</p><p class="muted">安装命令</p><code class="cmd">{}</code><p class="muted">卸载：<code>{}</code></p><form method="post" action="/admin/nodes/{}"><input type="hidden" name="csrf" value="{}"><div class="grid">
<label>名称<input name="name" value="{}" maxlength="80" required></label><label>国家/地区代码<input name="country" value="{}" maxlength="2" placeholder="HK"></label><label>手填 IP<input name="display_ip" value="{}" maxlength="100"></label><label>备注<input name="remark" value="{}" maxlength="300"></label><label>价格<input name="price" type="number" min="0" step="0.01" value="{}"></label><label>货币<input name="currency" value="{}" maxlength="8" placeholder="$"></label><label>计费周期{}</label><label>到期日期<input name="expires_at" type="date" value="{}"></label><label>每月额度（GB）<input name="traffic_limit" type="number" min="0" step="0.01" value="{}" placeholder="不限"></label><label>计算方式{}</label><label>流量重置日<input name="traffic_reset_day" type="number" min="1" max="31" value="{}"></label></div>
<details><summary class="muted">流量校正</summary><div class="grid">
<label>总下行（GB）<input name="fix_total_rx" type="number" min="0" step="0.01" placeholder="现在 {}"></label><label>总上行（GB）<input name="fix_total_tx" type="number" min="0" step="0.01" placeholder="现在 {}"></label>
<label>本期下行（GB）<input name="fix_month_rx" type="number" min="0" step="0.01" placeholder="现在 {}"></label><label>本期上行（GB）<input name="fix_month_tx" type="number" min="0" step="0.01" placeholder="现在 {}"></label></div></details><div class="actions"><label><input type="checkbox" name="public" value="1" {}>公开显示</label><label><input type="checkbox" name="notify" value="1" {}>掉线/到期通知</label><label><input type="checkbox" name="auto_renew" value="1" {}>到期后仍在线自动续期</label><button>保存节点</button><button class="secondary" name="action" value="rotate" formnovalidate>重新生成 Token</button><label><input type="checkbox" name="confirm" value="1">确认</label><button class="danger" name="action" value="delete" formnovalidate>删除节点</button></div></form></details></div>"#,
        esc(&name),if public==1 {"公开"} else {"私有"},ip_line(&conn,&id),esc(&install_command(&base,&id,&token)),esc(&uninstall_command()),esc(&id),csrf,esc(&name),esc(&country),esc(&ip),esc(&remark),price,esc(&currency),cycle_select,esc(&expires),limit_text,mode_picker(&mode),reset,gb(total_rx),gb(total_tx),gb(month_rx),gb(month_tx),if public==1 {"checked"} else {""},if notify==1 {"checked"} else {""},if auto_renew==1 {"checked"} else {""}));
    }}
    body.push_str(&format!(r#"</section><section class="card"><h2>添加节点</h2><form method="post" action="/admin/nodes"><input type="hidden" name="csrf" value="{csrf}"><label>节点名称<input name="name" maxlength="80" required></label><div class="actions"><button>创建并显示 Agent 凭据</button></div></form></section>"#));
    body.push_str(&monitors_section(&conn,&csrf));
    body.push_str(&format!(r#"<section class="card"><h2>Telegram 通知</h2><form method="post" action="/admin/settings"><input type="hidden" name="csrf" value="{}"><div class="grid"><label>Bot Token<input type="password" name="bot_token" autocomplete="off" placeholder="{}"></label><label>Chat ID<input name="chat_id" value="{}" maxlength="80"></label></div><div class="actions"><button>保存通知设置</button><label><input type="checkbox" name="clear_token" value="1">清除 Token</label></div></form><form method="post" action="/admin/test"><input type="hidden" name="csrf" value="{}"><div class="actions"><button class="secondary">发送测试通知</button></div></form></section><form method="post" action="/admin/logout"><input type="hidden" name="csrf" value="{}"><button class="secondary">退出登录</button></form>"#,
        csrf,if bot {"已保存，留空则不修改"} else {"123456:ABC..."},esc(&chat),csrf,csrf));
    frame(&body).into_response()
}

#[derive(Deserialize)] pub(super) struct Login { password:String }
pub(super) async fn login(State(state):State<App>,headers:HeaderMap,Form(form):Form<Login>)->Response {
    if !same_origin(&headers) {return failure(StatusCode::FORBIDDEN,"请求来源不正确");}
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let source=client(&headers);
    let attempt:Option<(i64,i64,i64)>=conn.query_row("SELECT failures,first,until FROM login_attempts WHERE source=?",[&source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().ok().flatten();
    if attempt.is_some_and(|(_,_,until)|until>now()) {return failure(StatusCode::TOO_MANY_REQUESTS,"该地址登录失败次数过多，请十五分钟后再试");}
    // argon2 是故意算得慢的，同时只放一个进去；排队而不是直接拒绝，
    // 否则两个人同时点一下登录就有一个吃 429。
    let Ok(Ok(permit))=tokio::time::timeout(Duration::from_secs(3),LOGIN_GATE.acquire()).await else {
        return failure(StatusCode::TOO_MANY_REQUESTS,"登录请求太多，请稍后重试");
    };
    let hash:String=conn.query_row("SELECT value FROM admin_settings WHERE key='password'",[],|r|r.get(0)).unwrap_or_default();
    let verified=PasswordHash::new(&hash).ok().is_some_and(|h|Argon2::default().verify_password(form.password.as_bytes(),&h).is_ok());
    drop(permit);
    if !verified {
        let (failures,first)=match attempt {
            Some((failures,first,_)) if now()-first<LOGIN_WINDOW => (failures+1,first),
            _ => (1,now()),
        };
        let until=if failures>=LOGIN_TRIES {now()+LOGIN_WINDOW} else {0};
        let _=conn.execute("INSERT INTO login_attempts(source,failures,first,until) VALUES (?,?,?,?) ON CONFLICT(source) DO UPDATE SET failures=excluded.failures,first=excluded.first,until=excluded.until",params![source,failures,first,until]);
        let _=conn.execute("DELETE FROM login_attempts WHERE until<? AND first<?",params![now(),now()-LOGIN_WINDOW]);
        return failure(StatusCode::UNAUTHORIZED,"密码错误");
    }
    let _=conn.execute("DELETE FROM login_attempts WHERE source=?",[&source]);
    let Ok(raw)=random_bytes(32) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    let token=hex(&raw);
    if conn.execute("INSERT INTO admin_sessions(hash,expires) VALUES (?,?)",params![digest(&token),now()+SESSION_LIFETIME]).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
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
#[derive(Deserialize)]pub(super) struct NewNode {csrf:String,name:String}
pub(super) async fn add_node(State(state):State<App>,headers:HeaderMap,Form(form):Form<NewNode>)->Response {
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,&form.csrf) {return StatusCode::FORBIDDEN.into_response();}
    let name=form.name.trim();if name.is_empty()||name.len()>80 {return failure(StatusCode::BAD_REQUEST,"节点名称长度需在 1–80 字节之间");}
    let (Ok(id),Ok(token))=(new_secret(8),new_secret(32)) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if conn.execute("INSERT INTO nodes(id,name,token,public) VALUES (?,?,?,1)",params![id,name,token]).is_err(){return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
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
    Redirect::to(&access::admin_home()).into_response()
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
    if tx.execute("INSERT INTO monitors(name,host,port,interval,auto_join,enabled) VALUES (?,?,?,?,?,1)",
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
    Redirect::to(&access::admin_home()).into_response()
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
        return Redirect::to(&access::admin_home()).into_response();
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
    Redirect::to(&access::admin_home()).into_response()
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
    Redirect::to(&access::admin_home()).into_response()
}

pub(super) async fn save_site(State(state):State<App>,headers:HeaderMap,body:String)->Response {
    let fields=fields(&body);
    let Ok(conn)=db(&state.db_path) else{return StatusCode::INTERNAL_SERVER_ERROR.into_response()};
    if !authorized(&headers,&conn,one(&fields,"csrf")){return StatusCode::FORBIDDEN.into_response();}
    // 选项旁的「删除」按钮和「使用」在同一个表单里，按钮的 name 是 delete。
    if let Some(what)=Some(one(&fields,"delete")).filter(|v|!v.is_empty()) {
        return match site::delete(&conn,what) {
            Ok(())=>Redirect::to(&access::admin_home()).into_response(),
            Err(_)=>StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    match one(&fields,"action") {
        "icon"=>return match site::choose(&conn,one(&fields,"icon")) {
            Ok(true)=>Redirect::to(&access::admin_home()).into_response(),
            Ok(false)=>failure(StatusCode::BAD_REQUEST,"这个图标现在用不了：请先上传图片或设置表情"),
            Err(_)=>StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        "reset_icon"=>{
            if site::reset_icon(&conn).is_err() {return StatusCode::INTERNAL_SERVER_ERROR.into_response();}
            return Redirect::to(&access::admin_home()).into_response();
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
    Redirect::to(&access::admin_home()).into_response()
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
    Redirect::to(&access::admin_home()).into_response()
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
    Redirect::to(&access::admin_home()).into_response()
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
