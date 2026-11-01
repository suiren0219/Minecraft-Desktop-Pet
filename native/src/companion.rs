//! 陪伴大脑（进程内版）：屏幕上下文 → AstrBot LLM → 气泡事件。
//! 取代外部 python：上下文采集、触发器、LLM 调用全部在本进程完成。
//!
//! 配置：数据目录下 companion.json {"base_url":"http://host:6185","key":"abk_..."}
//! 缺文件 = 陪伴功能关闭（不影响桌宠本体）。

use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
};

use crate::bridge;
use crate::wide;

const SESSION: &str = "xiaoya-companion";
const USER: &str = "xiaoya-pet";
const MIN_GAP: f64 = 300.;          // 两次 LLM 调用最小间隔
const APP_DEBOUNCE: f64 = 600.;     // 同一应用 10 分钟内只问候一次
const IDLE_SOFT: f64 = 25. * 60.;
const IDLE_HARD: f64 = 60. * 60.;
const AMBIENT_MIN: f64 = 25. * 60.;
const AMBIENT_MAX: f64 = 55. * 60.;

static CONFIG: Mutex<Option<(String, String)>> = Mutex::new(None); // (base_url, key)
static DATA_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
static LAST_EVAL: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct State {
    last_app: String,
    last_app_at: f64,
    last_llm: f64,
    idle_soft_done: bool,
    idle_hard_done: bool,
    next_ambient: f64,
}
static STATE: Mutex<Option<State>> = Mutex::new(None);

pub fn init(dir: &Path) {
    *DATA_DIR.lock().unwrap() = Some(dir.to_path_buf());
    let cfg = dir.join("companion.json");
    match std::fs::read_to_string(&cfg) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                let base = v.get("base_url").and_then(|x| x.as_str()).unwrap_or("").trim_end_matches('/').to_string();
                let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if base.is_empty() || key.is_empty() {
                    bridge::log("companion: companion.json 字段不全，陪伴关闭");
                    return;
                }
                let host_for_feed = base
                    .strip_prefix("http://")
                    .unwrap_or(&base)
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                *CONFIG.lock().unwrap() = Some((base, key));
                // 开场白：启动 15 秒后主动说第一句
                state()
                    .get_or_insert(State {
                        last_app: String::new(),
                        last_app_at: 0.,
                        last_llm: 0.,
                        idle_soft_done: false,
                        idle_hard_done: false,
                        next_ambient: epoch() + 15.,
                    })
                    .next_ambient = epoch() + 15.;
                bridge::log("companion: 已启用（companion.json）");
                if v.get("feed").and_then(|x| x.as_bool()).unwrap_or(true) {
                    let port = v.get("feed_port").and_then(|x| x.as_u64()).unwrap_or(6186);
                    let host = host_for_feed.clone();
                    let feed_base = format!("http://{host}:{port}");
                    let key = v
                        .get("feed_key")
                        .and_then(|x| x.as_str())
                        .unwrap_or("xiaoya-feed-6K2M")
                        .to_string();
                    bridge::log(&format!("companion: 信息流轮询 {feed_base}"));
                    let feed_base2 = feed_base.clone();
                    let key2 = key.clone();
                    std::thread::spawn(move || feed_loop(feed_base2, key2));
                }
            }
            Err(e) => bridge::log(&format!("companion: companion.json 解析失败 {e}")),
        },
        Err(_) => bridge::log("companion: 无 companion.json，陪伴关闭"),
    }
}

fn enabled() -> bool {
    CONFIG.lock().unwrap().is_some()
}

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap()
}

fn epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.)
}

fn now_secs() -> String {
    unsafe {
        #[repr(C)]
        struct ST { wYear: u16, wMonth: u16, wDayOfWeek: u16, wDay: u16,
                    wHour: u16, wMinute: u16, wSecond: u16, wMillis: u16 }
        #[link(name = "kernel32")]
        extern "system" { fn GetLocalTime(out: *mut ST); }
        let mut st = ST { wYear:0, wMonth:0, wDayOfWeek:0, wDay:0, wHour:0, wMinute:0, wSecond:0, wMillis:0 };
        unsafe { GetLocalTime(&mut st); }
        format!("{:02}:{:02}", st.wHour, st.wMinute)
    }
}

