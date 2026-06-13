// PIL-style image I/O + manipulation, native via the `image` crate.
//
// Complements the stryke core `image_*` builtins (which are filters/math:
// blur, edge, sharpen, grayscale, rotate, flip operating on pixel data) by
// providing the file-I/O and manipulation surface PIL has that the builtins
// lack: open/save/decode across every raster format, new, crop, resize,
// thumbnail, paste, mode convert, per-pixel access, and ImageDraw-style
// shape + text drawing.
//
// Images live in a handle registry (like a PIL `Image` object): `img_open`
// returns an integer handle; later ops reference it; `img_save` writes it.

use image::DynamicImage;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static IMAGES: once_cell::sync::OnceCell<parking_lot_lite::Mutex<HashMap<u64, DynamicImage>>> =
    once_cell::sync::OnceCell::new();

// A tiny std-Mutex wrapper so we don't add parking_lot just for this module.
mod parking_lot_lite {
    pub struct Mutex<T>(std::sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub fn new(t: T) -> Self {
            Mutex(std::sync::Mutex::new(t))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}

fn images() -> &'static parking_lot_lite::Mutex<HashMap<u64, DynamicImage>> {
    IMAGES.get_or_init(|| parking_lot_lite::Mutex::new(HashMap::new()))
}

static NEXT_IMG: AtomicU64 = AtomicU64::new(1);

static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

fn insert_image(img: DynamicImage) -> u64 {
    let h = NEXT_IMG.fetch_add(1, Ordering::Relaxed);
    images().lock().insert(h, img);
    h
}

/// Replace the image under `handle` with `f(image)`.
fn transform<F>(handle: u64, f: F) -> Result<()>
where
    F: FnOnce(DynamicImage) -> Result<DynamicImage>,
{
    let mut map = images().lock();
    let img = map
        .remove(&handle)
        .ok_or_else(|| anyhow!("unknown image handle: {handle}"))?;
    let new = f(img)?;
    map.insert(handle, new);
    Ok(())
}

/// Read-only access to the image under `handle`.
fn with_image<F, T>(handle: u64, f: F) -> Result<T>
where
    F: FnOnce(&DynamicImage) -> Result<T>,
{
    let map = images().lock();
    let img = map
        .get(&handle)
        .ok_or_else(|| anyhow!("unknown image handle: {handle}"))?;
    f(img)
}

fn req_u64_img(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing {key}"))
}

fn opt_i64(v: &Value, key: &str, default: i64) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(default)
}

fn mode_of(img: &DynamicImage) -> &'static str {
    use image::ColorType::*;
    match img.color() {
        L8 | L16 => "L",
        La8 | La16 => "LA",
        Rgb8 | Rgb16 | Rgb32F => "RGB",
        Rgba8 | Rgba16 | Rgba32F => "RGBA",
        _ => "RGBA",
    }
}

/// Parse a color from `"#rrggbb"`, `"#rrggbbaa"`, or `[r,g,b(,a)]`.
fn parse_color(v: Option<&Value>) -> image::Rgba<u8> {
    let default = image::Rgba([0, 0, 0, 255]);
    let Some(v) = v else { return default };
    match v {
        Value::String(s) => {
            let h = s.trim_start_matches('#');
            let p = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
            if h.len() >= 8 {
                image::Rgba([p(0), p(2), p(4), p(6)])
            } else if h.len() >= 6 {
                image::Rgba([p(0), p(2), p(4), 255])
            } else {
                default
            }
        }
        Value::Array(a) => {
            let c = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0) as u8;
            image::Rgba([c(0), c(1), c(2), a.get(3).and_then(Value::as_u64).unwrap_or(255) as u8])
        }
        _ => default,
    }
}

fn filter_of(v: &Value) -> image::imageops::FilterType {
    use image::imageops::FilterType::*;
    match v.get("filter").and_then(Value::as_str).unwrap_or("lanczos3") {
        "nearest" => Nearest,
        "triangle" | "bilinear" => Triangle,
        "catmullrom" | "bicubic" => CatmullRom,
        "gaussian" => Gaussian,
        _ => Lanczos3,
    }
}

fn info_json(handle: u64, img: &DynamicImage) -> Value {
    json!({
        "handle": handle,
        "width": img.width(),
        "height": img.height(),
        "mode": mode_of(img),
    })
}

// ── ops ──────────────────────────────────────────────────────────────────────

fn op_img_open(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let img = image::open(path).map_err(|e| anyhow!("open {path}: {e}"))?;
    let h = insert_image(img);
    with_image(h, |img| Ok(info_json(h, img)))
}

