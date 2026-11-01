//! AstrBot 桥接收口：本地 TCP 监听 127.0.0.1:9528。
//! 外部桥接脚本（astrbot_bridge.py）把 {"kind":"msg"|"reply","text":"..."}
//! POST 进来，这里向桌宠窗口投递 BRIDGE_MSG，主线程据此触发挥手等反应。
//! 之后要加气泡播报，把 text 经静态队列传给主线程即可，协议不用改。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// WM_APP+14：diagnostic 已占用 +12/+13，往前顺延。
pub const BRIDGE_MSG: u32 = WM_APP + 14;

/// 临时诊断日志：追加到 %TEMP%\xiaoya-pet-debug.log，定位气泡链路断点用。
pub fn log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("xiaoya-pet-debug.log");
    // 轮转：超 1MB 归档为 .log.1，防无限增长
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1_000_000 {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.);
        let _ = writeln!(f, "[{:.3}] {}", ts, msg);
    }
}

static EVENTS: OnceLock<Mutex<VecDeque<(String, String, f64)>>> = OnceLock::new();
static PET_HWND: AtomicUsize = AtomicUsize::new(0);

fn events() -> &'static Mutex<VecDeque<(String, String, f64)>> {
    EVENTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 外部/进程内模块投递事件并通知主线程（companion 陪伴语走这里）。
pub fn push_event(kind: String, text: String, secs: f64) {
    events()
        .lock()
        .unwrap()
        .push_back((kind, text, secs));
    let h = PET_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            PostMessageW(h as HWND, BRIDGE_MSG, 0, 0);
        }
    }
}

/// 主线程在 BRIDGE_MSG 里取走全部待处理事件 (kind, text)。
pub fn take_events() -> Vec<(String, String, f64)> {
    let mut q = events().lock().unwrap();
    let out: Vec<(String, String, f64)> = q.drain(..).collect();
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// HWND 是裸指针不满足 Send，跨线程只传 usize。
pub fn spawn(hwnd: HWND) {
    let hwnd_addr = hwnd as usize;
    PET_HWND.store(hwnd_addr, Ordering::Relaxed);
    std::thread::spawn(move || {
        // 只绑回环：联动指令仅接受本机来源。
        let listener = match TcpListener::bind("127.0.0.1:9528") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bridge: 端口 9528 监听失败（可能已有实例在跑）：{e}");
                return;
            }
        };
        for stream in listener.incoming() {
            match stream {
                Ok(mut s) => handle(&mut s, hwnd_addr as HWND),
                Err(_) => continue,
            }
        }
    });
}

fn handle(stream: &mut TcpStream, hwnd: HWND) {
    // HTTP 头和体可能分多个 TCP 段到达：按 Content-Length 读满再解析。
    use std::io::ErrorKind;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(800)));
    let mut data: Vec<u8> = Vec::new();
    loop {
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => break,
            Err(_) => return,
        }
        if let Some(i) = find_subslice(&data, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..i]).to_lowercase();
            let cl: usize = headers
                .find("content-length:")
                .and_then(|p| {
                    headers[p + 15..]
                        .split(|c| c == '\r' || c == '\n')
                        .next()?
                        .trim()
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            if data.len() >= i + 4 + cl {
                break;
            }
        }
    }
    let body = String::from_utf8_lossy(&data);
    // HTTP 客户端发来的是"请求头 + 空行 + 请求体"，取空行之后的纯 JSON；
    // 裸 socket 客户端没有头，则整个内容就是 JSON。
    let json_text = match body.find("\r\n\r\n") {
        Some(i) => &body[i + 4..],
        None => match body.find("\n\n") {
            Some(i) => &body[i + 2..],
            None => body.as_ref(),
        },
    };
    log(&format!(
        "handle: recv {} bytes, json: {}",
        data.len(),
        json_text.chars().take(80).collect::<String>()
    ));
    let mut queued = false;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_text) {
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("msg").to_string();
        let text = v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();
        let secs = v.get("secs").and_then(|s| s.as_f64()).unwrap_or(0.);
        if !text.is_empty() || kind != "say" {
            events().lock().unwrap().push_back((kind, text, secs));
            queued = true;
        }
    } else if body.contains("kind") || body.contains("text") {
        log("handle: JSON 解析失败，按裸事件入队");
        events().lock().unwrap().push_back(("msg".into(), String::new(), 0.));
        queued = true;
    }
    if queued {
        log(&format!("handle: queued events, posting BRIDGE_MSG"));
        unsafe {
            PostMessageW(hwnd, BRIDGE_MSG, 0, 0);
        }
    }
    // 回一个最小 HTTP 响应，让 curl/urllib 之类客户端拿到 200 而不是空回复
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// 双击桌宠：向陪伴大脑（companion.py, 9529）要一句陪伴话。尽力而为，失败静默。
pub fn poke() {
    log("poke: 双击触发，请求陪伴语");
    std::thread::spawn(|| {
        let Ok(mut s) = TcpStream::connect("127.0.0.1:9529") else { return; };
        let req = "POST /poke HTTP/1.1\r\n\
                   Content-Type: application/json\r\n\
                   Content-Length: 2\r\n\
                   Connection: close\r\n\
                   \r\n\
                   {}";
        let _ = s.write_all(req.as_bytes());
        let _ = s.flush();
    });
}