fn foreground() -> (String, String) {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return (String::new(), String::new());
        }
        let mut pid = 0u32;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, &mut pid);
        let mut name = String::new();
        if pid != 0 {
            let h = windows_sys::Win32::System::Threading::OpenProcess(0x1000, 0, pid);
            if !h.is_null() {
                let mut buf = [0u16; 260];
                let mut size = 260u32;
                if windows_sys::Win32::System::Threading::QueryFullProcessImageNameW(
                    h, 0, buf.as_mut_ptr(), &mut size,
                ) != 0
                {
                    let full = String::from_utf16_lossy(&buf[..size as usize]);
                    name = full.rsplit('\\').next().unwrap_or("").to_string();
                }
                windows_sys::Win32::Foundation::CloseHandle(h);
            }
        }
        let len = GetWindowTextLengthW(hwnd);
        let title = if len > 0 {
            let mut buf = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        };
        (name, title)
    }
}

fn idle_seconds() -> f64 {
    unsafe {
        let mut lii: LASTINPUTINFO = std::mem::zeroed();
        lii.cbSize = std::mem::size_of::<LASTINPUTINFO>() as u32;
        if GetLastInputInfo(&mut lii) != 0 {
            let tick = windows_sys::Win32::System::SystemInformation::GetTickCount64();
            return (tick - lii.dwTime as u64) as f64 / 1000.;
        }
        0.
    }
}

const APP_NAMES: &[(&str, &str)] = &[
    ("Code.exe", "写代码"),
    ("chrome.exe", "浏览器"),
    ("msedge.exe", "浏览器"),
    ("QQ.exe", "QQ"),
    ("Bip.exe", "B站直播"),
    ("bsp.exe", "B站直播"),
];

fn app_label(proc: &str) -> String {
    APP_NAMES
        .iter()
        .find(|(k, _)| *k == proc)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| if proc.is_empty() { "未知应用".into() } else { proc.to_string() })
}

fn cfg_flag(key: &str, default: bool) -> bool {
    let dir = DATA_DIR.lock().unwrap().clone();
    let Some(dir) = dir else { return default; };
    let Ok(text) = std::fs::read_to_string(dir.join("companion.json")) else { return default; };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return default; };
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

