//! PrintWindow capture (works while parked or covered), PNG through WIC, Windows OCR.
use crate::engine::{clip, Args, Ctx, Engine, Fail, R};
use crate::win::{self, hv};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::IsIconic;

/// Top-down 32-bit BGRX pixels of the whole window (window coords).
pub struct Frame {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
    pub rect: RECT,
}

fn grab(h: HWND, w: i32, ht: i32) -> R<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -ht, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0);
        let out = match bmp {
            Ok(bmp) => {
                let old = SelectObject(dc, bmp.into());
                let ok = PrintWindow(h, dc, PRINT_WINDOW_FLAGS(2)).as_bool(); // PW_RENDERFULLCONTENT
                let _ = GdiFlush();
                let v = std::slice::from_raw_parts(bits as *const u8, (w * ht * 4) as usize).to_vec();
                SelectObject(dc, old);
                let _ = DeleteObject(bmp.into());
                if ok { Ok(v) } else { Err(Fail::Ghost("PrintWindow failed".into())) }
            }
            Err(e) => Err(Fail::Ghost(format!("CreateDIBSection failed: {}", e.message()))),
        };
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        out
    }
}

/// Parked windows may never have painted some children: invalidate (no erase, so nothing is blanked), then grab
/// until two frames match, since the app repaints on its own thread after we return.
pub fn capture(h: HWND) -> R<Frame> {
    unsafe {
        if IsIconic(h).as_bool() {
            return Err(Fail::Ghost("background unavailable: window is minimized; run ghost park first".into()));
        }
        let _ = RedrawWindow(Some(h), None, None, RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW);
    }
    let rect = win::rect(h);
    let (w, ht) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || ht <= 0 {
        return Err(Fail::Ghost("window has no size".into()));
    }
    let mut px = grab(h, w, ht)?;
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(6));
        let next = grab(h, w, ht)?;
        let same = next == px;
        px = next;
        if same {
            break;
        }
    }
    Ok(Frame { w, h: ht, px, rect })
}

fn wic() -> R<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).map_err(Fail::from) }
}

fn blank(f: &Frame, c: &WICRect) -> bool {
    let mut seen: Vec<u32> = vec![];
    let (sx, sy) = ((c.Width / 40).max(1), (c.Height / 40).max(1));
    let mut x = 0;
    while x < c.Width {
        let mut y = 0;
        while y < c.Height {
            let i = (((c.Y + y) * f.w + c.X + x) * 4) as usize;
            let v = u32::from_le_bytes([f.px[i], f.px[i + 1], f.px[i + 2], 0]);
            if !seen.contains(&v) {
                seen.push(v);
                if seen.len() > 2 {
                    return false;
                }
            }
            y += sy;
        }
        x += sx;
    }
    true
}

