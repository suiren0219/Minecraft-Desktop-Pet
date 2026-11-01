//! 桌宠头顶气泡：陪伴语显示。
//! 绘制 = 纯 Rust 圆角矩形栅格化 + GDI 文字掩膜（白底黑字，按亮度转覆盖度）。
//! frame.pixels 为 BGRA（第 4 字节 alpha），气泡像素全部 alpha=255，规避预乘问题。

use std::ptr::null_mut;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GetDC, PatBlt, ReleaseDC, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, TRANSPARENT, WHITENESS,
};

// windows-sys 没导出 DrawText 格式常量，按 Win32 文档值手写
const DT_CALCRECT: u32 = 0x400;
const DT_WORDBREAK: u32 = 0x10;
const DT_NOPREFIX: u32 = 0x800;

const TEXT_INK: [u8; 3] = [47, 47, 47]; // 默认告示牌文字：深灰
const BORDER: i32 = 1; // 暗边宽度
const TAIL: i32 = 14; // 尾巴高度

/// Mojang 官方木板材质（client.jar 提取），12 种木头。
const PLANK_SETS: &[(&str, &[u8])] = &[
    ("oak", include_bytes!("../assets/planks/oak.png")),
    ("spruce", include_bytes!("../assets/planks/spruce.png")),
    ("birch", include_bytes!("../assets/planks/birch.png")),
    ("jungle", include_bytes!("../assets/planks/jungle.png")),
    ("acacia", include_bytes!("../assets/planks/acacia.png")),
    ("dark_oak", include_bytes!("../assets/planks/dark_oak.png")),
    ("mangrove", include_bytes!("../assets/planks/mangrove.png")),
    ("cherry", include_bytes!("../assets/planks/cherry.png")),
    ("bamboo", include_bytes!("../assets/planks/bamboo.png")),
    ("crimson", include_bytes!("../assets/planks/crimson.png")),
    ("warped", include_bytes!("../assets/planks/warped.png")),
    ("pale_oak", include_bytes!("../assets/planks/pale_oak.png")),
];

struct PlanksTex {
    w: usize,
    h: usize,
    px: Vec<[u8; 3]>,
}

/// 按名字取木板材质（16×16，压暗 12% 让文字更清晰）；未知名字回退橡木。
fn planks(name: &str) -> &'static PlanksTex {
    static CACHE: std::sync::OnceLock<std::collections::HashMap<String, PlanksTex>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        PLANK_SETS
            .iter()
            .map(|(n, data)| {
                let img = image::load_from_memory(data)
                    .unwrap_or_else(|e| panic!("{} 解码失败: {e}", n))
                    .to_rgba8();
                let (w, h) = (img.width() as usize, img.height() as usize);
                let mut px = Vec::with_capacity(w * h);
                for y in 0..h {
                    for x in 0..w {
                        let p = img.get_pixel(x as u32, y as u32);
                        px.push([
                            (p[0] as u32 * 88 / 100) as u8,
                            (p[1] as u32 * 88 / 100) as u8,
                            (p[2] as u32 * 88 / 100) as u8,
                        ]);
                    }
                }
                (n.to_string(), PlanksTex { w, h, px })
            })
            .collect()
    });
    cache.get(name).unwrap_or_else(|| cache.get("oak").unwrap())
}

struct Bubble {
    segs: VecDeque<String>,   // 分段队列：长回复按句切分逐段播
    seg_secs: f64,            // 每段停留时长
    until: Instant,
    wood: String,
    ink: [u8; 3],
}

static SHOWN: Mutex<Option<Bubble>> = Mutex::new(None);

/// 长文本按句切分：优先在 。！？； 处断，每段不超过 60 字。
fn split_segments(text: &str) -> VecDeque<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 60 {
        return VecDeque::from([text.to_string()]);
    }
    let mut segs = Vec::new();
    let mut buf = String::new();
    for c in chars {
        buf.push(c);
        if buf.chars().count() >= 55 && "。！？；，、".contains(c) {
            segs.push(buf.trim().to_string());
            buf.clear();
        } else if buf.chars().count() >= 62 {
            segs.push(buf.trim().to_string());
            buf.clear();
        }
    }
    if !buf.trim().is_empty() {
        segs.push(buf.trim().to_string());
    }
    VecDeque::from(segs)
}