/// 气泡视觉配置：木板材质 + 文字颜色。每次冒气泡时读一次，改配置即生效。
pub fn visual() -> (String, [u8; 3]) {
    let mut wood = "oak".to_string();
    let mut ink = [47u8, 47, 47];
    let dir = DATA_DIR.lock().unwrap().clone();
    if let Some(dir) = dir {
        if let Ok(text) = std::fs::read_to_string(dir.join("companion.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(w) = v.get("wood").and_then(|x| x.as_str()) {
                    wood = w.to_string();
                }
                match v.get("text_color") {
                    Some(serde_json::Value::Array(a)) if a.len() == 3 => {
                        ink = [
                            a[0].as_u64().unwrap_or(47) as u8,
                            a[1].as_u64().unwrap_or(47) as u8,
                            a[2].as_u64().unwrap_or(47) as u8,
                        ];
                    }
                    Some(serde_json::Value::String(s)) => {
                        let hex = s.trim_start_matches('#');
                        if hex.len() == 6 {
                            if let Ok(v) = u32::from_str_radix(hex, 16) {
                                ink = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    (wood, ink)
}

fn tts_speak(text: &str) {
    if !cfg_flag("tts", true) || text.is_empty() {
        return;
    }
    let text = text.to_string();
    std::thread::spawn(move || {
        let tmp = std::env::temp_dir();
        let txt = tmp.join("xiaoya-tts.txt");
        let wav = tmp.join("xiaoya-tts.wav");
        if std::fs::write(&txt, &text).is_err() {
            return;
        }
        let ps = format!(
            "Add-Type -AssemblyName System.Speech; $s=New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}'); $t=Get-Content -Raw -Encoding UTF8 '{}'; $s.Speak($t); $s.Dispose()",
            wav.display(),
            txt.display()
        );
        match std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
        {
            Ok(o) if o.status.success() => {
                bridge::log("tts: wav synthesized");
                crate::sound::play_file(wav.to_str().unwrap_or(""));
            }
            _ => bridge::log("tts: synthesis failed"),
        }
    });
}

fn read_clipboard() -> Option<String> {
    unsafe {
        use windows_sys::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(13); // CF_UNICODETEXT
        let out = if h.is_null() {
            None
        } else {
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                None
            } else {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
            }
        };
        if !h.is_null() {
            GlobalUnlock(h);
        }
        CloseClipboard();
        out
    }
}

/// 主循环每帧调用；内部 5 秒评估一次，触发即起线程调 LLM。
pub fn tick() {
    if !enabled() || BUSY.load(Ordering::Relaxed) {
        return;
    }
    let mut last_eval = LAST_EVAL.lock().unwrap();
    if let Some(t) = *last_eval {
        if t.elapsed() < Duration::from_secs(5) {
            return;
        }
    }
    *last_eval = Some(Instant::now());
    drop(last_eval);

    let hhmm = now_secs();
    let (proc, title) = foreground();
    let idle = idle_seconds();
    let label = app_label(&proc);

    let mut st_guard = state();
    let st = st_guard.get_or_insert(State {
        last_app: String::new(),
        last_app_at: 0.,
        last_llm: 0.,
        idle_soft_done: false,
        idle_hard_done: false,
        next_ambient: epoch() + 90.,
    });
    let now = epoch();
    let mut prompt: Option<String> = None;

    if !proc.is_empty() && proc != st.last_app && now - st.last_app_at > APP_DEBOUNCE && now - st.last_llm > MIN_GAP {
        st.last_app = proc.clone();
        st.last_app_at = now;
        prompt = Some(format!("现在 {hhmm}，用户刚切到「{label}」（窗口：{}）", title.chars().take(40).collect::<String>()));
    } else if idle > IDLE_HARD && !st.idle_hard_done {
        st.idle_hard_done = true;
        prompt = Some(format!("现在 {hhmm}，用户已经发呆 {} 分钟没动电脑了", idle as i64 / 60));
    } else if idle > IDLE_SOFT && !st.idle_soft_done {
        st.idle_soft_done = true;
        prompt = Some(format!("现在 {hhmm}，用户发呆 {} 分钟了，一直没动", idle as i64 / 60));
    } else if now >= st.next_ambient {
        st.next_ambient = now + AMBIENT_MIN + rand_frac() * (AMBIENT_MAX - AMBIENT_MIN);
        prompt = Some(format!("现在 {hhmm}，用户前台是「{label}」，主动找句话说点应景的陪伴"));
    } else if idle < IDLE_SOFT * 0.5 {
        st.idle_soft_done = false;
        st.idle_hard_done = false;
    }
    drop(st);

    if let Some(ctx) = prompt {
        if BUSY.swap(true, Ordering::Relaxed) {
            return;
        }
        std::thread::spawn(move || {
            let reply = match call_llm(&format!(
                "【屏幕上下文】{ctx}\n【要求】说一句陪伴话。"
            )) {
                Ok(r) => clean_reply(&r),
                Err(e) => {
                    bridge::log(&format!("companion: LLM 失败 {e}"));
                    String::new()
                }
            };
            if !reply.is_empty() {
                bridge::log(&format!("companion: 小亚 {:?}", reply));
                if let Some(s) = state().as_mut() {
                    s.last_llm = epoch();
                }
                bridge::push_event("say".into(), reply.clone(), 0.);
                tts_speak(&reply);
            }
            BUSY.store(false, Ordering::Relaxed);
        });
    }
}

/// 双击桌宠：思考反馈 →（可选）读剪贴板作为用户的话 → LLM → 气泡 + TTS。
pub fn poke() {
    if !enabled() || BUSY.load(Ordering::Relaxed) {
        return;
    }
    BUSY.store(true, Ordering::Relaxed);
    bridge::push_event("say".into(), "嗯—让我想想…".into(), 20.);
    let (proc, _title) = foreground();
    let label = app_label(&proc);
    let clip = if cfg_flag("clipboard_chat", true) { read_clipboard() } else { None };
    let mut last = LAST_CLIP.lock().unwrap();
    let user_said = clip.filter(|t| {
        !t.trim().is_empty() && t.trim().len() <= 500 && Some(t.trim().to_string()) != *last
    });
    if let Some(t) = &user_said {
        *last = Some(t.trim().to_string());
    }
    drop(last);
    std::thread::spawn(move || {
        let prompt = match &user_said {
            Some(t) => format!(
                "现在 {}，用户双击了你，并复制了一句话让你回应：\"{}\"——像朋友一样简短回应（25字内），可以调侃或关心",
                now_secs(),
                t.chars().take(200).collect::<String>()
            ),
            None => format!(
                "现在 {}，用户双击戳了戳你（前台：{label}），主动说句简短的陪伴话回应这戳",
                now_secs()
            ),
        };
        let reply = match call_llm(&prompt) {
            Ok(r) => clean_reply(&r),
            Err(e) => {
                bridge::log(&format!("companion: poke LLM 失败 {e}"));
                String::new()
            }
        };
        if !reply.is_empty() {
            bridge::log(&format!("companion: poke 小亚 {:?}", reply));
            bridge::push_event("say".into(), reply.clone(), 0.);
            tts_speak(&reply);
        }
        BUSY.store(false, Ordering::Relaxed);
    });
}

static LAST_CLIP: Mutex<Option<String>> = Mutex::new(None);

fn rand_frac() -> f64 {
    let x = epoch() * 1000.0;
    (x % 1000.0) / 1000.0
}

// ---------------- HTTP（WinHTTP，与 online.rs 同款风格） ----------------

fn http_post(url_base: &str, path: &str, body: &str, key: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let rest = url_base
        .strip_prefix("http://")
        .ok_or("companion: 目前只支持 http:// 的 base_url")?;
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "端口解析失败")?),
        None => (rest.to_string(), 80u16),
    };
    let mut stream = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("连接服务器失败 {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .map_err(|e| e.to_string())?;
    let crlf = "\r\n";
    let crlf = "\r\n";
    let req = format!("POST {path} HTTP/1.1{crlf}Host: {host}:{port}{crlf}Authorization: Bearer {key}{crlf}Content-Type: application/json{crlf}Content-Length: {clen}{crlf}Connection: close{crlf}{crlf}{body}", clen = body.len());
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().ok();
    let mut raw = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break,
        }
        if raw.len() > 1 << 20 {
            break;
        }
    }
    // 剥 HTTP 响应头
    Ok(match raw.find("\r\n\r\n") {
        Some(i) => raw[i + 4..].to_string(),
        None => raw,
    })
}

fn http_get(url_base: &str, path_qs: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let rest = url_base
        .strip_prefix("http://")
        .ok_or("companion: 目前只支持 http:// 的 feed_url")?;
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "端口解析失败")?),
        None => (rest.to_string(), 80u16),
    };
    let mut stream = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("feed 连接失败 {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .map_err(|e| e.to_string())?;
    let crlf = "\r\n";
    let req = format!("GET {path_qs} HTTP/1.1{crlf}Host: {host}:{port}{crlf}Connection: close{crlf}{crlf}");
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = String::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break,
        }
        if raw.len() > 1 << 20 {
            break;
        }
    }
    Ok(match raw.find("



") {
        Some(i) => raw[i + 4..].to_string(),
        None => raw,
    })
}