impl Engine {
    pub fn shot(&mut self, cx: &mut Ctx, g: Args) -> R<()> {
        let h = g.win;
        let p = std::path::Path::new(&g.pos[0]);
        let path = if p.is_absolute() || cx.cwd.is_empty() { p.to_path_buf() } else { std::path::Path::new(&cx.cwd).join(p) };
        let path = std::path::absolute(&path).unwrap_or(path).to_string_lossy().into_owned();
        let mut scale: f64 = 1.0;
        if let Some(v) = g.opt.get("--scale") {
            scale = v.parse().ok().filter(|s: &f64| *s > 0.0 && *s <= 4.0).ok_or_else(|| Fail::Usage("--scale needs a number in (0,4]".into()))?;
        }
        let mut max = 0u32;
        if let Some(v) = g.opt.get("--max") {
            max = v.parse().ok().filter(|m: &u32| *m >= 16).ok_or_else(|| Fail::Usage("--max needs pixels >= 16".into()))?;
        }
        let gray = g.flags.contains("--gray");
        let f = capture(h)?;
        // Crop the invisible resize borders DWM adds around the visible frame.
        let mut crop = WICRect { X: 0, Y: 0, Width: f.w, Height: f.h };
        let mut fr = RECT::default();
        if unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut fr as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32) }.is_ok() {
            let (x0, y0) = ((fr.left - f.rect.left).max(0), (fr.top - f.rect.top).max(0));
            let (x1, y1) = ((fr.right - f.rect.left).min(f.w), (fr.bottom - f.rect.top).min(f.h));
            if x1 > x0 && y1 > y0 {
                crop = WICRect { X: x0, Y: y0, Width: x1 - x0, Height: y1 - y0 };
            }
        }
        if max > 0 {
            scale = scale.min(max as f64 / crop.Width.max(crop.Height) as f64);
        }
        let ow = (round_even(crop.Width as f64 * scale) as u32).max(1);
        let oh = (round_even(crop.Height as f64 * scale) as u32).max(1);
        let is_blank = blank(&f, &crop);
        unsafe {
            let fac = wic()?;
            let src = fac.CreateBitmapFromMemory(f.w as u32, f.h as u32, &GUID_WICPixelFormat32bppBGR, (f.w * 4) as u32, &f.px)?;
            let clipper = fac.CreateBitmapClipper()?;
            clipper.Initialize(&src, &crop)?;
            let scaler = fac.CreateBitmapScaler()?;
            scaler.Initialize(&clipper, ow, oh, WICBitmapInterpolationModeHighQualityCubic)?;
            let source: IWICBitmapSource = if gray {
                // 8-bit grayscale PNG: about a third of the bytes of RGB.
                let conv = fac.CreateFormatConverter()?;
                conv.Initialize(&scaler, &GUID_WICPixelFormat8bppGray, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
                conv.into()
            } else {
                scaler.into()
            };
            let stream = fac.CreateStream()?;
            let wp = win::wide(&path);
            stream.InitializeFromFilename(PCWSTR(wp.as_ptr()), GENERIC_WRITE.0).map_err(|e| Fail::Ghost(format!("cannot write {path}: {}", e.message())))?;
            let enc = fac.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame: Option<IWICBitmapFrameEncode> = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(|| Fail::Ghost("PNG encoder failed".into()))?;
            frame.Initialize(None)?;
            frame.SetSize(ow, oh)?;
            let mut fmt = if gray { GUID_WICPixelFormat8bppGray } else { GUID_WICPixelFormat24bppBGR };
            frame.SetPixelFormat(&mut fmt)?;
            frame.WriteSource(&source, std::ptr::null())?;
            frame.Commit()?;
            enc.Commit()?;
        }
        if is_blank {
            cx.err.push_str("warning: capture looks blank (app may not support PrintWindow)\n");
        }
        cx.out.push_str(&format!("ok {ow}x{oh} {path}\n"));
        Ok(())
    }

    pub fn ocr(&mut self, cx: &mut Ctx, g: Args) -> R<()> {
        let h = g.win;
        let mut lines = self.ocr_once(h, &g)?;
        // A window caught mid-repaint can OCR as nothing: one retry after the app has had time to paint.
        if lines.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(60));
            lines = self.ocr_once(h, &g)?;
        }
        let ws = self.st(h);
        ws.ocr.clear();
        for (t, x, y) in lines {
            ws.ocr.push((x, y));
            cx.out.push_str(&format!("o{} \"{}\" {},{}\n", ws.ocr.len(), clip(&t, 80), x, y));
        }
        Ok(())
    }

    fn ocr_once(&mut self, h: HWND, g: &Args) -> R<Vec<(String, i32, i32)>> {
        use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
        use windows::Security::Cryptography::CryptographicBuffer;
        let f = capture(h)?;
        let mut area = RECT { left: 0, top: 0, right: f.w, bottom: f.h };
        // Client area only: the title bar (already known from `windows`) and the black resize borders threw
        // Windows OCR's line layout off, dropping whole lines.
        unsafe {
            let mut cr = RECT::default();
            let mut o = POINT::default();
            if windows::Win32::UI::WindowsAndMessaging::GetClientRect(h, &mut cr).is_ok() && ClientToScreen(h, &mut o).as_bool() {
                let c = RECT { left: o.x - f.rect.left, top: o.y - f.rect.top, right: o.x - f.rect.left + cr.right, bottom: o.y - f.rect.top + cr.bottom };
                if let Some(i) = inter(&area, &c) {
                    area = i;
                }
            }
        }
        if let Some(t) = g.pos.first() {
            let b = unsafe { self.resolve(h, t)?.CurrentBoundingRectangle()? };
            let r = RECT { left: b.left - f.rect.left, top: b.top - f.rect.top, right: b.right - f.rect.left, bottom: b.bottom - f.rect.top };
            area = inter(&area, &r).ok_or_else(|| Fail::Ghost(format!("element {t} is outside the window")))?;
        }
        let (aw, ah) = (area.right - area.left, area.bottom - area.top);
        // Small UI text reads far better upscaled; at 2x Windows OCR still dropped a button label beside a check box.
        let k: i32 = (1..=3).rev().find(|&k| aw.max(ah) * k <= 4000).unwrap_or(1);
        let (iw, ih) = ((aw * k) as u32, (ah * k) as u32);
        let mut bgra = vec![0u8; (iw * ih * 4) as usize];
        unsafe {
            let fac = wic()?;
            let src = fac.CreateBitmapFromMemory(f.w as u32, f.h as u32, &GUID_WICPixelFormat32bppBGR, (f.w * 4) as u32, &f.px)?;
            let clipper = fac.CreateBitmapClipper()?;
            clipper.Initialize(&src, &WICRect { X: area.left, Y: area.top, Width: aw, Height: ah })?;
            let scaler = fac.CreateBitmapScaler()?;
            scaler.Initialize(&clipper, iw, ih, WICBitmapInterpolationModeHighQualityCubic)?;
            let conv = fac.CreateFormatConverter()?;
            conv.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            conv.CopyPixels(std::ptr::null(), iw * 4, &mut bgra)?;
        }
        if self.ocr_engine.is_none() {
            self.ocr_engine = windows::Media::Ocr::OcrEngine::TryCreateFromUserProfileLanguages().ok();
        }
        let Some(eng) = &self.ocr_engine else { return Err(Fail::Ghost("Windows OCR has no language installed".into())) };
        let buf = CryptographicBuffer::CreateFromByteArray(&bgra)?;
        let sb = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(&buf, BitmapPixelFormat::Bgra8, iw as i32, ih as i32, BitmapAlphaMode::Premultiplied)?;
        let res = eng.RecognizeAsync(&sb)?.join()?;
        let _ = sb.Close();
        let mut out = vec![];
        for line in res.Lines()? {
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, 0f32, 0f32);
            for w in line.Words()? {
                let b = w.BoundingRect()?;
                x0 = x0.min(b.X);
                y0 = y0.min(b.Y);
                x1 = x1.max(b.X + b.Width);
                y1 = y1.max(b.Y + b.Height);
            }
            let cx = area.left + ((x0 + x1) / 2.0 / k as f32) as i32;
            let cy = area.top + ((y0 + y1) / 2.0 / k as f32) as i32;
            out.push((line.Text()?.to_string(), cx, cy));
        }
        let _ = hv(h);
        Ok(out)
    }
}

fn inter(a: &RECT, b: &RECT) -> Option<RECT> {
    let r = RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// Half-to-even, like the C# version's Math.Round, so sizes match it exactly.
fn round_even(v: f64) -> f64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && r % 2.0 != 0.0 { r - v.signum() } else { r }
}