/// 显示一段文本；秒数随长度自适应。材质/文字色读 companion.json。
pub fn show(text: &str, secs: f64) {
    let (wood, ink) = crate::companion::visual();
    let segs = split_segments(text);
    crate::bridge::log(&format!(
        "bubble::show: {:?} wood={} ink={:?} 段数={}",
        segs.front(), wood, ink, segs.len()
    ));
    let seg_secs = if secs > 0.0 { secs } else { 4.5 };
    let until = Instant::now()
        + Duration::from_secs_f64((seg_secs * segs.len() as f64).min(30.0));
    *SHOWN.lock().unwrap() = Some(Bubble {
        segs,
        seg_secs,
        until,
        wood,
        ink,
    });
}

pub fn current() -> Option<(String, String, [u8; 3])> {
    let g = SHOWN.lock().unwrap();
    g.as_ref()
        .filter(|b| Instant::now() < b.until)
        .map(|b| (b.segs.front().cloned().unwrap_or_default(), b.wood.clone(), b.ink))
}

/// 当前段到期：还有下一段就滚到下一段并返回 true（换内容重绘）；
/// 全部播完则清空并返回 true（擦除重绘）。
pub fn should_redraw() -> bool {
    let mut g = SHOWN.lock().unwrap();
    if let Some(b) = g.as_mut() {
        if Instant::now() >= b.until {
            b.segs.pop_front();
            if !b.segs.is_empty() {
                b.until = Instant::now()
                    + Duration::from_secs_f64(b.seg_secs * (b.segs.front().map(|s| s.chars().count()).unwrap_or(10) as f64).max(3.0) / 10.0);
                return true;
            }
            *g = None;
            return true;
        }
    }
    false
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn put(frame: &mut crate::render::Frame, x: i32, y: i32, c: [u8; 3]) {
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
        return;
    }
    let off = ((y as usize * frame.width) + x as usize) * 4;
    frame.pixels[off..off + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
}

/// 在 frame 顶部居中画气泡（含尾巴）。文字经 GDI 掩膜混入。
pub fn draw(frame: &mut crate::render::Frame, text: &str, wood: &str, ink: [u8; 3]) {
    crate::bridge::log(&format!("bubble::draw: frame={}x{}", frame.width, frame.height));
    let font_h = (frame.width as i32 / 22).clamp(22, 40);
    let max_w = (frame.width as i32 * 3 / 4).max(160);

    unsafe {
        let hdc_screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(hdc_screen);
        ReleaseDC(null_mut(), hdc_screen);

        // 1) 量文字尺寸（DT_CALCRECT），高度不够就换行后重算
        let wide = utf16(text);
        let font = CreateFontW(
            font_h, 0, 0, 0, 500, // 半粗
            0, 0, 0, 1, 0, 0, ANTIALIASED_QUALITY as u32, 0,
            utf16("Microsoft YaHei UI").as_ptr(),
        );
        let old_font = SelectObject(mem, font);
        let mut rect = RECT { left: 0, top: 0, right: max_w, bottom: 0 };
        DrawTextW(
            mem, wide.as_ptr(), (wide.len() - 1) as i32, &mut rect,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        let text_w = (rect.right - rect.left).max(1);
        let text_h = (rect.bottom - rect.top).max(font_h);

        // 2) 白底黑字的文字掩膜 DIB
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = text_w;
        info.bmiHeader.biHeight = -text_h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        let bmp = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let old_bmp = SelectObject(mem, bmp);
        PatBlt(mem, 0, 0, text_w, text_h, WHITENESS);
        SetTextColor(mem, 0x00000000);
        SetBkMode(mem, TRANSPARENT as i32);
        let mut rect = RECT { left: 0, top: 0, right: text_w, bottom: text_h };
        DrawTextW(
            mem, wide.as_ptr(), (wide.len() - 1) as i32, &mut rect,
            DT_WORDBREAK | DT_NOPREFIX,
        );
        let mask = std::slice::from_raw_parts(bits as *const u8, (text_w * text_h) as usize * 4);

        // 3) 气泡几何（frame 顶部居中）
        let pad = font_h / 2 + BORDER;
        let bw = text_w + pad * 2;
        let bh = text_h + pad * 2;
        let bx0 = ((frame.width as i32 - bw) / 2).max(2);
        let by0 = 6;
        let bx1 = (bx0 + bw).min(frame.width as i32 - 2);
        let by1 = by0 + bh;
        let tx0 = bx0 + BORDER;
        let ty0 = by0 + BORDER;

        // 4) 告示牌：官方橡木板材质平铺（每纹素若干帧像素，保持像素感）
        let tex = planks(wood);
        let texel = ((bw / (tex.w as i32 * 2)).clamp(3, 12)) as usize;
        for y in by0..by1 {
            for x in bx0..bx1 {
                let tx = (((x - bx0).max(0)) as usize / texel) % tex.w;
                let ty = (((y - by0).max(0)) as usize / texel) % tex.h;
                let mut c = tex.px[ty * tex.w + tx];
                let border = x < bx0 + BORDER
                    || x >= bx1 - BORDER
                    || y < by0 + BORDER
                    || y >= by1 - BORDER;
                if border {
                    c = [
                        (c[0] as u32 * 55 / 100) as u8,
                        (c[1] as u32 * 55 / 100) as u8,
                        (c[2] as u32 * 55 / 100) as u8,
                    ];
                }
                put(frame, x, y, c);
            }
        }
        // 尾巴（板纹色）
        let tail_x = frame.width as i32 / 2;
        for i in 0..TAIL {
            let half = (TAIL - i) * 2 / 3 + 1;
            for x in (tail_x - half)..=(tail_x + half) {
                let tx = (((x - tail_x + half).max(0)) as usize / texel) % tex.w;
                let ty = (((by1 + i - 1 - by0).max(0)) as usize / texel) % tex.h;
                let c = tex.px[ty * tex.w + tx];
                put(frame, x, by1 + i, c);
            }
        }

        // 5) 文字掩膜混入：亮度→覆盖度，深灰字叠在木板色上
        for my in 0..text_h {
            for mx in 0..text_w {
                let off = ((my as usize * text_w as usize) + mx as usize) * 4;
                let b = mask[off] as u32;
                let g = mask[off + 1] as u32;
                let r = mask[off + 2] as u32;
                let lum = (r + g + b) / 3;
                let cov = 255 - lum;
                if cov == 0 {
                    continue;
                }
                let fx = tx0 + mx;
                let fy = ty0 + my;
                if fx < 0 || fy < 0 {
                    continue;
                }
                let idx = ((fy as usize * frame.width) + fx as usize) * 4;
                if idx + 3 >= frame.pixels.len() {
                    continue;
                }
                let base = {
                    let tx = (((fx - bx0).max(0)) as usize / texel) % tex.w;
                    let ty = (((fy - by0).max(0)) as usize / texel) % tex.h;
                    tex.px[ty * tex.w + tx]
                };
                for ch in 0..3 {
                    let base_c = base[2 - ch] as i32; // 帧是 BGR 序
                    let ink_c = ink[2 - ch] as i32;
                    let v = base_c + (ink_c - base_c) * cov as i32 / 255;
                    frame.pixels[idx + ch] = v.clamp(0, 255) as u8;
                }
                frame.pixels[idx + 3] = 255;
            }
        }

        SelectObject(mem, old_bmp);
        SelectObject(mem, old_font);
        DeleteObject(bmp);
        DeleteObject(font);
        DeleteDC(mem);

        // 一次性帧转储：把画完气泡的帧存成 PNG，诊断"帧里有没画上"用
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static DUMPED: AtomicBool = AtomicBool::new(false);
            if !DUMPED.swap(true, Ordering::Relaxed) {
                let img = image::RgbaImage::from_fn(frame.width as u32, frame.height as u32, |x, y| {
                    let off = ((y as usize * frame.width) + x as usize) * 4;
                    image::Rgba([
                        frame.pixels[off + 2], // 帧是 BGRA，dump 转 RGB
                        frame.pixels[off + 1],
                        frame.pixels[off],
                        frame.pixels[off + 3],
                    ])
                });
                let path = std::env::temp_dir().join("xiaoya-frame-dump.png");
                let saved = img.save(&path);
                crate::bridge::log(&format!(
                    "bubble: frame dump -> {:?} ok={}",
                    path, saved.is_ok()
                ));
            }
        }
    }
}