fn call_llm(prompt: &str) -> Result<String, String> {
    let (base, key) = CONFIG
        .lock()
        .unwrap()
        .clone()
        .ok_or("companion 未配置")?;
    let body = json!({
        "message": prompt,
        "session_id": SESSION,
        "username": USER,
        "enable_streaming": false,
    })
    .to_string();
    let raw = http_post(&base, "/api/v1/chat", &body, &key)?;
    // 两种返回：一次性 JSON 或 SSE 流
    if raw.trim_start().starts_with('{') {
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("chat 响应解析失败 {e}"))?;
        let data = v.get("data");
        for k in ["reply", "text", "message", "result"] {
            if let Some(s) = data.and_then(|d| d.get(k)).and_then(|x| x.as_str()) {
                if !s.trim().is_empty() {
                    return Ok(s.to_string());
                }
            }
        }
        Ok(String::new())
    } else {
        // SSE：收集 plain 且非 reasoning 的分片
        let mut reply = String::new();
        for line in raw.lines() {
            let line = line.trim();
            if let Some(payload) = line.strip_prefix("data:") {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(payload.trim()) {
                    if obj.get("type").and_then(|t| t.as_str()) == Some("plain")
                        && obj.get("chain_type").and_then(|t| t.as_str()) != Some("reasoning")
                    {
                        reply.push_str(obj.get("data").and_then(|d| d.as_str()).unwrap_or(""));
                    }
                }
            }
        }
        Ok(reply)
    }
}

