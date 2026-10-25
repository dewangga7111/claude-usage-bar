//! Software rendering into premultiplied RGBA, presented through `UpdateLayeredWindow`.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

#[derive(Clone, Copy)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub fn with_alpha(self, a: u8) -> Self {
        Rgba(self.0, self.1, self.2, a)
    }
}

pub struct Fonts {
    pub regular: FontVec,
    pub bold: FontVec,
}

impl Fonts {
    pub fn load() -> Fonts {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or("C:\\Windows".into())).join("Fonts");
        let load = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| FontVec::try_from_vec(std::fs::read(dir.join(n)).ok()?).ok())
                .expect("no usable system font found")
        };
        Fonts {
            regular: load(&["segoeui.ttf", "arial.ttf"]),
            bold: load(&["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"]),
        }
    }
}

/// A pixmap addressed in logical (96 DPI) pixels.
pub struct Canvas {
    pub pix: Pixmap,
    pub scale: f32,
}

impl Canvas {
    pub fn new(width: u32, height: u32, scale: f32) -> Canvas {
        Canvas { pix: Pixmap::new(width.max(1), height.max(1)).expect("canvas size"), scale }
    }

    pub fn logical_width(&self) -> f32 {
        self.pix.width() as f32 / self.scale
    }

    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Rgba) {
        let s = self.scale;
        let Some(path) = rounded_rect(x * s, y * s, w * s, h * s, r * s) else { return };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.0, color.1, color.2, color.3);
        self.pix.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    pub fn measure(&self, font: &FontVec, size: f32, text: &str) -> f32 {
        let scaled = font.as_scaled(px_scale(font, size * self.scale));
        let mut width = 0.0;
        let mut last = None;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            if let Some(prev) = last {
                width += scaled.kern(prev, id);
            }
            width += scaled.h_advance(id);
            last = Some(id);
        }
        width / self.scale
    }

    /// Draws `text` with its baseline at `baseline`; returns the advance in logical pixels.
    pub fn text(&mut self, font: &FontVec, size: f32, x: f32, baseline: f32, text: &str, color: Rgba) -> f32 {
        let scale = px_scale(font, size * self.scale);
        let scaled = font.as_scaled(scale);
        let mut caret = x * self.scale;
        let start = caret;
        let y = baseline * self.scale;
        let mut last = None;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            if let Some(prev) = last {
                caret += scaled.kern(prev, id);
            }
            let glyph = id.with_scale_and_position(scale, point(caret.round(), y.round()));
            caret += scaled.h_advance(id);
            last = Some(id);
            let Some(outline) = font.outline_glyph(glyph) else { continue };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                self.blend(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, color, coverage);
            });
        }
        (caret - start) / self.scale
    }

    /// Like [`Canvas::text`] but cut with an ellipsis to fit `max_width`.
    pub fn text_fit(&mut self, font: &FontVec, size: f32, x: f32, baseline: f32, text: &str, max_width: f32, color: Rgba) {
        if self.measure(font, size, text) <= max_width {
            self.text(font, size, x, baseline, text, color);
            return;
        }
        let mut cut: String = text.into();
        while !cut.is_empty() && self.measure(font, size, &format!("{cut}…")) > max_width {
            cut.pop();
        }
        self.text(font, size, x, baseline, &format!("{}…", cut.trim_end()), color);
    }

    fn blend(&mut self, x: i32, y: i32, c: Rgba, coverage: f32) {
        let (w, h) = (self.pix.width() as i32, self.pix.height() as i32);
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let a = c.3 as f32 / 255.0 * coverage.clamp(0.0, 1.0);
        let keep = 1.0 - a;
        let px = &mut self.pix.data_mut()[i..i + 4];
        px[0] = (c.0 as f32 * a + px[0] as f32 * keep + 0.5) as u8;
        px[1] = (c.1 as f32 * a + px[1] as f32 * keep + 0.5) as u8;
        px[2] = (c.2 as f32 * a + px[2] as f32 * keep + 0.5) as u8;
        px[3] = (255.0 * a + px[3] as f32 * keep + 0.5) as u8;
    }

    /// Returns a copy of the top `height` logical pixels drawn over `background`.
    pub fn crop_onto(&self, height: f32, background: impl FnOnce(&mut Canvas)) -> Canvas {
        let mut out = Canvas::new(self.pix.width(), (height * self.scale).ceil() as u32, self.scale);
        background(&mut out);
        out.pix.draw_pixmap(0, 0, self.pix.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        out
    }
}

/// `PxScale` is in units of the font's line height; convert from an em size.
fn px_scale(font: &FontVec, em_px: f32) -> PxScale {
    let units_per_em = font.units_per_em().unwrap_or(2048.0);
    PxScale::from(em_px * font.height_unscaled() / units_per_em)
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    // Distance from the corner to the cubic control points of a quarter circle.
    let k = r * (1.0 - 0.552_284_8);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - k, y, x + w, y + k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - k, x + w - k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + k, y + h, x, y + h - k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + k, x + k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Pushes the canvas to a `WS_EX_LAYERED` window and moves it to (`x`, `y`) in screen pixels.
pub fn present(hwnd: HWND, canvas: &Canvas, x: i32, y: i32) {
    let (w, h) = (canvas.pix.width() as i32, canvas.pix.height() as i32);
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        if let Ok(bitmap) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (w * h * 4) as usize);
            for (d, s) in dst.chunks_exact_mut(4).zip(canvas.pix.data().chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
            let old = SelectObject(mem, bitmap.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&POINT { x, y }),
                Some(&SIZE { cx: w, cy: h }),
                Some(mem),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bitmap.into());
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
}
