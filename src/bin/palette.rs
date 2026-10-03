//! 配色。后台「配色」卡片里选一套，存在 admin_settings 的 `palette`，公开页、主域名、登录页和后台一起换。
//!
//! 访客只能切亮暗，换不了配色：公开页上没有选配色的地方，颜色是 hub 发页面时直接写进去的，
//! 选的是什么，谁打开都是什么。
//!
//! 「原版」什么都不加，页面用的就是 theme/src/index.css 和 ADMIN_CSS 里本来的颜色，
//! 没选过配色的老库升级后一点都不变。其余每套各有一份亮色、一份暗色。
//! 暗色进度条上的白字、按钮上的白字，对比度都按 4.5 以上挑的。
use super::*;
use std::sync::RwLock;

pub(crate) const DEFAULT: &str = "orig";

/// 一套配色在亮或暗一边的颜色。前面这些对应 theme/src/index.css 里的同名变量，
/// field、btn、ok_text 只给后台用（输入框底色、按钮底色、「在线」字色）。
pub(super) struct Tone {
    bg:&'static str, fg:&'static str, card:&'static str, pop:&'static str,
    primary:&'static str, pfg:&'static str, muted:&'static str, mfg:&'static str,
    accent:&'static str, border:&'static str, nav:&'static str, chart:[&'static str;5],
    warn:&'static str, danger:&'static str, track:&'static str, bar_text:&'static str,
    ok:&'static str, warn_bar:&'static str, danger_bar:&'static str, dot_on:&'static str, dot_off:&'static str,
    field:&'static str, btn:&'static str, ok_text:&'static str,
}

pub(super) struct Palette { pub(super) key:&'static str, pub(super) name:&'static str, dark:Tone, light:Tone }