/// 剥思考过程 / 表情标签 / 分隔线，只留最后那句要念的话。
fn strip_tag_block(text: &str, open_prefix: &str, close_prefix: &str) -> String {
    let mut out = text.to_string();
    loop {
        let Some(i) = out.find(open_prefix) else { break; };
        let Some(j) = out[i..].find(close_prefix) else { break; };
        // 闭合标签可能带 id（</think:xxx>），吃到它后面的 '>'
        let close_abs = i + j;
        let Some(gt) = out[close_abs..].find('>') else { break; };
        let end = close_abs + gt + 1;
        out.replace_range(i..end, "");
    }
    out
}

fn clean_reply(text: &str) -> String {
    let mut t = text.to_string();
    // 思考标签有三种形态：<think:xxx>…</think:xxx>、<think>…</think>、🤔 思考块
    t = strip_tag_block(&t, "<think", "</think");
    t = strip_tag_block(&t, "<pc_reaction_expression>", "</pc_reaction_expression>");
    if let Some(i) = t.find("<pc_reaction_expression>") {
        if let Some(j) = t[i..].find("</pc_reaction_expression>") {
            let end = i + j + "</pc_reaction_expression>".len();
            t.replace_range(i..end, "");
        }
    }
    if let Some(i) = t.find("🤔") {
        // 思考块以分隔线结束；取最后一段
        let tail = &t[i..];
        let seg = tail.rsplit("────").next().unwrap_or(tail);
        t = seg.to_string();
    }
    let lines: Vec<&str> = t
        .lines()
        .map(|l| l.trim().trim_matches(['─', '-', ' ']))
        .filter(|l| !l.is_empty() && *l != "────")
        .collect();
    lines.last().unwrap_or(&"").to_string()
}

/// 信息流轮询：每 2.5s 拉一次增量消息，转成气泡事件。
fn feed_loop(feed_base: String, key: String) {
    std::thread::sleep(Duration::from_secs(3));
    loop {
        let since = LAST_SEQ.load(Ordering::Relaxed);
        let path_qs = format!("/events?key={key}&since={since}");
        match http_get(&feed_base, &path_qs) {
            Ok(body) => {
                // 非 JSON 响应可见化（插件未装/端口被 nginx 占用等），每 100s 记一条防刷屏
                if !body.trim_start().starts_with('{') {
                    static BAD: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let n = BAD.fetch_add(1, Ordering::Relaxed);
                    if n % 40 == 0 {
                        bridge::log(&format!(
                            "feed: 非 JSON 响应（插件未安装/端口被占用？）前80字: {}",
                            body.chars().take(80).collect::<String>()
                        ));
                    }
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(list) = v.get("events").and_then(|e| e.as_array()) {
                        for e in list {
                            let seq = e.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
                            if seq <= since {
                                continue;
                            }
                            let sender = e.get("sender").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let text = e.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let text: String = text.chars().take(100).collect();
                            if text.is_empty() {
                                continue;
                            }
                            LAST_SEQ.store(seq, Ordering::Relaxed);
                            let shown = if sender.is_empty() {
                                text
                            } else {
                                format!("{sender}: {text}")
                            };
                            bridge::log(&format!("feed: {shown}"));
                            bridge::push_event("msg".into(), shown, 0.);
                        }
                    }
                }
            }
            Err(e) => {
                bridge::log(&format!("feed: {e}"));
            }
        }
        std::thread::sleep(Duration::from_millis(2500));
    }
}