fn op_img_new(opts: Value) -> Result<Value> {
    let w = req_u64_img(&opts, "width")? as u32;
    let h = req_u64_img(&opts, "height")? as u32;
    let color = parse_color(opts.get("color"));
    let buf = image::RgbaImage::from_pixel(w, h, color);
    let handle = insert_image(DynamicImage::ImageRgba8(buf));
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_save(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let path = req_str(&opts, "path")?.to_string();
    with_image(handle, |img| {
        img.save(&path).map_err(|e| anyhow!("save {path}: {e}"))?;
        Ok(json!({"ok": true, "path": path}))
    })
}

fn op_img_info(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_resize(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let w = req_u64_img(&opts, "width")? as u32;
    let h = req_u64_img(&opts, "height")? as u32;
    let filter = filter_of(&opts);
    transform(handle, |img| Ok(img.resize_exact(w, h, filter)))?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_thumbnail(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let max = req_u64_img(&opts, "max")? as u32;
    let filter = filter_of(&opts);
    transform(handle, |img| Ok(img.resize(max, max, filter)))?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_crop(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let x = req_u64_img(&opts, "x")? as u32;
    let y = req_u64_img(&opts, "y")? as u32;
    let w = req_u64_img(&opts, "width")? as u32;
    let h = req_u64_img(&opts, "height")? as u32;
    transform(handle, |img| Ok(img.crop_imm(x, y, w, h)))?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_rotate(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let deg = opt_i64(&opts, "degrees", 90).rem_euclid(360);
    transform(handle, |img| {
        Ok(match deg {
            90 => img.rotate90(),
            180 => img.rotate180(),
            270 => img.rotate270(),
            0 => img,
            other => {
                // Arbitrary angle via imageproc, transparent fill.
                use imageproc::geometric_transformations::{
                    rotate_about_center, Border, Interpolation,
                };
                let rgba = img.to_rgba8();
                let rad = (other as f32).to_radians();
                let out = rotate_about_center(
                    &rgba,
                    rad,
                    Interpolation::Bilinear,
                    Border::Constant(image::Rgba([0, 0, 0, 0])),
                );
                DynamicImage::ImageRgba8(out)
            }
        })
    })?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_flip(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let dir = opts.get("dir").and_then(Value::as_str).unwrap_or("h");
    transform(handle, |img| {
        Ok(match dir {
            "v" | "vertical" => img.flipv(),
            _ => img.fliph(),
        })
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_convert(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let mode = opts.get("mode").and_then(Value::as_str).unwrap_or("RGBA");
    transform(handle, |img| {
        Ok(match mode.to_ascii_uppercase().as_str() {
            "L" | "GRAY" | "GRAYSCALE" => DynamicImage::ImageLuma8(img.to_luma8()),
            "LA" => DynamicImage::ImageLumaA8(img.to_luma_alpha8()),
            "RGB" => DynamicImage::ImageRgb8(img.to_rgb8()),
            _ => DynamicImage::ImageRgba8(img.to_rgba8()),
        })
    })?;
    with_image(handle, |img| Ok(info_json(handle, img)))
}

fn op_img_paste(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let src = req_u64_img(&opts, "src")?;
    let x = opt_i64(&opts, "x", 0);
    let y = opt_i64(&opts, "y", 0);
    let src_img = with_image(src, |img| Ok(img.to_rgba8()))?;
    transform(handle, |img| {
        let mut base = img.to_rgba8();
        image::imageops::overlay(&mut base, &src_img, x, y);
        Ok(DynamicImage::ImageRgba8(base))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_get_pixel(opts: Value) -> Result<Value> {
    use image::GenericImageView;
    let handle = req_u64_img(&opts, "handle")?;
    let x = req_u64_img(&opts, "x")? as u32;
    let y = req_u64_img(&opts, "y")? as u32;
    with_image(handle, |img| {
        if x >= img.width() || y >= img.height() {
            return Err(anyhow!("pixel ({x},{y}) out of bounds"));
        }
        let p = img.get_pixel(x, y).0;
        Ok(json!({"r": p[0], "g": p[1], "b": p[2], "a": p[3]}))
    })
}

fn op_img_put_pixel(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let x = req_u64_img(&opts, "x")? as u32;
    let y = req_u64_img(&opts, "y")? as u32;
    let color = parse_color(opts.get("color"));
    transform(handle, |img| {
        let mut buf = img.to_rgba8();
        if x < buf.width() && y < buf.height() {
            buf.put_pixel(x, y, color);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_draw_rect(opts: Value) -> Result<Value> {
    use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut};
    use imageproc::rect::Rect;
    let handle = req_u64_img(&opts, "handle")?;
    let x = opt_i64(&opts, "x", 0) as i32;
    let y = opt_i64(&opts, "y", 0) as i32;
    let w = req_u64_img(&opts, "width")? as u32;
    let h = req_u64_img(&opts, "height")? as u32;
    let color = parse_color(opts.get("color"));
    let fill = opts.get("fill").and_then(Value::as_bool).unwrap_or(true);
    transform(handle, |img| {
        let mut buf = img.to_rgba8();
        let rect = Rect::at(x, y).of_size(w.max(1), h.max(1));
        if fill {
            draw_filled_rect_mut(&mut buf, rect, color);
        } else {
            draw_hollow_rect_mut(&mut buf, rect, color);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_draw_line(opts: Value) -> Result<Value> {
    use imageproc::drawing::draw_line_segment_mut;
    let handle = req_u64_img(&opts, "handle")?;
    let x0 = opt_i64(&opts, "x0", 0) as f32;
    let y0 = opt_i64(&opts, "y0", 0) as f32;
    let x1 = opt_i64(&opts, "x1", 0) as f32;
    let y1 = opt_i64(&opts, "y1", 0) as f32;
    let color = parse_color(opts.get("color"));
    transform(handle, |img| {
        let mut buf = img.to_rgba8();
        draw_line_segment_mut(&mut buf, (x0, y0), (x1, y1), color);
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_draw_circle(opts: Value) -> Result<Value> {
    use imageproc::drawing::{draw_filled_circle_mut, draw_hollow_circle_mut};
    let handle = req_u64_img(&opts, "handle")?;
    let cx = opt_i64(&opts, "x", 0) as i32;
    let cy = opt_i64(&opts, "y", 0) as i32;
    let r = req_u64_img(&opts, "radius")? as i32;
    let color = parse_color(opts.get("color"));
    let fill = opts.get("fill").and_then(Value::as_bool).unwrap_or(true);
    transform(handle, |img| {
        let mut buf = img.to_rgba8();
        if fill {
            draw_filled_circle_mut(&mut buf, (cx, cy), r, color);
        } else {
            draw_hollow_circle_mut(&mut buf, (cx, cy), r, color);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_draw_text(opts: Value) -> Result<Value> {
    use ab_glyph::{FontRef, PxScale};
    use imageproc::drawing::draw_text_mut;
    let handle = req_u64_img(&opts, "handle")?;
    let x = opt_i64(&opts, "x", 0) as i32;
    let y = opt_i64(&opts, "y", 0) as i32;
    let text = req_str(&opts, "text")?.to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(16.0) as f32;
    let color = parse_color(opts.get("color"));
    // Optional caller font file; otherwise the vendored DejaVu Sans.
    let font_bytes: Vec<u8> = match opts.get("font").and_then(Value::as_str) {
        Some(p) => std::fs::read(p).map_err(|e| anyhow!("font {p}: {e}"))?,
        None => FONT_BYTES.to_vec(),
    };
    let font = FontRef::try_from_slice(&font_bytes).map_err(|_| anyhow!("invalid font"))?;
    let scale = PxScale::from(size);
    transform(handle, |img| {
        let mut buf = img.to_rgba8();
        draw_text_mut(&mut buf, color, x, y, scale, &font, &text);
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_close(opts: Value) -> Result<Value> {
    let handle = req_u64_img(&opts, "handle")?;
    let removed = images().lock().remove(&handle).is_some();
    Ok(json!({"ok": true, "closed": removed}))
}

// ── filters (all in-place on the handle's image) ─────────────────────────────

fn op_img_blur(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let sigma = opts.get("sigma").and_then(Value::as_f64).unwrap_or(2.0) as f32;
    transform(h, |img| Ok(img.blur(sigma)))?;
    Ok(json!({"ok": true}))
}

fn op_img_sharpen(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let sigma = opts.get("sigma").and_then(Value::as_f64).unwrap_or(2.0) as f32;
    let threshold = opts.get("threshold").and_then(Value::as_i64).unwrap_or(2) as i32;
    transform(h, |img| Ok(img.unsharpen(sigma, threshold)))?;
    Ok(json!({"ok": true}))
}

fn op_img_brighten(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let value = opts.get("value").and_then(Value::as_i64).unwrap_or(0) as i32;
    transform(h, |img| Ok(img.brighten(value)))?;
    Ok(json!({"ok": true}))
}

fn op_img_contrast(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let c = opts.get("value").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    transform(h, |img| Ok(img.adjust_contrast(c)))?;
    Ok(json!({"ok": true}))
}

fn op_img_huerotate(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let deg = opts.get("degrees").and_then(Value::as_i64).unwrap_or(0) as i32;
    transform(h, |img| Ok(img.huerotate(deg)))?;
    Ok(json!({"ok": true}))
}

fn op_img_invert(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |mut img| {
        img.invert();
        Ok(img)
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_grayscale(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| Ok(img.grayscale()))?;
    Ok(json!({"ok": true}))
}

/// Per-pixel RGBA transform helper (alpha preserved unless touched).
fn pixel_map<F>(h: u64, f: F) -> Result<Value>
where
    F: Fn([u8; 4]) -> [u8; 4],
{
    transform(h, |img| {
        let mut buf = img.to_rgba8();
        for px in buf.pixels_mut() {
            px.0 = f(px.0);
        }
        Ok(image::DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

fn op_img_gamma(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let g = opts.get("gamma").and_then(Value::as_f64).unwrap_or(1.0).max(0.01);
    let inv = 1.0 / g;
    let lut: Vec<u8> = (0..256).map(|i| (255.0 * (i as f64 / 255.0).powf(inv)).round() as u8).collect();
    pixel_map(h, |[r, g2, b, a]| [lut[r as usize], lut[g2 as usize], lut[b as usize], a])
}

fn op_img_threshold(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let level = opts.get("level").and_then(Value::as_u64).unwrap_or(128) as u32;
    pixel_map(h, move |[r, g, b, a]| {
        let luma = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
        let v = if luma >= level { 255 } else { 0 };
        [v, v, v, a]
    })
}

fn op_img_posterize(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let levels = opts.get("levels").and_then(Value::as_u64).unwrap_or(4).clamp(2, 256) as u32;
    let step = 255 / (levels - 1);
    let q = move |c: u8| -> u8 { (((c as u32 + step / 2) / step) * step).min(255) as u8 };
    pixel_map(h, move |[r, g, b, a]| [q(r), q(g), q(b), a])
}

fn op_img_sepia(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    pixel_map(h, |[r, g, b, a]| {
        let (rf, gf, bf) = (r as f64, g as f64, b as f64);
        let nr = (0.393 * rf + 0.769 * gf + 0.189 * bf).min(255.0) as u8;
        let ng = (0.349 * rf + 0.686 * gf + 0.168 * bf).min(255.0) as u8;
        let nb = (0.272 * rf + 0.534 * gf + 0.131 * bf).min(255.0) as u8;
        [nr, ng, nb, a]
    })
}

fn op_img_tint(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let c = parse_color(opts.get("color"));
    let (tr, tg, tb) = (c.0[0] as u32, c.0[1] as u32, c.0[2] as u32);
    pixel_map(h, move |[r, g, b, a]| {
        [
            (r as u32 * tr / 255) as u8,
            (g as u32 * tg / 255) as u8,
            (b as u32 * tb / 255) as u8,
            a,
        ]
    })
}

// ── extended processing ops (PIL-complete surface) ───────────────────────────
//
// Everything below stays native: manual per-pixel work plus the handful of
// `imageproc` routines verified to exist (median_filter, canny). No new crates.

/// Rec.601 luma of an RGB triple (0..255).
fn luma601(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
}

/// An owned RGBA copy of the image under `handle`.
fn rgba_of(handle: u64) -> Result<image::RgbaImage> {
    with_image(handle, |img| Ok(img.to_rgba8()))
}

/// Stretch each RGB channel to the full 0..255 range, clipping `cutoff`
/// percent of each channel's histogram tails (default 0).
fn op_img_autocontrast(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let cutoff = opts.get("cutoff").and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 49.0);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let n = (buf.width() * buf.height()) as f64;
        let clip = (n * cutoff / 100.0) as u32;
        let mut lut = [[0u8; 256]; 3];
        for c in 0..3 {
            let mut hist = [0u32; 256];
            for px in buf.pixels() {
                hist[px.0[c] as usize] += 1;
            }
            let (mut lo, mut acc) = (0usize, 0u32);
            while lo < 255 {
                acc += hist[lo];
                if acc > clip {
                    break;
                }
                lo += 1;
            }
            let (mut hi, mut acc2) = (255usize, 0u32);
            while hi > 0 {
                acc2 += hist[hi];
                if acc2 > clip {
                    break;
                }
                hi -= 1;
            }
            if hi <= lo {
                for (i, slot) in lut[c].iter_mut().enumerate() {
                    *slot = i as u8;
                }
            } else {
                let scale = 255.0 / (hi - lo) as f64;
                for (i, slot) in lut[c].iter_mut().enumerate() {
                    *slot = (((i as f64 - lo as f64) * scale).round()).clamp(0.0, 255.0) as u8;
                }
            }
        }
        for px in buf.pixels_mut() {
            for c in 0..3 {
                px.0[c] = lut[c][px.0[c] as usize];
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Per-channel histogram equalization (flattens the tonal distribution).
fn op_img_equalize(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| {
        let mut buf = img.to_rgba8();
        let total = (buf.width() * buf.height()).max(1) as f64;
        let mut lut = [[0u8; 256]; 3];
        for c in 0..3 {
            let mut hist = [0u32; 256];
            for px in buf.pixels() {
                hist[px.0[c] as usize] += 1;
            }
            let mut cum = 0u32;
            for (i, &count) in hist.iter().enumerate() {
                cum += count;
                lut[c][i] = (cum as f64 / total * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        for px in buf.pixels_mut() {
            for c in 0..3 {
                px.0[c] = lut[c][px.0[c] as usize];
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Invert all channel values at or above `threshold` (default 128).
fn op_img_solarize(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let t = opts.get("threshold").and_then(Value::as_u64).unwrap_or(128) as u8;
    pixel_map(h, move |[r, g, b, a]| {
        let f = |v: u8| if v >= t { 255 - v } else { v };
        [f(r), f(g), f(b), a]
    })
}

/// Map grayscale luma to a gradient between `black` (luma 0) and `white`
/// (luma 255). Colors accept "#rrggbb"/[r,g,b].
fn op_img_colorize(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let lo = parse_color(opts.get("black"));
    let hi = parse_color(opts.get("white"));
    pixel_map(h, move |[r, g, b, a]| {
        let t = luma601(r, g, b) as f64 / 255.0;
        let lerp = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
        [lerp(lo.0[0], hi.0[0]), lerp(lo.0[1], hi.0[1]), lerp(lo.0[2], hi.0[2]), a]
    })
}

/// Apply a 3x3 convolution kernel (flattened row-major, 9 numbers) with an
/// optional `divisor` (default = kernel sum or 1) and `offset` (default 0).
/// Used by emboss/edge presets and arbitrary user kernels.
fn convolve3(buf: &image::RgbaImage, k: &[f64; 9], div: f64, off: f64) -> image::RgbaImage {
    let (w, h) = (buf.width(), buf.height());
    let mut out = image::RgbaImage::new(w, h);
    let div = if div.abs() < f64::EPSILON { 1.0 } else { div };
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f64; 3];
            for ky in 0..3i32 {
                for kx in 0..3i32 {
                    let sx = (x as i32 + kx - 1).clamp(0, w as i32 - 1) as u32;
                    let sy = (y as i32 + ky - 1).clamp(0, h as i32 - 1) as u32;
                    let p = buf.get_pixel(sx, sy).0;
                    let kv = k[(ky * 3 + kx) as usize];
                    for c in 0..3 {
                        acc[c] += p[c] as f64 * kv;
                    }
                }
            }
            let a = buf.get_pixel(x, y).0[3];
            out.put_pixel(
                x,
                y,
                image::Rgba([
                    (acc[0] / div + off).clamp(0.0, 255.0) as u8,
                    (acc[1] / div + off).clamp(0.0, 255.0) as u8,
                    (acc[2] / div + off).clamp(0.0, 255.0) as u8,
                    a,
                ]),
            );
        }
    }
    out
}

/// Emboss via a fixed 3x3 kernel.
fn op_img_emboss(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| {
        let buf = img.to_rgba8();
        let k = [-2.0, -1.0, 0.0, -1.0, 1.0, 1.0, 0.0, 1.0, 2.0];
        Ok(DynamicImage::ImageRgba8(convolve3(&buf, &k, 1.0, 128.0)))
    })?;
    Ok(json!({"ok": true}))
}

/// Arbitrary 3x3 convolution. opts: kernel (9 numbers), divisor?, offset?.
fn op_img_convolve(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let arr = opts
        .get("kernel")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing kernel (expected 9 numbers)"))?;
    if arr.len() < 9 {
        return Err(anyhow!("kernel must have 9 numbers"));
    }
    let mut k = [0.0f64; 9];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = arr[i].as_f64().unwrap_or(0.0);
    }
    let sum: f64 = k.iter().sum();
    let div = opts.get("divisor").and_then(Value::as_f64).unwrap_or(if sum.abs() < f64::EPSILON { 1.0 } else { sum });
    let off = opts.get("offset").and_then(Value::as_f64).unwrap_or(0.0);
    transform(h, move |img| {
        let buf = img.to_rgba8();
        Ok(DynamicImage::ImageRgba8(convolve3(&buf, &k, div, off)))
    })?;
    Ok(json!({"ok": true}))
}

/// Canny edge detection → white edges on black. opts: low (default 30), high
/// (default 100).
fn op_img_edges(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let low = opts.get("low").and_then(Value::as_f64).unwrap_or(30.0) as f32;
    let high = opts.get("high").and_then(Value::as_f64).unwrap_or(100.0) as f32;
    transform(h, move |img| {
        let edges = imageproc::edges::canny(&img.to_luma8(), low.min(high), high.max(low));
        Ok(DynamicImage::ImageLuma8(edges))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}

/// Separable box blur with radius `r` (default 2).
fn op_img_box_blur(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let r = opts.get("radius").and_then(Value::as_u64).unwrap_or(2).clamp(1, 64) as i32;
    transform(h, move |img| {
        let src = img.to_rgba8();
        let (w, hgt) = (src.width() as i32, src.height() as i32);
        let win = (2 * r + 1) as f64;
        // horizontal pass
        let mut tmp = image::RgbaImage::new(w as u32, hgt as u32);
        for y in 0..hgt {
            for x in 0..w {
                let mut acc = [0.0f64; 3];
                for dx in -r..=r {
                    let sx = (x + dx).clamp(0, w - 1) as u32;
                    let p = src.get_pixel(sx, y as u32).0;
                    for c in 0..3 {
                        acc[c] += p[c] as f64;
                    }
                }
                let a = src.get_pixel(x as u32, y as u32).0[3];
                tmp.put_pixel(x as u32, y as u32, image::Rgba([(acc[0] / win) as u8, (acc[1] / win) as u8, (acc[2] / win) as u8, a]));
            }
        }
        // vertical pass
        let mut out = image::RgbaImage::new(w as u32, hgt as u32);
        for y in 0..hgt {
            for x in 0..w {
                let mut acc = [0.0f64; 3];
                for dy in -r..=r {
                    let sy = (y + dy).clamp(0, hgt - 1) as u32;
                    let p = tmp.get_pixel(x as u32, sy).0;
                    for c in 0..3 {
                        acc[c] += p[c] as f64;
                    }
                }
                let a = tmp.get_pixel(x as u32, y as u32).0[3];
                out.put_pixel(x as u32, y as u32, image::Rgba([(acc[0] / win) as u8, (acc[1] / win) as u8, (acc[2] / win) as u8, a]));
            }
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

/// Median filter (despeckle). opts: radius (default 1).
fn op_img_median(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let r = opts.get("radius").and_then(Value::as_u64).unwrap_or(1).clamp(1, 32) as u32;
    transform(h, move |img| {
        let out = imageproc::filter::median_filter(&img.to_rgba8(), r, r);
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

/// Pixelate / mosaic: shrink by `block` then nearest-upscale back. opts:
/// block (default 8).
fn op_img_pixelate(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let block = opts.get("block").and_then(Value::as_u64).unwrap_or(8).clamp(2, 256) as u32;
    transform(h, move |img| {
        use image::imageops::FilterType::Nearest;
        let (w, hgt) = (img.width(), img.height());
        let small = img.resize_exact((w / block).max(1), (hgt / block).max(1), Nearest);
        Ok(small.resize_exact(w, hgt, Nearest))
    })?;
    Ok(json!({"ok": true}))
}

/// Radial vignette darkening toward the edges. opts: strength 0..1 (default
/// 0.6).
fn op_img_vignette(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let strength = opts.get("strength").and_then(Value::as_f64).unwrap_or(0.6).clamp(0.0, 1.0);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let (w, hgt) = (buf.width() as f64, buf.height() as f64);
        let (cx, cy) = (w / 2.0, hgt / 2.0);
        let maxd = (cx * cx + cy * cy).sqrt();
        for (x, y, px) in buf.enumerate_pixels_mut() {
            let d = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt() / maxd;
            let f = 1.0 - strength * d * d;
            for c in 0..3 {
                px.0[c] = (px.0[c] as f64 * f).clamp(0.0, 255.0) as u8;
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Scale the alpha channel by `factor` 0..1 (semi-transparency).
fn op_img_opacity(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let f = opts.get("factor").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
    pixel_map(h, move |[r, g, b, a]| [r, g, b, (a as f64 * f) as u8])
}

/// Set a constant alpha for every pixel (0..255).
fn op_img_putalpha(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let a = opts.get("alpha").and_then(Value::as_u64).unwrap_or(255).min(255) as u8;
    pixel_map(h, move |[r, g, b, _]| [r, g, b, a])
}

/// Linear cross-fade between the base handle and `src` by `alpha` 0..1
/// (`src` is resized to the base size). out = base*(1-a) + src*a.
fn op_img_blend(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let src = req_u64_img(&opts, "src")?;
    let alpha = opts.get("alpha").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.0, 1.0);
    let src_img = rgba_of(src)?;
    transform(h, move |img| {
        let base = img.to_rgba8();
        let s = image::imageops::resize(&src_img, base.width(), base.height(), image::imageops::FilterType::Triangle);
        let mut out = base.clone();
        for (p, q) in out.pixels_mut().zip(s.pixels()) {
            for c in 0..4 {
                p.0[c] = (p.0[c] as f64 * (1.0 - alpha) + q.0[c] as f64 * alpha).round() as u8;
            }
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

/// Blend two handles with a Photoshop-style `mode`: multiply, screen,
/// overlay, darken, lighten, difference, add, subtract. `src` resized to base.
fn op_img_blend_mode(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let src = req_u64_img(&opts, "src")?;
    let mode = opts.get("mode").and_then(Value::as_str).unwrap_or("multiply").to_string();
    let src_img = rgba_of(src)?;
    transform(h, move |img| {
        let base = img.to_rgba8();
        let s = image::imageops::resize(&src_img, base.width(), base.height(), image::imageops::FilterType::Triangle);
        let mut out = base.clone();
        let f = |a: u8, b: u8| -> u8 {
            let (x, y) = (a as f64 / 255.0, b as f64 / 255.0);
            let v = match mode.as_str() {
                "screen" => 1.0 - (1.0 - x) * (1.0 - y),
                "overlay" => if x < 0.5 { 2.0 * x * y } else { 1.0 - 2.0 * (1.0 - x) * (1.0 - y) },
                "darken" => x.min(y),
                "lighten" => x.max(y),
                "difference" => (x - y).abs(),
                "add" => x + y,
                "subtract" => x - y,
                _ => x * y, // multiply
            };
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        };
        for (p, q) in out.pixels_mut().zip(s.pixels()) {
            for c in 0..3 {
                p.0[c] = f(p.0[c], q.0[c]);
            }
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

/// Composite `src` over the base through a grayscale `mask` handle (white =
/// keep src, black = keep base). All resized to base size.
fn op_img_composite(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let src = req_u64_img(&opts, "src")?;
    let mask = req_u64_img(&opts, "mask")?;
    let src_img = rgba_of(src)?;
    let mask_img = rgba_of(mask)?;
    transform(h, move |img| {
        let base = img.to_rgba8();
        let (w, hgt) = (base.width(), base.height());
        let s = image::imageops::resize(&src_img, w, hgt, image::imageops::FilterType::Triangle);
        let m = image::imageops::resize(&mask_img, w, hgt, image::imageops::FilterType::Triangle);
        let mut out = base.clone();
        for (i, p) in out.pixels_mut().enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            let mp = m.get_pixel(x, y).0;
            let t = luma601(mp[0], mp[1], mp[2]) as f64 / 255.0;
            let q = s.get_pixel(x, y).0;
            for c in 0..4 {
                p.0[c] = (p.0[c] as f64 * (1.0 - t) + q[c] as f64 * t).round() as u8;
            }
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

/// Add a solid-color border of `size` px on every side (grows the canvas).
/// opts: size (default 10), color (default opaque black).
fn op_img_border(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let sz = opts.get("size").and_then(Value::as_u64).unwrap_or(10) as u32;
    let color = parse_color(opts.get("color"));
    transform(h, move |img| {
        let src = img.to_rgba8();
        let (w, hgt) = (src.width(), src.height());
        let mut out = image::RgbaImage::from_pixel(w + 2 * sz, hgt + 2 * sz, color);
        image::imageops::overlay(&mut out, &src, sz as i64, sz as i64);
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}

/// Autocrop a uniform border matching the top-left pixel within `tolerance`
/// (default 0). Returns the new geometry.
fn op_img_trim(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let tol = opts.get("tolerance").and_then(Value::as_u64).unwrap_or(0) as i32;
    transform(h, move |img| {
        let buf = img.to_rgba8();
        let (w, hgt) = (buf.width(), buf.height());
        if w == 0 || hgt == 0 {
            return Ok(DynamicImage::ImageRgba8(buf));
        }
        let bg = buf.get_pixel(0, 0).0;
        let matches = |p: &[u8; 4]| (0..4).all(|c| (p[c] as i32 - bg[c] as i32).abs() <= tol);
        let (mut minx, mut miny, mut maxx, mut maxy) = (w, hgt, 0u32, 0u32);
        let mut any = false;
        for (x, y, px) in buf.enumerate_pixels() {
            if !matches(&px.0) {
                any = true;
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
            }
        }
        if !any {
            return Ok(DynamicImage::ImageRgba8(buf));
        }
        let cropped = image::imageops::crop_imm(&buf, minx, miny, maxx - minx + 1, maxy - miny + 1).to_image();
        Ok(DynamicImage::ImageRgba8(cropped))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}

/// Transpose across the main diagonal (out[y,x] = in[x,y]); swaps W/H.
fn op_img_transpose(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| {
        let src = img.to_rgba8();
        let (w, hgt) = (src.width(), src.height());
        let mut out = image::RgbaImage::new(hgt, w);
        for (x, y, px) in src.enumerate_pixels() {
            out.put_pixel(y, x, *px);
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}

/// Transverse across the anti-diagonal; swaps W/H.
fn op_img_transverse(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| {
        let src = img.to_rgba8();
        let (w, hgt) = (src.width(), src.height());
        let mut out = image::RgbaImage::new(hgt, w);
        for (x, y, px) in src.enumerate_pixels() {
            out.put_pixel(hgt - 1 - y, w - 1 - x, *px);
        }
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}

/// Per-channel 256-bin histogram plus a luma histogram.
fn op_img_histogram(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    with_image(h, |img| {
        let buf = img.to_rgba8();
        let (mut r, mut g, mut b, mut l) = ([0u32; 256], [0u32; 256], [0u32; 256], [0u32; 256]);
        for px in buf.pixels() {
            r[px.0[0] as usize] += 1;
            g[px.0[1] as usize] += 1;
            b[px.0[2] as usize] += 1;
            l[luma601(px.0[0], px.0[1], px.0[2]) as usize] += 1;
        }
        Ok(json!({"r": r.to_vec(), "g": g.to_vec(), "b": b.to_vec(), "luma": l.to_vec()}))
    })
}

/// Min/max value of each channel.
fn op_img_extrema(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    with_image(h, |img| {
        let buf = img.to_rgba8();
        let mut lo = [255u8; 4];
        let mut hi = [0u8; 4];
        for px in buf.pixels() {
            for c in 0..4 {
                lo[c] = lo[c].min(px.0[c]);
                hi[c] = hi[c].max(px.0[c]);
            }
        }
        Ok(json!({
            "r": [lo[0], hi[0]], "g": [lo[1], hi[1]],
            "b": [lo[2], hi[2]], "a": [lo[3], hi[3]],
        }))
    })
}

/// Deterministic LCG → uniform f64 in [0,1). Seeded so output is reproducible.
struct Lcg(u64);
impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Add noise. opts: kind "gaussian"|"salt_pepper" (default gaussian),
/// amount (gaussian stddev, default 20; or s&p rate 0..1, default 0.05),
/// seed (default 1).
fn op_img_noise(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let kind = opts.get("kind").and_then(Value::as_str).unwrap_or("gaussian").to_string();
    let amount = opts.get("amount").and_then(Value::as_f64);
    let seed = opts.get("seed").and_then(Value::as_u64).unwrap_or(1);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let mut rng = Lcg(seed.wrapping_add(0x9E3779B97F4A7C15));
        if kind == "salt_pepper" {
            let rate = amount.unwrap_or(0.05).clamp(0.0, 1.0);
            for px in buf.pixels_mut() {
                if rng.next_f64() < rate {
                    let v = if rng.next_f64() < 0.5 { 0 } else { 255 };
                    px.0[0] = v;
                    px.0[1] = v;
                    px.0[2] = v;
                }
            }
        } else {
            let std = amount.unwrap_or(20.0);
            for px in buf.pixels_mut() {
                // Box–Muller for a normal sample.
                let (u1, u2) = (rng.next_f64().max(1e-12), rng.next_f64());
                let n = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos() * std;
                for c in 0..3 {
                    px.0[c] = (px.0[c] as f64 + n).clamp(0.0, 255.0) as u8;
                }
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Tile semi-transparent diagonal watermark text across the image. opts:
/// text (required), color (default white), size (default 28), opacity 0..1
/// (default 0.25), gap (default 220), font?.
fn op_img_watermark(opts: Value) -> Result<Value> {
    use ab_glyph::{FontRef, PxScale};
    use imageproc::drawing::draw_text_mut;
    let h = req_u64_img(&opts, "handle")?;
    let text = req_str(&opts, "text")?.to_string();
    let color = parse_color(opts.get("color").or(Some(&Value::String("#ffffff".into()))));
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(28.0) as f32;
    let opacity = opts.get("opacity").and_then(Value::as_f64).unwrap_or(0.25).clamp(0.0, 1.0);
    let gap = opts.get("gap").and_then(Value::as_u64).unwrap_or(220).max(40) as i32;
    let font_bytes: Vec<u8> = match opts.get("font").and_then(Value::as_str) {
        Some(p) => std::fs::read(p).map_err(|e| anyhow!("font {p}: {e}"))?,
        None => FONT_BYTES.to_vec(),
    };
    transform(h, move |img| {
        let mut base = img.to_rgba8();
        let (w, hgt) = (base.width() as i32, base.height() as i32);
        // Draw onto a transparent layer, then alpha-composite at `opacity`.
        let mut layer = image::RgbaImage::new(base.width(), base.height());
        let font = FontRef::try_from_slice(&font_bytes).map_err(|_| anyhow!("invalid font"))?;
        let scale = PxScale::from(size);
        let mut row = 0;
        let mut y = 0;
        while y < hgt {
            let xoff = if row % 2 == 0 { 0 } else { gap / 2 };
            let mut x = -xoff;
            while x < w {
                draw_text_mut(&mut layer, color, x, y, scale, &font, &text);
                x += gap;
            }
            y += gap / 2;
            row += 1;
        }
        for (p, q) in base.pixels_mut().zip(layer.pixels()) {
            let a = q.0[3] as f64 / 255.0 * opacity;
            for c in 0..3 {
                p.0[c] = (p.0[c] as f64 * (1.0 - a) + q.0[c] as f64 * a).round() as u8;
            }
        }
        Ok(DynamicImage::ImageRgba8(base))
    })?;
    Ok(json!({"ok": true}))
}

/// Split into channel images. Returns `{handles: {r,g,b,a}}`, each a grayscale
/// (L) image of that channel.
fn op_img_split(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let buf = rgba_of(h)?;
    let (w, hgt) = (buf.width(), buf.height());
    let mut chans = [(); 4].map(|_| image::GrayImage::new(w, hgt));
    for (x, y, px) in buf.enumerate_pixels() {
        for c in 0..4 {
            chans[c].put_pixel(x, y, image::Luma([px.0[c]]));
        }
    }
    let [r, g, b, a] = chans;
    Ok(json!({"handles": {
        "r": insert_image(DynamicImage::ImageLuma8(r)),
        "g": insert_image(DynamicImage::ImageLuma8(g)),
        "b": insert_image(DynamicImage::ImageLuma8(b)),
        "a": insert_image(DynamicImage::ImageLuma8(a)),
    }}))
}

/// Merge grayscale channel handles into one RGB(A) image. opts: r, g, b
/// (required handles), a? (optional). Returns a new image handle.
fn op_img_merge(opts: Value) -> Result<Value> {
    let r = rgba_of(req_u64_img(&opts, "r")?)?;
    let g = rgba_of(req_u64_img(&opts, "g")?)?;
    let b = rgba_of(req_u64_img(&opts, "b")?)?;
    let a = opts.get("a").and_then(Value::as_u64).map(rgba_of).transpose()?;
    let (w, hgt) = (r.width(), r.height());
    let mut out = image::RgbaImage::new(w, hgt);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let av = a.as_ref().map(|m| m.get_pixel(x, y).0[0]).unwrap_or(255);
        *px = image::Rgba([
            r.get_pixel(x, y).0[0],
            g.get_pixel(x.min(g.width() - 1), y.min(g.height() - 1)).0[0],
            b.get_pixel(x.min(b.width() - 1), y.min(b.height() - 1)).0[0],
            av,
        ]);
    }
    let handle = insert_image(DynamicImage::ImageRgba8(out));
    with_image(handle, |img| Ok(info_json(handle, img)))
}

/// 3x3 grayscale-style morphology (per RGB channel). `grow` = dilate (max),
/// else erode (min).
fn morph3(buf: &image::RgbaImage, grow: bool) -> image::RgbaImage {
    let (w, hgt) = (buf.width() as i32, buf.height() as i32);
    let mut out = buf.clone();
    for y in 0..hgt {
        for x in 0..w {
            let mut v = if grow { [0u8; 3] } else { [255u8; 3] };
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let sx = (x + dx).clamp(0, w - 1) as u32;
                    let sy = (y + dy).clamp(0, hgt - 1) as u32;
                    let p = buf.get_pixel(sx, sy).0;
                    for c in 0..3 {
                        v[c] = if grow { v[c].max(p[c]) } else { v[c].min(p[c]) };
                    }
                }
            }
            let px = out.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                px.0[c] = v[c];
            }
        }
    }
    out
}

/// Morphological dilate, repeated `iterations` times (default 1).
fn op_img_dilate(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let it = opts.get("iterations").and_then(Value::as_u64).unwrap_or(1).clamp(1, 16);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        for _ in 0..it {
            buf = morph3(&buf, true);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Morphological erode, repeated `iterations` times (default 1).
fn op_img_erode(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let it = opts.get("iterations").and_then(Value::as_u64).unwrap_or(1).clamp(1, 16);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        for _ in 0..it {
            buf = morph3(&buf, false);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

// ── animation, advanced drawing, transforms, byte I/O ────────────────────────

/// Open an animated image (gif/webp) and split it into per-frame handles.
/// Returns `{ frames => [{handle, width, height, delay_ms}], count }`. A
/// non-animated path comes back as a single frame.
fn op_img_open_frames(opts: Value) -> Result<Value> {
    use image::AnimationDecoder;
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path).map_err(|e| anyhow!("open {path}: {e}"))?;
    let frames: Vec<image::Frame> = match ext_of(path).as_str() {
        "gif" => image::codecs::gif::GifDecoder::new(Cursor::new(&bytes))
            .map_err(|e| anyhow!("gif decode: {e}"))?
            .into_frames()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow!("gif frames: {e}"))?,
        "webp" => image::codecs::webp::WebPDecoder::new(Cursor::new(&bytes))
            .map_err(|e| anyhow!("webp decode: {e}"))?
            .into_frames()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow!("webp frames: {e}"))?,
        _ => {
            // Static image: one frame.
            let img = image::load_from_memory(&bytes).map_err(|e| anyhow!("decode {path}: {e}"))?;
            let h = insert_image(img);
            return with_image(h, |img| {
                Ok(json!({"count": 1, "frames": [{
                    "handle": h, "width": img.width(), "height": img.height(), "delay_ms": 0
                }]}))
            });
        }
    };
    let mut out = Vec::with_capacity(frames.len());
    for f in frames {
        let (n, d) = f.delay().numer_denom_ms();
        let delay_ms = if d == 0 { 0.0 } else { n as f64 / d as f64 };
        let (w, hgt) = (f.buffer().width(), f.buffer().height());
        let handle = insert_image(DynamicImage::ImageRgba8(f.into_buffer()));
        out.push(json!({"handle": handle, "width": w, "height": hgt, "delay_ms": delay_ms}));
    }
    Ok(json!({"count": out.len(), "frames": out}))
}

/// Write an animated GIF from a list of image handles (resized to the first
/// frame's size). opts: delay (ms, default 100), delays => [ms,...] per frame,
/// repeat => "infinite" (default) or an integer loop count.
fn op_img_save_animated(opts: Value) -> Result<Value> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame};
    let path = req_str(&opts, "path")?.to_string();
    let handles: Vec<u64> = opts
        .get("handles")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing handles (expected array)"))?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    if handles.is_empty() {
        return Err(anyhow!("no frames to write"));
    }
    let default_delay = opts.get("delay").and_then(Value::as_u64).unwrap_or(100);
    let per: Vec<u64> = opts
        .get("delays")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let first = rgba_of(handles[0])?;
    let (w, hgt) = (first.width(), first.height());
    let file = std::fs::File::create(&path)?;
    let mut enc = GifEncoder::new(file);
    let repeat = match opts.get("repeat") {
        Some(Value::Number(n)) => Repeat::Finite(n.as_u64().unwrap_or(0) as u16),
        _ => Repeat::Infinite,
    };
    enc.set_repeat(repeat).map_err(|e| anyhow!("gif repeat: {e}"))?;
    for (i, &h) in handles.iter().enumerate() {
        let mut rgba = rgba_of(h)?;
        if rgba.width() != w || rgba.height() != hgt {
            rgba = image::imageops::resize(&rgba, w, hgt, image::imageops::FilterType::Triangle);
        }
        let ms = per.get(i).copied().unwrap_or(default_delay);
        let frame = Frame::from_parts(rgba, 0, 0, Delay::from_numer_denom_ms(ms as u32, 1));
        enc.encode_frame(frame).map_err(|e| anyhow!("encode frame {i}: {e}"))?;
    }
    Ok(json!({"ok": true, "path": path, "frames": handles.len()}))
}

/// Combine handles into a grid montage. opts: cols (default ceil(sqrt(n))),
/// gap (default 4), bg (default opaque white). Returns a new image handle.
fn op_img_montage(opts: Value) -> Result<Value> {
    let handles: Vec<u64> = opts
        .get("handles")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing handles (expected array)"))?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    if handles.is_empty() {
        return Err(anyhow!("no images to montage"));
    }
    let imgs: Vec<image::RgbaImage> = handles.iter().map(|&h| rgba_of(h)).collect::<Result<_>>()?;
    let n = imgs.len();
    let cols = opts.get("cols").and_then(Value::as_u64).map(|c| c as usize).unwrap_or_else(|| (n as f64).sqrt().ceil() as usize).max(1);
    let rows = n.div_ceil(cols);
    let gap = opts.get("gap").and_then(Value::as_u64).unwrap_or(4) as u32;
    let bg = parse_color(opts.get("bg").or(Some(&Value::String("#ffffff".into()))));
    let cell_w = imgs.iter().map(|i| i.width()).max().unwrap_or(1);
    let cell_h = imgs.iter().map(|i| i.height()).max().unwrap_or(1);
    let total_w = cols as u32 * cell_w + (cols as u32 + 1) * gap;
    let total_h = rows as u32 * cell_h + (rows as u32 + 1) * gap;
    let mut canvas = image::RgbaImage::from_pixel(total_w, total_h, bg);
    for (i, im) in imgs.iter().enumerate() {
        let (cr, cc) = (i / cols, i % cols);
        let x = gap + cc as u32 * (cell_w + gap);
        let y = gap + cr as u32 * (cell_h + gap);
        image::imageops::overlay(&mut canvas, im, x as i64, y as i64);
    }
    let handle = insert_image(DynamicImage::ImageRgba8(canvas));
    with_image(handle, |img| Ok(info_json(handle, img)))
}

/// Fill the image with a gradient. opts: kind => "linear"|"radial"
/// (default linear), from (color), to (color), angle (deg, linear only,
/// default 0 = left→right).
fn op_img_gradient(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let kind = opts.get("kind").and_then(Value::as_str).unwrap_or("linear").to_string();
    let from = parse_color(opts.get("from"));
    let to = parse_color(opts.get("to").or(Some(&Value::String("#ffffff".into()))));
    let angle = opts.get("angle").and_then(Value::as_f64).unwrap_or(0.0).to_radians();
    transform(h, move |img| {
        let (w, hgt) = (img.width(), img.height());
        let mut buf = image::RgbaImage::new(w, hgt);
        let (cx, cy) = (w as f64 / 2.0, hgt as f64 / 2.0);
        let maxd = (cx * cx + cy * cy).sqrt().max(1.0);
        let (dx, dy) = (angle.cos(), angle.sin());
        let lerp = |t: f64, c: usize| (from.0[c] as f64 + (to.0[c] as f64 - from.0[c] as f64) * t).clamp(0.0, 255.0) as u8;
        for (x, y, px) in buf.enumerate_pixels_mut() {
            let t = if kind == "radial" {
                (((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt() / maxd).clamp(0.0, 1.0)
            } else {
                // projection of (x,y) onto the gradient direction, normalized
                let proj = (x as f64 * dx + y as f64 * dy) / ((w as f64 - 1.0).max(1.0) * dx.abs() + (hgt as f64 - 1.0).max(1.0) * dy.abs()).max(1.0);
                proj.clamp(0.0, 1.0)
            };
            *px = image::Rgba([lerp(t, 0), lerp(t, 1), lerp(t, 2), lerp(t, 3)]);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Draw an axis-aligned ellipse centered at ($x,$y) with radii ($rx,$ry).
/// opts: fill => 1 (default) or 0.
fn op_img_draw_ellipse(opts: Value) -> Result<Value> {
    use imageproc::drawing::{draw_filled_ellipse_mut, draw_hollow_ellipse_mut};
    let h = req_u64_img(&opts, "handle")?;
    let x = opt_i64(&opts, "x", 0) as i32;
    let y = opt_i64(&opts, "y", 0) as i32;
    let rx = req_u64_img(&opts, "rx")? as i32;
    let ry = req_u64_img(&opts, "ry")? as i32;
    let color = parse_color(opts.get("color"));
    let fill = opts.get("fill").and_then(Value::as_bool).unwrap_or(true);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        if fill {
            draw_filled_ellipse_mut(&mut buf, (x, y), rx, ry, color);
        } else {
            draw_hollow_ellipse_mut(&mut buf, (x, y), rx, ry, color);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Draw a filled polygon from $points (`[[x,y],...]`).
fn op_img_draw_polygon(opts: Value) -> Result<Value> {
    use imageproc::drawing::draw_polygon_mut;
    use imageproc::point::Point;
    let h = req_u64_img(&opts, "handle")?;
    let color = parse_color(opts.get("color"));
    let pts: Vec<Point<i32>> = opts
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing points (expected [[x,y],...])"))?
        .iter()
        .filter_map(|p| {
            let a = p.as_array()?;
            Some(Point::new(a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32))
        })
        .collect();
    if pts.len() < 3 {
        return Err(anyhow!("polygon needs at least 3 points"));
    }
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let mut p = pts;
        if p.first() == p.last() {
            p.pop(); // draw_polygon_mut auto-closes; reject duplicate end point
        }
        draw_polygon_mut(&mut buf, &p, color);
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Draw multi-line text (splits on "\n"). opts: size (16), line_height
/// (default 1.2 * size), font.
fn op_img_draw_text_multiline(opts: Value) -> Result<Value> {
    use ab_glyph::{FontRef, PxScale};
    use imageproc::drawing::draw_text_mut;
    let h = req_u64_img(&opts, "handle")?;
    let x = opt_i64(&opts, "x", 0) as i32;
    let y = opt_i64(&opts, "y", 0) as i32;
    let text = req_str(&opts, "text")?.to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(16.0) as f32;
    let line_h = opts.get("line_height").and_then(Value::as_f64).unwrap_or((size * 1.2) as f64) as i32;
    let color = parse_color(opts.get("color"));
    let font_bytes: Vec<u8> = match opts.get("font").and_then(Value::as_str) {
        Some(p) => std::fs::read(p).map_err(|e| anyhow!("font {p}: {e}"))?,
        None => FONT_BYTES.to_vec(),
    };
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let font = FontRef::try_from_slice(&font_bytes).map_err(|_| anyhow!("invalid font"))?;
        let scale = PxScale::from(size);
        for (i, line) in text.split('\n').enumerate() {
            draw_text_mut(&mut buf, color, x, y + i as i32 * line_h, scale, &font, line);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Apply a 3x3 projective transform (`matrix` = 9 row-major numbers): covers
/// affine + perspective warps. Out-of-bounds fills transparent.
fn op_img_warp(opts: Value) -> Result<Value> {
    use imageproc::geometric_transformations::{warp, Border, Interpolation, Projection};
    let h = req_u64_img(&opts, "handle")?;
    let arr = opts
        .get("matrix")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing matrix (expected 9 numbers)"))?;
    if arr.len() < 9 {
        return Err(anyhow!("matrix must have 9 numbers"));
    }
    let mut m = [0.0f32; 9];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = arr[i].as_f64().unwrap_or(0.0) as f32;
    }
    let proj = Projection::from_matrix(m).ok_or_else(|| anyhow!("matrix is not invertible"))?;
    transform(h, move |img| {
        let rgba = img.to_rgba8();
        let out = warp(&rgba, proj, Interpolation::Bilinear, Border::Constant(image::Rgba([0, 0, 0, 0])));
        Ok(DynamicImage::ImageRgba8(out))
    })?;
    Ok(json!({"ok": true}))
}

// Standard base64 alphabet (RFC 4648) for image byte I/O.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let mut rev = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let clean: Vec<u8> = s.bytes().filter(|&c| c != b'=' && !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = rev[c as usize];
            if v == 255 {
                return Err(anyhow!("invalid base64 character"));
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        // emit the high bytes that are fully populated
        let nbytes = (bits / 8) as usize;
        let shifted = acc << (24 - bits);
        for b in 0..nbytes {
            out.push((shifted >> (16 - b * 8)) as u8);
        }
    }
    Ok(out)
}

/// Encode an image handle to a base64 string. opts: format (default "png").
/// Returns `{ base64, format, bytes }`.
fn op_img_to_base64(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let fmt = opts.get("format").and_then(Value::as_str).unwrap_or("png").to_ascii_lowercase();
    let format = match fmt.as_str() {
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "gif" => image::ImageFormat::Gif,
        "bmp" => image::ImageFormat::Bmp,
        "webp" => image::ImageFormat::WebP,
        "tif" | "tiff" => image::ImageFormat::Tiff,
        _ => image::ImageFormat::Png,
    };
    with_image(h, |img| {
        let mut buf = Cursor::new(Vec::new());
        let to_write = if matches!(format, image::ImageFormat::Jpeg) {
            DynamicImage::ImageRgb8(img.to_rgb8())
        } else {
            img.clone()
        };
        to_write.write_to(&mut buf, format).map_err(|e| anyhow!("encode {fmt}: {e}"))?;
        let bytes = buf.into_inner();
        Ok(json!({"base64": base64_encode(&bytes), "format": fmt, "bytes": bytes.len()}))
    })
}

/// Decode a base64 string into a new image handle. Returns image info.
fn op_img_from_base64(opts: Value) -> Result<Value> {
    let b64 = req_str(&opts, "base64")?;
    let bytes = base64_decode(b64)?;
    let img = image::load_from_memory(&bytes).map_err(|e| anyhow!("decode base64 image: {e}"))?;
    let handle = insert_image(img);
    with_image(handle, |img| Ok(info_json(handle, img)))
}

// ── shapes, fills, masks, color analysis ─────────────────────────────────────

/// Is point (px,py) inside the rounded rect [x,x+w)×[y,y+h) with corner `r`?
fn in_rrect(px: i64, py: i64, x: i64, y: i64, w: i64, h: i64, r: i64) -> bool {
    if px < x || py < y || px >= x + w || py >= y + h {
        return false;
    }
    let r = r.min(w / 2).min(h / 2).max(0);
    // corner centers
    let (lx, rx) = (x + r, x + w - 1 - r);
    let (ty, by) = (y + r, y + h - 1 - r);
    let cx = if px < lx { lx } else if px > rx { rx } else { px };
    let cy = if py < ty { ty } else if py > by { by } else { py };
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

/// Draw a rounded rectangle. opts: fill => 1 (default) or 0 (outline,
/// `stroke` px wide, default 2).
fn op_img_draw_rounded_rect(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let x = opt_i64(&opts, "x", 0);
    let y = opt_i64(&opts, "y", 0);
    let w = req_u64_img(&opts, "width")? as i64;
    let ht = req_u64_img(&opts, "height")? as i64;
    let r = opts.get("radius").and_then(Value::as_i64).unwrap_or(8);
    let color = parse_color(opts.get("color"));
    let fill = opts.get("fill").and_then(Value::as_bool).unwrap_or(true);
    let stroke = opts.get("stroke").and_then(Value::as_i64).unwrap_or(2).max(1);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let (iw, ih) = (buf.width() as i64, buf.height() as i64);
        for py in y.max(0)..(y + ht).min(ih) {
            for px in x.max(0)..(x + w).min(iw) {
                let inside = in_rrect(px, py, x, y, w, ht, r);
                let draw = if fill {
                    inside
                } else {
                    inside && !in_rrect(px, py, x + stroke, y + stroke, w - 2 * stroke, ht - 2 * stroke, (r - stroke).max(0))
                };
                if draw {
                    buf.put_pixel(px as u32, py as u32, color);
                }
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Draw an open polyline through $points (`[[x,y],...]`).
fn op_img_draw_polyline(opts: Value) -> Result<Value> {
    use imageproc::drawing::draw_line_segment_mut;
    let h = req_u64_img(&opts, "handle")?;
    let color = parse_color(opts.get("color"));
    let pts: Vec<(f32, f32)> = opts
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing points"))?
        .iter()
        .filter_map(|p| {
            let a = p.as_array()?;
            Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32))
        })
        .collect();
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        for w in pts.windows(2) {
            draw_line_segment_mut(&mut buf, w[0], w[1], color);
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Draw a circular arc from `start` to `end` degrees. opts: fill => 1 draws a
/// wedge; otherwise a stroked arc.
fn op_img_draw_arc(opts: Value) -> Result<Value> {
    use imageproc::drawing::{draw_line_segment_mut, draw_polygon_mut};
    use imageproc::point::Point;
    let h = req_u64_img(&opts, "handle")?;
    let cx = opt_i64(&opts, "x", 0) as i32;
    let cy = opt_i64(&opts, "y", 0) as i32;
    let r = req_u64_img(&opts, "radius")? as f64;
    let a0 = opts.get("start").and_then(Value::as_f64).unwrap_or(0.0).to_radians();
    let a1 = opts.get("end").and_then(Value::as_f64).unwrap_or(90.0).to_radians();
    let color = parse_color(opts.get("color"));
    let fill = opts.get("fill").and_then(Value::as_bool).unwrap_or(false);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let steps = (((a1 - a0).abs() / 0.05).ceil().max(2.0)) as usize;
        let pt = |a: f64| (cx as f64 + r * a.cos(), cy as f64 + r * a.sin());
        if fill {
            let mut poly: Vec<Point<i32>> = vec![Point::new(cx, cy)];
            for k in 0..=steps {
                let (x, y) = pt(a0 + (a1 - a0) * k as f64 / steps as f64);
                poly.push(Point::new(x as i32, y as i32));
            }
            poly.dedup();
            if poly.len() >= 3 {
                draw_polygon_mut(&mut buf, &poly, color);
            }
        } else {
            for k in 0..steps {
                let p0 = pt(a0 + (a1 - a0) * k as f64 / steps as f64);
                let p1 = pt(a0 + (a1 - a0) * (k + 1) as f64 / steps as f64);
                draw_line_segment_mut(&mut buf, (p0.0 as f32, p0.1 as f32), (p1.0 as f32, p1.1 as f32), color);
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Bucket flood fill from a seed pixel. opts: tolerance (default 0).
fn op_img_flood_fill(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let sx = req_u64_img(&opts, "x")? as i64;
    let sy = req_u64_img(&opts, "y")? as i64;
    let color = parse_color(opts.get("color"));
    let tol = opts.get("tolerance").and_then(Value::as_i64).unwrap_or(0);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let (w, ht) = (buf.width() as i64, buf.height() as i64);
        if sx < 0 || sy < 0 || sx >= w || sy >= ht {
            return Ok(DynamicImage::ImageRgba8(buf));
        }
        let target = buf.get_pixel(sx as u32, sy as u32).0;
        let matches = |p: &[u8; 4]| (0..4).all(|c| (p[c] as i64 - target[c] as i64).abs() <= tol);
        if color.0 == target {
            return Ok(DynamicImage::ImageRgba8(buf));
        }
        let mut stack = vec![(sx, sy)];
        while let Some((x, y)) = stack.pop() {
            if x < 0 || y < 0 || x >= w || y >= ht {
                continue;
            }
            let px = buf.get_pixel_mut(x as u32, y as u32);
            if !matches(&px.0) {
                continue;
            }
            *px = color;
            stack.push((x + 1, y));
            stack.push((x - 1, y));
            stack.push((x, y + 1));
            stack.push((x, y - 1));
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Replace every pixel near $from with $to. opts: tolerance (default 16).
fn op_img_replace_color(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let from = parse_color(opts.get("from"));
    let to = parse_color(opts.get("to"));
    let tol = opts.get("tolerance").and_then(Value::as_i64).unwrap_or(16);
    pixel_map(h, move |p| {
        if (0..3).all(|c| (p[c] as i64 - from.0[c] as i64).abs() <= tol) {
            to.0
        } else {
            p
        }
    })
}

/// Permute channels by an `order` string over r/g/b/a, e.g. "bgr", "grba".
fn op_img_swap_channels(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let order = req_str(&opts, "order")?.to_ascii_lowercase();
    let idx = |c: char| match c {
        'r' => 0usize,
        'g' => 1,
        'b' => 2,
        'a' => 3,
        _ => 0,
    };
    let map: Vec<usize> = order.chars().map(idx).collect();
    pixel_map(h, move |p| {
        let mut out = p;
        for (i, &src) in map.iter().enumerate().take(4) {
            out[i] = p[src];
        }
        out
    })
}

/// Top-K dominant colors via coarse 16-level bucketing. opts: count
/// (default 5). Returns `{ colors => [{r,g,b,hex,count}] }`.
fn op_img_dominant_colors(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let count = opts.get("count").and_then(Value::as_u64).unwrap_or(5).clamp(1, 64) as usize;
    with_image(h, |img| {
        let buf = img.to_rgba8();
        let mut hist: HashMap<u16, u32> = HashMap::new();
        for px in buf.pixels() {
            // 4 bits per channel → 12-bit bucket key
            let key = (((px.0[0] >> 4) as u16) << 8) | (((px.0[1] >> 4) as u16) << 4) | ((px.0[2] >> 4) as u16);
            *hist.entry(key).or_insert(0) += 1;
        }
        let mut v: Vec<(u16, u32)> = hist.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let colors: Vec<Value> = v
            .into_iter()
            .take(count)
            .map(|(key, cnt)| {
                let r = (((key >> 8) & 0xf) as u8) << 4 | 0x8;
                let g = (((key >> 4) & 0xf) as u8) << 4 | 0x8;
                let b = ((key & 0xf) as u8) << 4 | 0x8;
                json!({"r": r, "g": g, "b": b, "hex": format!("#{r:02x}{g:02x}{b:02x}"), "count": cnt})
            })
            .collect();
        Ok(json!({"colors": colors}))
    })
}

/// Compare two image handles (src resized to base). Returns `{mse, rmse,
/// max_diff, identical}`. opt `diff => 1` also returns a `diff_handle`
/// (per-pixel absolute difference).
fn op_img_compare(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let other = req_u64_img(&opts, "other")?;
    let want_diff = opts.get("diff").and_then(Value::as_bool).unwrap_or(false);
    let a = rgba_of(h)?;
    let mut b = rgba_of(other)?;
    if b.width() != a.width() || b.height() != a.height() {
        b = image::imageops::resize(&b, a.width(), a.height(), image::imageops::FilterType::Triangle);
    }
    let mut sum_sq = 0f64;
    let mut max_diff = 0i64;
    let mut diff = want_diff.then(|| image::RgbaImage::new(a.width(), a.height()));
    for (i, (pa, pb)) in a.pixels().zip(b.pixels()).enumerate() {
        let mut dpx = [0u8, 0, 0, 255];
        for c in 0..3 {
            let d = (pa.0[c] as i64 - pb.0[c] as i64).abs();
            sum_sq += (d * d) as f64;
            max_diff = max_diff.max(d);
            dpx[c] = d as u8;
        }
        if let Some(d) = diff.as_mut() {
            d.put_pixel((i as u32) % a.width(), (i as u32) / a.width(), image::Rgba(dpx));
        }
    }
    let n = (a.width() * a.height()) as f64 * 3.0;
    let mse = if n > 0.0 { sum_sq / n } else { 0.0 };
    let mut out = json!({"mse": mse, "rmse": mse.sqrt(), "max_diff": max_diff, "identical": max_diff == 0});
    if let Some(d) = diff {
        out["diff_handle"] = json!(insert_image(DynamicImage::ImageRgba8(d)));
    }
    Ok(out)
}

/// Measure rendered text. opts: size (16), font. Returns `{width, height}`.
fn op_img_text_size(opts: Value) -> Result<Value> {
    use ab_glyph::{FontRef, PxScale};
    let text = req_str(&opts, "text")?;
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(16.0) as f32;
    let font_bytes: Vec<u8> = match opts.get("font").and_then(Value::as_str) {
        Some(p) => std::fs::read(p).map_err(|e| anyhow!("font {p}: {e}"))?,
        None => FONT_BYTES.to_vec(),
    };
    let font = FontRef::try_from_slice(&font_bytes).map_err(|_| anyhow!("invalid font"))?;
    let (w, h) = imageproc::drawing::text_size(PxScale::from(size), &font, text);
    Ok(json!({"width": w, "height": h}))
}

/// Mask to the inscribed circle (transparent outside).
fn op_img_crop_circle(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    transform(h, |img| {
        let mut buf = img.to_rgba8();
        let (w, ht) = (buf.width() as f64, buf.height() as f64);
        let (cx, cy) = (w / 2.0, ht / 2.0);
        let r = cx.min(cy);
        for (x, y, px) in buf.enumerate_pixels_mut() {
            let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
            if d > r {
                px.0[3] = 0;
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Round the image corners (transparent outside the rounded rect).
fn op_img_round_corners(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let r = opts.get("radius").and_then(Value::as_i64).unwrap_or(16);
    transform(h, move |img| {
        let mut buf = img.to_rgba8();
        let (w, ht) = (buf.width() as i64, buf.height() as i64);
        for y in 0..ht {
            for x in 0..w {
                if !in_rrect(x, y, 0, 0, w, ht, r) {
                    buf.get_pixel_mut(x as u32, y as u32).0[3] = 0;
                }
            }
        }
        Ok(DynamicImage::ImageRgba8(buf))
    })?;
    Ok(json!({"ok": true}))
}

/// Add a soft drop shadow (grows the canvas). opts: dx (6), dy (6), blur
/// sigma (4), color (black), opacity 0..1 (0.5).
fn op_img_drop_shadow(opts: Value) -> Result<Value> {
    let h = req_u64_img(&opts, "handle")?;
    let dx = opts.get("dx").and_then(Value::as_i64).unwrap_or(6);
    let dy = opts.get("dy").and_then(Value::as_i64).unwrap_or(6);
    let sigma = opts.get("blur").and_then(Value::as_f64).unwrap_or(4.0) as f32;
    let sc = parse_color(opts.get("color"));
    let opacity = opts.get("opacity").and_then(Value::as_f64).unwrap_or(0.5).clamp(0.0, 1.0);
    transform(h, move |img| {
        let src = img.to_rgba8();
        let (w, ht) = (src.width(), src.height());
        let pad = (sigma as i64 * 3 + dx.abs().max(dy.abs())).max(1) as u32;
        let cw = w + 2 * pad;
        let ch = ht + 2 * pad;
        // shadow silhouette from src alpha, tinted, then blurred
        let mut shadow = image::RgbaImage::new(cw, ch);
        for (x, y, px) in src.enumerate_pixels() {
            if px.0[3] > 0 {
                let a = (px.0[3] as f64 * opacity) as u8;
                let tx = x as i64 + pad as i64 + dx;
                let ty = y as i64 + pad as i64 + dy;
                if tx >= 0 && ty >= 0 && (tx as u32) < cw && (ty as u32) < ch {
                    shadow.put_pixel(tx as u32, ty as u32, image::Rgba([sc.0[0], sc.0[1], sc.0[2], a]));
                }
            }
        }
        let blurred = image::imageops::blur(&shadow, sigma);
        let mut canvas = DynamicImage::ImageRgba8(blurred).to_rgba8();
        image::imageops::overlay(&mut canvas, &src, pad as i64, pad as i64);
        Ok(DynamicImage::ImageRgba8(canvas))
    })?;
    with_image(h, |img| Ok(info_json(h, img)))
}