pub(super) static PALETTES:[Palette;11]=[
    Palette{key:"orig",name:"原版",
        dark:Tone{bg:"#31363b",fg:"#f1f1f1",card:"#1c1d26",pop:"#22232e",primary:"#4992ff",pfg:"#fff",muted:"#22232e",mfg:"#aaaaaa",accent:"#303241",border:"#3a3e41",nav:"#1c2127",chart:["oklch(0.72 0.08 252)","oklch(0.72 0.08 162)","oklch(0.78 0.1 76)","oklch(0.7 0.09 342)","oklch(0.74 0.06 212)"],warn:"#f0ad4e",danger:"#e0605c",track:"rgb(255 255 255/7.5%)",bar_text:"#fff",ok:"linear-gradient(to right,rgb(77 133 58),rgb(54 126 54))",warn_bar:"linear-gradient(to right,rgb(153 95 13),rgb(181 113 15))",danger_bar:"linear-gradient(to right,rgb(145 35 32),rgb(161 38 35))",dot_on:"linear-gradient(rgb(77 133 58),rgb(54 126 54))",dot_off:"linear-gradient(rgb(155 37 34),rgb(161 38 35))",field:"#22232e",btn:"#2d6cc8",ok_text:"#5fb865"},
        light:Tone{bg:"#f5f5f5",fg:"#212121",card:"#ffffff",pop:"#ffffff",primary:"#337ab7",pfg:"#fff",muted:"#f9f9f9",mfg:"#777777",accent:"#e6e6e6",border:"#dddddd",nav:"#f9f9f9",chart:["oklch(0.56 0.09 252)","oklch(0.62 0.08 162)","oklch(0.7 0.1 72)","oklch(0.58 0.09 342)","oklch(0.62 0.06 212)"],warn:"#8a6d3b",danger:"#a94442",track:"#f5f5f5",bar_text:"#000",ok:"linear-gradient(rgb(92 184 92),rgb(68 157 68))",warn_bar:"linear-gradient(rgb(240 173 78),rgb(236 151 31))",danger_bar:"linear-gradient(rgb(217 83 79),rgb(201 48 44))",dot_on:"linear-gradient(rgb(92 184 92),rgb(68 157 68))",dot_off:"linear-gradient(rgb(217 83 79),rgb(201 48 44))",field:"#fff",btn:"#346d9b",ok_text:"#329356"}},
    Palette{key:"komari",name:"紫",
        dark:Tone{bg:"#14121f",fg:"#eeeef0",card:"#1c1a26",pop:"#221f2d",primary:"#baa7ff",pfg:"#14121f",muted:"#24212f",mfg:"#b5b2bc",accent:"#2b1f4c",border:"#322e3b",nav:"#1c1a26",chart:["#9b8afb","#3dd68c","#ffc53d","#f47fb0","#70b8ff"],warn:"#ffca16",danger:"#ff9592",track:"#323035",bar_text:"#fff",ok:"linear-gradient(to right,#25855a,#25855a)",warn_bar:"linear-gradient(to right,#a36608,#a36608)",danger_bar:"linear-gradient(to right,#c62f36,#c62f36)",dot_on:"linear-gradient(#3dd68c,#30a46c)",dot_off:"linear-gradient(#ec5d5e,#e5484d)",field:"#0f0d17",btn:"#6e56cf",ok_text:"#3dd68c"},
        light:Tone{bg:"#fbfaff",fg:"#211f26",card:"#ffffff",pop:"#ffffff",primary:"#6e56cf",pfg:"#fff",muted:"#f9f8fe",mfg:"#65636d",accent:"#efeafe",border:"#e4e1ec",nav:"#f4f0fe",chart:["#6e56cf","#12a594","#e2a336","#d6409f","#0090ff"],warn:"#ab6400",danger:"#ce2c31",track:"#efedf5",bar_text:"#000",ok:"linear-gradient(#5bd49a,#3cc283)",warn_bar:"linear-gradient(#ffd66b,#ffc53d)",danger_bar:"linear-gradient(#f77b80,#ec5d62)",dot_on:"linear-gradient(#3cc283,#30a46c)",dot_off:"linear-gradient(#ec5d62,#e5484d)",field:"#fff",btn:"#6e56cf",ok_text:"#218358"}},
    Palette{key:"sea",name:"青",
        dark:Tone{bg:"#0b1821",fg:"#e3f0f5",card:"#0f212c",pop:"#13283a",primary:"#3cc9de",pfg:"#04222a",muted:"#122634",mfg:"#8fb0c0",accent:"#173244",border:"#1d3646",nav:"#08131a",chart:["#3cc9de","#4cd6a6","#f2c14e","#ef7fa8","#8ea7ff"],warn:"#f2c14e",danger:"#ff8a80",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#0f7863,#12806a)",warn_bar:"linear-gradient(to right,#94560a,#9a5b00)",danger_bar:"linear-gradient(to right,#b8302f,#c03538)",dot_on:"linear-gradient(#2bd4a0,#16b386)",dot_off:"linear-gradient(#f0605c,#d9443f)",field:"#0b1821",btn:"#0e7c91",ok_text:"#2bd4a0"},
        light:Tone{bg:"#eef5f8",fg:"#0f2530",card:"#ffffff",pop:"#ffffff",primary:"#0e7c91",pfg:"#fff",muted:"#f5fafc",mfg:"#557282",accent:"#e1f1f6",border:"#d0e2e9",nav:"#e2f0f5",chart:["#0e7c91","#1a9a6c","#d4930f","#c74a7a","#4b6bd6"],warn:"#8a5a00",danger:"#b3332f",track:"#e9f1f4",bar_text:"#000",ok:"linear-gradient(#5fdcb6,#3fcca0)",warn_bar:"linear-gradient(#f9d36b,#f4bf3a)",danger_bar:"linear-gradient(#f58c8a,#ee6b68)",dot_on:"linear-gradient(#3fcca0,#1fae84)",dot_off:"linear-gradient(#ee6b68,#e5534f)",field:"#fff",btn:"#0e7c91",ok_text:"#138060"}},
    Palette{key:"amber",name:"琥珀",
        dark:Tone{bg:"#18140f",fg:"#f2eadf",card:"#211c15",pop:"#2a231b",primary:"#e8a33d",pfg:"#241806",muted:"#271f18",mfg:"#b5a894",accent:"#33291e",border:"#3a3027",nav:"#120f0b",chart:["#e8a33d","#8fc47a","#5fb8c9","#e07a6a","#b49cf0"],warn:"#ffcc66",danger:"#f07a6a",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#36753f,#3a7d44)",warn_bar:"linear-gradient(to right,#9a580a,#a35f0a)",danger_bar:"linear-gradient(to right,#ad3536,#b8393a)",dot_on:"linear-gradient(#7fd18a,#4fae5c)",dot_off:"linear-gradient(#ec6a58,#d9503f)",field:"#18140f",btn:"#a85a0a",ok_text:"#7fd18a"},
        light:Tone{bg:"#f6f3ee",fg:"#2a2118",card:"#fffefb",pop:"#ffffff",primary:"#a85a0a",pfg:"#fff",muted:"#faf7f2",mfg:"#7a6a55",accent:"#f2e8d8",border:"#e4d9c8",nav:"#efe6d8",chart:["#c27a12","#4f8f3a","#2f8aa0","#c24f40","#7a5fc9"],warn:"#8a5a00",danger:"#b03a2e",track:"#f1ebe2",bar_text:"#000",ok:"linear-gradient(#7fd08a,#5fbf6d)",warn_bar:"linear-gradient(#f7cd6a,#f0b53c)",danger_bar:"linear-gradient(#f0907f,#e8705c)",dot_on:"linear-gradient(#5fbf6d,#3f9a4c)",dot_off:"linear-gradient(#e8705c,#d9534f)",field:"#fff",btn:"#a85a0a",ok_text:"#2f7d3a"}},
    Palette{key:"sakura",name:"樱",
        dark:Tone{bg:"#1a1117",fg:"#f6e9f0",card:"#22161e",pop:"#2b1c26",primary:"#f07ba8",pfg:"#2a0a18",muted:"#291b24",mfg:"#c0a6b4",accent:"#3a2231",border:"#3d2935",nav:"#150d12",chart:["#f07ba8","#5fd0b0","#f5c26b","#a99bff","#6cc2f0"],warn:"#f5c26b",danger:"#ff8f8f",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#237a55,#26805a)",warn_bar:"linear-gradient(to right,#a05a0a,#a55f0b)",danger_bar:"linear-gradient(to right,#bb2f45,#c4344a)",dot_on:"linear-gradient(#4fd6a0,#2bb383)",dot_off:"linear-gradient(#f0607a,#d94461)",field:"#1a1117",btn:"#c2306f",ok_text:"#4fd6a0"},
        light:Tone{bg:"#fcf5f8",fg:"#2b1621",card:"#ffffff",pop:"#ffffff",primary:"#c2306f",pfg:"#fff",muted:"#fdf8fa",mfg:"#86596f",accent:"#fae6ef",border:"#f0d9e4",nav:"#f9e8f0",chart:["#c2306f","#1a9a7a","#d4930f","#6e56cf","#2a86c7"],warn:"#8a5a00",danger:"#b3263a",track:"#f6edf1",bar_text:"#000",ok:"linear-gradient(#6fdcb0,#4ccb98)",warn_bar:"linear-gradient(#f9d36b,#f4bf3a)",danger_bar:"linear-gradient(#f68b9b,#ee6680)",dot_on:"linear-gradient(#4ccb98,#1fae7a)",dot_off:"linear-gradient(#ee6680,#e04a64)",field:"#fff",btn:"#c2306f",ok_text:"#138060"}},
    Palette{key:"forest",name:"森",
        dark:Tone{bg:"#0e1612",fg:"#e6f0ea",card:"#131e18",pop:"#18261f",primary:"#5fd39a",pfg:"#062214",muted:"#17241d",mfg:"#9db3a6",accent:"#1d2f25",border:"#24362c",nav:"#0a110d",chart:["#5fd39a","#e6c35c","#6cb7e8","#e889a8","#b8a3f0"],warn:"#e6c35c",danger:"#ff8a7a",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#22764f,#267f55)",warn_bar:"linear-gradient(to right,#8e5c06,#946006)",danger_bar:"linear-gradient(to right,#b5342f,#bf3a35)",dot_on:"linear-gradient(#5fd39a,#3bb87c)",dot_off:"linear-gradient(#ef6a5e,#d94f43)",field:"#0e1612",btn:"#23794f",ok_text:"#5fd39a"},
        light:Tone{bg:"#f1f6f2",fg:"#14261b",card:"#ffffff",pop:"#ffffff",primary:"#1f7a4c",pfg:"#fff",muted:"#f6faf7",mfg:"#587063",accent:"#e2f0e6",border:"#d3e3d8",nav:"#e3efe6",chart:["#1f7a4c","#b8860b","#2b7bb9","#c0507a","#6c55c7"],warn:"#8a5a00",danger:"#b3352c",track:"#e9f1eb",bar_text:"#000",ok:"linear-gradient(#7ad9a6,#55c98b)",warn_bar:"linear-gradient(#f7d36b,#f0bd3a)",danger_bar:"linear-gradient(#f39283,#ec6f5f)",dot_on:"linear-gradient(#55c98b,#2fa86b)",dot_off:"linear-gradient(#ec6f5f,#d9534f)",field:"#fff",btn:"#1f7a4c",ok_text:"#1f7a4c"}},
    Palette{key:"tokyo",name:"靛",
        dark:Tone{bg:"#16161e",fg:"#c0caf5",card:"#1a1b26",pop:"#1f2335",primary:"#7aa2f7",pfg:"#10131f",muted:"#1f2133",mfg:"#9aa5ce",accent:"#283050",border:"#2a2e42",nav:"#13131a",chart:["#7aa2f7","#9ece6a","#e0af68","#f7768e","#bb9af7"],warn:"#e0af68",danger:"#f7768e",track:"#24283b",bar_text:"#fff",ok:"linear-gradient(to right,#2e7d55,#2e7d55)",warn_bar:"linear-gradient(to right,#9a5f0a,#9a5f0a)",danger_bar:"linear-gradient(to right,#c0384e,#c0384e)",dot_on:"linear-gradient(#9ece6a,#73b14a)",dot_off:"linear-gradient(#f7768e,#e0526c)",field:"#13131a",btn:"#3d59a1",ok_text:"#9ece6a"},
        light:Tone{bg:"#eef0f7",fg:"#1d2340",card:"#ffffff",pop:"#ffffff",primary:"#2e5bd6",pfg:"#fff",muted:"#f6f7fb",mfg:"#5a6385",accent:"#e4e9f8",border:"#d5dae9",nav:"#e1e5f2",chart:["#2e5bd6","#4f8a2a","#b5760c","#c43b5c","#7a4fd0"],warn:"#8a5a00",danger:"#b8303f",track:"#eceef5",bar_text:"#000",ok:"linear-gradient(#86d47a,#62c457)",warn_bar:"linear-gradient(#f7d36b,#f0bd3a)",danger_bar:"linear-gradient(#f58ca0,#ee6a84)",dot_on:"linear-gradient(#62c457,#3fa53a)",dot_off:"linear-gradient(#ee6a84,#d94b68)",field:"#fff",btn:"#2e5bd6",ok_text:"#3a7d1f"}},
    Palette{key:"coral",name:"珊瑚",
        dark:Tone{bg:"#101624",fg:"#edf0f7",card:"#151d2e",pop:"#1a2438",primary:"#ff8a6b",pfg:"#2a0e05",muted:"#18213a",mfg:"#a3adc6",accent:"#1f2a44",border:"#273350",nav:"#0c111d",chart:["#ff8a6b","#5fd3c4","#ffd166","#a79bff","#73b7ff"],warn:"#ffd166",danger:"#ff7b8a",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#1f7a62,#21806a)",warn_bar:"linear-gradient(to right,#9c5a08,#a35f0a)",danger_bar:"linear-gradient(to right,#bb3246,#c4384b)",dot_on:"linear-gradient(#5fd3c4,#30b3a3)",dot_off:"linear-gradient(#ff6b7d,#e5485e)",field:"#0c111d",btn:"#c4502f",ok_text:"#5fd3c4"},
        light:Tone{bg:"#fbf5f3",fg:"#2a1a15",card:"#ffffff",pop:"#ffffff",primary:"#c2410c",pfg:"#fff",muted:"#fdf9f8",mfg:"#7b5f57",accent:"#fde9e2",border:"#f1dcd5",nav:"#fde8e1",chart:["#c2410c","#138a7c","#b8860b","#6a55c9","#2b78c2"],warn:"#8a5a00",danger:"#b3263a",track:"#f5edea",bar_text:"#000",ok:"linear-gradient(#74dcca,#4fcbb6)",warn_bar:"linear-gradient(#f9d36b,#f4bf3a)",danger_bar:"linear-gradient(#f68b9b,#ee6680)",dot_on:"linear-gradient(#4fcbb6,#22a892)",dot_off:"linear-gradient(#ee6680,#e04a64)",field:"#fff",btn:"#c2410c",ok_text:"#127a6d"}},
    Palette{key:"matcha",name:"抹茶",
        dark:Tone{bg:"#161813",fg:"#eef0e6",card:"#1d2019",pop:"#23271f",primary:"#b5d56a",pfg:"#1a2208",muted:"#22261d",mfg:"#b0b59f",accent:"#2b3124",border:"#343a2d",nav:"#11130f",chart:["#b5d56a","#6fc3b2","#f0b866","#e58fa8","#92a8f0"],warn:"#f0c866",danger:"#ff8f7a",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#4a7720,#4f7d24)",warn_bar:"linear-gradient(to right,#9a5f0a,#a1640a)",danger_bar:"linear-gradient(to right,#b23a31,#bb3f36)",dot_on:"linear-gradient(#b5d56a,#8fb84a)",dot_off:"linear-gradient(#ef6a5e,#d94f43)",field:"#11130f",btn:"#5a7d1e",ok_text:"#b5d56a"},
        light:Tone{bg:"#f5f6ef",fg:"#22261a",card:"#fffffb",pop:"#ffffff",primary:"#5a7314",pfg:"#fff",muted:"#fafbf5",mfg:"#6a6f58",accent:"#eef1de",border:"#dfe3cc",nav:"#ebeedd",chart:["#6b8a15","#1f8a78","#c08010","#b84e6e","#4a62c4"],warn:"#8a5a00",danger:"#b03a2e",track:"#eff1e6",bar_text:"#000",ok:"linear-gradient(#b7dd7a,#9acd5a)",warn_bar:"linear-gradient(#f7d36b,#f0bd3a)",danger_bar:"linear-gradient(#f0907f,#e8705c)",dot_on:"linear-gradient(#9acd5a,#6fa834)",dot_off:"linear-gradient(#e8705c,#d9534f)",field:"#fff",btn:"#5a7314",ok_text:"#4a6a10"}},
    Palette{key:"wine",name:"酒红",
        dark:Tone{bg:"#1a0f12",fg:"#f6e9eb",card:"#231418",pop:"#2c1a1f",primary:"#e8bb64",pfg:"#2a1a04",muted:"#2a181d",mfg:"#c6a8ad",accent:"#3a2026",border:"#432a30",nav:"#14090c",chart:["#e8bb64","#ef7a8c","#6fc3b2","#a99bff","#7ab8f0"],warn:"#ffd27a",danger:"#ff8a8a",track:"rgb(255 255 255/7%)",bar_text:"#fff",ok:"linear-gradient(to right,#237a55,#26805a)",warn_bar:"linear-gradient(to right,#a05a0a,#a55f0b)",danger_bar:"linear-gradient(to right,#b52f3c,#c0353f)",dot_on:"linear-gradient(#5fd3a0,#30b380)",dot_off:"linear-gradient(#f0607a,#d94461)",field:"#14090c",btn:"#8f2338",ok_text:"#5fd3a0"},
        light:Tone{bg:"#faf4f4",fg:"#2b1519",card:"#ffffff",pop:"#ffffff",primary:"#8f2338",pfg:"#fff",muted:"#fcf8f8",mfg:"#7d5a60",accent:"#f5e4e6",border:"#ecd8db",nav:"#f3e1e3",chart:["#8f2338","#b8860b","#1a8a7a","#5f55c9","#2b78c2"],warn:"#8a5a00",danger:"#b3263a",track:"#f4eced",bar_text:"#000",ok:"linear-gradient(#6fdcb0,#4ccb98)",warn_bar:"linear-gradient(#f9d36b,#f4bf3a)",danger_bar:"linear-gradient(#f68b9b,#ee6680)",dot_on:"linear-gradient(#4ccb98,#1fae7a)",dot_off:"linear-gradient(#ee6680,#e04a64)",field:"#fff",btn:"#8f2338",ok_text:"#138060"}},
    Palette{key:"nord",name:"霜",
        dark:Tone{bg:"#2e3440",fg:"#eceff4",card:"#3b4252",pop:"#434c5e",primary:"#88c0d0",pfg:"#1f2530",muted:"#414a5b",mfg:"#b8c0cf",accent:"#4a5468",border:"#4c566a",nav:"#272c36",chart:["#88c0d0","#a3be8c","#ebcb8b","#b48ead","#81a1c1"],warn:"#ebcb8b",danger:"#f2a7ad",track:"rgb(255 255 255/8%)",bar_text:"#fff",ok:"linear-gradient(to right,#4f7a3e,#4f7a3e)",warn_bar:"linear-gradient(to right,#9a6208,#9a6208)",danger_bar:"linear-gradient(to right,#a8414b,#a8414b)",dot_on:"linear-gradient(#a3be8c,#8fae74)",dot_off:"linear-gradient(#d0747c,#bf616a)",field:"#2e3440",btn:"#4c6a94",ok_text:"#a3be8c"},
        light:Tone{bg:"#eceff4",fg:"#2e3440",card:"#ffffff",pop:"#ffffff",primary:"#4c6a94",pfg:"#fff",muted:"#f5f7fa",mfg:"#5b6578",accent:"#e5e9f0",border:"#d8dee9",nav:"#e5e9f0",chart:["#5e81ac","#6f8f52","#b8860b","#93678c","#3f8fa3"],warn:"#8a5a00",danger:"#a8434c",track:"#eef1f5",bar_text:"#000",ok:"linear-gradient(#b4d19b,#a3be8c)",warn_bar:"linear-gradient(#f0d49b,#ebcb8b)",danger_bar:"linear-gradient(#d8848b,#bf616a)",dot_on:"linear-gradient(#a3be8c,#7f9f66)",dot_off:"linear-gradient(#d8848b,#bf616a)",field:"#fff",btn:"#4c6a94",ok_text:"#4f7a3e"}},
];

/// 当前配色的键。每次发页面都要用，放内存里，保存时同步刷新。
static CURRENT:RwLock<String>=RwLock::new(String::new());

pub(super) fn init(conn:&Connection)->rusqlite::Result<()> {
    let stored:Option<String>=conn
        .query_row("SELECT value FROM admin_settings WHERE key='palette'",[],|r|r.get(0))
        .optional()?;
    remember(stored.unwrap_or_default());
    Ok(())
}

fn remember(key:String) {
    if let Ok(mut current)=CURRENT.write() {
        *current=key;
    }
}

fn find(key:&str)->Option<&'static Palette> {
    PALETTES.iter().find(|p|p.key==key)
}

/// 当前这套。库里存的键不认识（比如以后删了某套）就按原版。
pub(super) fn current()->&'static Palette {
    let key=CURRENT.read().map(|k|k.clone()).unwrap_or_default();
    find(&key).unwrap_or(&PALETTES[0])
}

/// 换配色。不认识的键返回 false，什么都不改。
pub(super) fn save(conn:&Connection,key:&str)->rusqlite::Result<bool> {
    if find(key).is_none() {return Ok(false);}
    conn.execute(
        "INSERT INTO admin_settings(key,value) VALUES('palette',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key],
    )?;
    remember(key.to_string());
    Ok(true)
}

/// 手机状态栏和浏览器边框的颜色（顶栏的 --nav），(暗, 亮)。index.html 和 App.tsx 从 html 上的 data-nav-* 读。
pub(super) fn nav(p:&Palette)->(&'static str,&'static str) {(p.dark.nav,p.light.nav)}
/// 「添加到主屏幕」的启动底色。
pub(super) fn background(p:&Palette)->&'static str {p.dark.bg}

/// 后台卡片里的色条：暗色的底、卡片、主色、进度条绿。
pub(super) fn swatch(p:&Palette)->String {
    let t=&p.dark;
    [t.bg,t.card,t.primary,t.ok].iter().map(|c|format!("<i style=\"background:{c}\"></i>")).collect()
}

fn public_vars(t:&Tone,dark:bool)->String {
    let input=if dark {"rgb(255 255 255 / 15%)"} else {"color-mix(in oklab, var(--border), black 10%)"};
    format!("--background:{};--foreground:{};--card:{};--card-foreground:{};--popover:{};--popover-foreground:{};--primary:{};--primary-foreground:{};--secondary:{};--secondary-foreground:{};--muted:{};--muted-foreground:{};--accent:{};--accent-foreground:{};--destructive:{};--border:{};--input:{};--ring:{};--nav:{};--chart-1:{};--chart-2:{};--chart-3:{};--chart-4:{};--chart-5:{};--warn:{};--danger:{};--bar-track:{};--bar-text:{};--bar-ok:{};--bar-warn:{};--bar-danger:{};--dot-online:{};--dot-offline:{}",
        t.bg,t.fg,t.card,t.fg,t.pop,t.fg,t.primary,t.pfg,t.muted,t.fg,t.muted,t.mfg,t.accent,t.fg,t.danger,t.border,input,t.primary,t.nav,
        t.chart[0],t.chart[1],t.chart[2],t.chart[3],t.chart[4],t.warn,t.danger,t.track,t.bar_text,t.ok,t.warn_bar,t.danger_bar,t.dot_on,t.dot_off)
}

/// 公开页的 <style>，原版是空的。选择器写成 html:root / html.dark，比 index.css 里的
/// :root / .dark 多一个元素，不管谁先加载都是这里赢。
pub(super) fn public_style(p:&Palette)->String {
    if p.key==DEFAULT {return String::new();}
    format!("<style>html:root{{{}}}html.dark{{{}}}</style>",public_vars(&p.light,false),public_vars(&p.dark,true))
}

fn admin_vars(t:&Tone)->String {
    format!("--bg:{};--text:{};--head:{};--line:{};--line-soft:color-mix(in oklab,{} 65%,{});--card:{};--field:{};--field-line:color-mix(in oklab,{},{} 14%);--label:{};--link:{};--btn:{};--btn2:{};--btn2-text:{};--danger:{};--danger-line:color-mix(in oklab,{} 35%,{});--muted:{};--ok:{};--down:{};--code:{}",
        t.bg,t.fg,t.nav,t.border,t.border,t.card,t.card,t.field,t.border,t.fg,t.mfg,t.primary,t.btn,t.accent,t.fg,t.danger,t.danger,t.card,t.mfg,t.ok_text,t.danger,t.muted)
}

/// 接在 ADMIN_CSS 后面的颜色，原版是空的。和 ADMIN_CSS 一样暗色写两遍：跟随系统一遍、手动选黑夜一遍；
/// 选择器和 ADMIN_CSS 里的一样，放在后面所以覆盖它。
pub(super) fn admin_css(p:&Palette)->String {
    if p.key==DEFAULT {return String::new();}
    let dark=admin_vars(&p.dark);
    format!(":root{{{}}}@media (prefers-color-scheme:dark){{:root:not([data-theme=\"light\"]){{{dark}}}}}:root[data-theme=\"dark\"]{{{dark}}}",admin_vars(&p.light))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_the_default_comes_first() {
        assert_eq!(PALETTES[0].key,DEFAULT);
        for (i,p) in PALETTES.iter().enumerate() {
            assert!(PALETTES.iter().skip(i+1).all(|q|q.key!=p.key),"重复的键 {}",p.key);
            assert!(p.key.bytes().all(|b|b.is_ascii_lowercase()),"键要能直接放进 HTML");
        }
    }

    #[test]
    fn saves_only_known_palettes() {
        let conn=Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE admin_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);").unwrap();
        init(&conn).unwrap();
        assert_eq!(current().key,DEFAULT,"没选过就是原版");
        assert!(!save(&conn,"../x").unwrap());
        assert!(save(&conn,"komari").unwrap());
        assert_eq!(current().key,"komari");
        remember(String::new());
        init(&conn).unwrap();
        assert_eq!(current().key,"komari","重启后还是选的那套");
        remember("gone".to_string());
        assert_eq!(current().key,DEFAULT,"库里的键不认识就按原版");
        remember(String::new());
    }

    #[test]
    fn original_adds_nothing_others_cover_both_dark_paths() {
        assert!(public_style(&PALETTES[0]).is_empty());
        assert!(admin_css(&PALETTES[0]).is_empty());
        for p in PALETTES.iter().skip(1) {
            let css=admin_css(p);
            assert!(css.contains("prefers-color-scheme:dark")&&css.contains(":root[data-theme=\"dark\"]{--bg:"),"{}",p.key);
            let style=public_style(p);
            assert!(style.starts_with("<style>html:root{--background:")&&style.contains("html.dark{--background:")&&style.ends_with("}</style>"),"{}",p.key);
            assert!(!style.contains("</style><")&&style.matches('{').count()==style.matches('}').count());
        }
    }
}
