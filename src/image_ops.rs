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
