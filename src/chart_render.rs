// Standalone chart rendering — data in, an image handle out.
//
// `office__chart_render` rasterizes a chart (bar/column/line/area/scatter/pie)
// onto a fresh RGBA image using imageproc + the vendored font, then stores it
// in the same image registry the PIL surface uses and returns its handle. The
// caller then `img_save`s it to ANY raster format (png/jpeg/gif/bmp/webp/tiff)
// or further processes it — so "render a chart in whatever format I want" is
// just chart_render + img_save. Fully self-contained: no plotters, no system
// fonts, no external binaries.

use ab_glyph::{FontRef, PxScale};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_line_segment_mut, draw_polygon_mut,
    draw_text_mut,
};
use imageproc::point::Point;
use imageproc::rect::Rect;

const PALETTE: &[&str] = &[
    "#4472C4", "#ED7D31", "#A5A5A5", "#FFC000", "#5B9BD5", "#70AD47", "#264478", "#9E480E",
    "#636363", "#997300",
];

fn palette(i: usize) -> Rgba<u8> {
    parse_color(Some(&Value::String(PALETTE[i % PALETTE.len()].to_string())))
}

fn font() -> FontRef<'static> {
    FontRef::try_from_slice(FONT_BYTES).expect("vendored font is valid")
}

/// Numeric data of a series (`data:[n,...]`), ignoring non-numbers.
fn series_nums(s: &Value) -> Vec<f64> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// Bubble points of a series (`data:[[x,y,size],...]`).
fn series_points3(s: &Value) -> Vec<(f64, f64, f64)> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_f64()?,
                        p.get(1)?.as_f64()?,
                        p.get(2).and_then(Value::as_f64).unwrap_or(6.0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Scatter points of a series (`data:[[x,y],...]`).
fn series_points(s: &Value) -> Vec<(f64, f64)> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn series_color(s: &Value, i: usize) -> Rgba<u8> {
    match s.get("color") {
        Some(c) => parse_color(Some(c)),
        None => palette(i),
    }
}

/// Conditional formatting + data validation declared on a sheet:
/// `conditional:[{range:[r1,c1,r2,c2], rule, value, value2?, format:{…}}]`
/// `validations:[{range:[r1,c1,r2,c2], list:[strings]}]`.
fn write_xlsx_cond_val(ws: &mut rust_xlsxwriter::Worksheet, s: &Value) -> Result<()> {
    use rust_xlsxwriter::{ConditionalFormatCell, ConditionalFormatCellRule, DataValidation};
    if let Some(conds) = s.get("conditional").and_then(Value::as_array) {
        for c in conds {
            let Some(rng) = quad(c, "range") else { continue };
            let v = c.get("value").and_then(Value::as_f64).unwrap_or(0.0);
            let v2 = c.get("value2").and_then(Value::as_f64).unwrap_or(0.0);
            let rule = match c.get("rule").and_then(Value::as_str).unwrap_or("greater_than") {
                "less_than" => ConditionalFormatCellRule::LessThan(v),
                "greater_equal" => ConditionalFormatCellRule::GreaterThanOrEqualTo(v),
                "less_equal" => ConditionalFormatCellRule::LessThanOrEqualTo(v),
                "equal" => ConditionalFormatCellRule::EqualTo(v),
                "not_equal" => ConditionalFormatCellRule::NotEqualTo(v),
                "between" => ConditionalFormatCellRule::Between(v, v2),
                "not_between" => ConditionalFormatCellRule::NotBetween(v, v2),
                _ => ConditionalFormatCellRule::GreaterThan(v),
            };
            let mut cf = ConditionalFormatCell::new().set_rule(rule);
            if let Some(fmt) = c.get("format").and_then(Value::as_object).and_then(xlsx_format) {
                cf = cf.set_format(fmt);
            }
            ws.add_conditional_format(rng[0], rng[1] as u16, rng[2], rng[3] as u16, &cf)?;
        }
    }
    if let Some(vals) = s.get("validations").and_then(Value::as_array) {
        for v in vals {
            let Some(rng) = quad(v, "range") else { continue };
            if let Some(list) = v.get("list").and_then(Value::as_array) {
                let items: Vec<String> = list.iter().map(cell_to_string).collect();
                let dv = DataValidation::new().allow_list_strings(&items)?;
                ws.add_data_validation(rng[0], rng[1] as u16, rng[2], rng[3] as u16, &dv)?;
            }
        }
    }
    Ok(())
}

/// OHLC tuples of a series (`data:[[open,high,low,close],...]`).
fn series_ohlc(s: &Value) -> Vec<[f64; 4]> {
    s.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some([
                        p.first()?.as_f64()?,
                        p.get(1)?.as_f64()?,
                        p.get(2)?.as_f64()?,
                        p.get(3)?.as_f64()?,
                    ])
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Five-number summary (min, q1, median, q3, max) of a sorted-able slice.
fn five_number(data: &[f64]) -> Option<[f64; 5]> {
    if data.is_empty() {
        return None;
    }
    let mut v = data.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = |p: f64| {
        let idx = (p * (v.len() - 1) as f64).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    Some([v[0], q(0.25), q(0.5), q(0.75), v[v.len() - 1]])
}

fn op_chart_render(opts: Value) -> Result<Value> {
    let kind = opts.get("type").and_then(Value::as_str).unwrap_or("bar").to_string();
    let w = opts.get("width").and_then(Value::as_u64).unwrap_or(800).max(120) as u32;
    let h = opts.get("height").and_then(Value::as_u64).unwrap_or(600).max(120) as u32;
    let empty_series: Vec<Value> = Vec::new();
    let series = opts.get("series").and_then(Value::as_array).unwrap_or(&empty_series);
    let cats: Vec<String> = opts
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    let title = opts.get("title").and_then(Value::as_str).unwrap_or("");
    let labels = opts.get("labels").and_then(Value::as_bool).unwrap_or(false);

    let mut img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]));
    let fnt = font();
    let black = Rgba([30, 30, 30, 255]);
    let grid = Rgba([210, 210, 210, 255]);

    // Title.
    if !title.is_empty() {
        draw_text_mut(&mut img, black, (w as i32) / 2 - title.len() as i32 * 5, 8, PxScale::from(22.0), &fnt, title);
    }

    let (l, r, t, b) = (60i32, w as i32 - 24, 44i32, h as i32 - 40);
    let pw = (r - l).max(1) as f64;
    let ph = (b - t).max(1) as f64;

    // Kinds that don't need a `series` array (or use a different shape).
    let needs_series = !matches!(kind.as_str(), "sankey" | "gauge" | "heatmap");

    // Non-cartesian renderers draw into `img` then we fall through to the
    // shared legend/finish path. `false` => handle below in the cartesian arm.
    let mut special = true;
    match kind.as_str() {
        "sankey" => render_sankey(&mut img, &opts, w as f64, h as f64),
        "radar" => {
            require_series(&opts)?;
            render_radar(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "pie" | "donut" | "doughnut" => {
            require_series(&opts)?;
            render_pie(&mut img, &fnt, series, &cats, l, t, r, b, kind != "pie", labels, black)
        }
        "funnel" => {
            require_series(&opts)?;
            render_funnel(&mut img, &fnt, series, &cats, l, t, r, b, labels, black)
        }
        "gauge" => render_gauge(&mut img, &fnt, &opts, l, t, r, b, black),
        "heatmap" => render_heatmap(&mut img, &fnt, &opts, &cats, l, t, r, b, black),
        "treemap" => {
            require_series(&opts)?;
            render_treemap(&mut img, &fnt, series, &cats, l, t, r, b, black)
        }
        "polar" => {
            require_series(&opts)?;
            render_polar(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        "bullet" => {
            require_series(&opts)?;
            render_bullet(&mut img, &fnt, series, l, t, r, b, black, grid)
        }
        "pareto" => {
            require_series(&opts)?;
            render_pareto(&mut img, &fnt, series, &cats, l, t, r, b, black, grid)
        }
        _ => special = false,
    }

    if !special {
        if needs_series {
            require_series(&opts)?;
        }
        // Cartesian: axes + y scale.
        draw_line_segment_mut(&mut img, (l as f32, b as f32), (r as f32, b as f32), black); // x axis
        draw_line_segment_mut(&mut img, (l as f32, t as f32), (l as f32, b as f32), black); // y axis

        let scatter = kind == "scatter" || kind == "bubble";
        let ohlc_like = kind == "ohlc" || kind == "candlestick";
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
        if scatter {
            for s in series {
                for (x, y, _) in series_points3(s) {
                    ymin = ymin.min(y);
                    ymax = ymax.max(y);
                    xmin = xmin.min(x);
                    xmax = xmax.max(x);
                }
            }
        } else if ohlc_like {
            for s in series {
                for o in series_ohlc(s) {
                    ymin = ymin.min(o[2]);
                    ymax = ymax.max(o[1]);
                }
            }
        } else if kind == "waterfall" {
            let mut cum = 0.0;
            ymin = ymin.min(0.0);
            ymax = ymax.max(0.0);
            for v in series.first().map(series_nums).unwrap_or_default() {
                cum += v;
                ymin = ymin.min(cum);
                ymax = ymax.max(cum);
            }
        } else if kind == "stacked_area" {
            let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
            ymin = ymin.min(0.0);
            for ci in 0..ncat {
                let col: f64 = series.iter().map(|s| series_nums(s).get(ci).copied().unwrap_or(0.0)).sum();
                ymax = ymax.max(col);
            }
        } else {
            for s in series {
                for v in series_nums(s) {
                    ymin = ymin.min(v);
                    ymax = ymax.max(v);
                }
            }
            ymin = ymin.min(0.0);
        }
        if !ymin.is_finite() || !ymax.is_finite() || (ymax - ymin).abs() < f64::EPSILON {
            ymax = ymin + 1.0;
        }
        if scatter && (!xmin.is_finite() || (xmax - xmin).abs() < f64::EPSILON) {
            xmax = xmin + 1.0;
        }
        let yp = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * ph) as f32;

        // Y gridlines + tick labels (5 ticks).
        for i in 0..=5 {
            let v = ymin + (ymax - ymin) * (i as f64) / 5.0;
            let y = yp(v);
            draw_line_segment_mut(&mut img, (l as f32, y), (r as f32, y), grid);
            draw_text_mut(&mut img, black, 4, y as i32 - 6, PxScale::from(12.0), &fnt, &format!("{v:.1}"));
        }

        match kind.as_str() {
            "line" => render_line_area(&mut img, series, l, r, b, ymin, ymax, false),
            "area" => render_line_area(&mut img, series, l, r, b, ymin, ymax, true),
            "stacked_area" => render_stacked_area(&mut img, series, l, r, b, ymin, ymax),
            "step" => render_step(&mut img, series, l, r, b, ymin, ymax),
            "scatter" => render_scatter(&mut img, series, l as f64, pw, xmin, xmax, yp),
            "bubble" => render_bubble(&mut img, series, l as f64, pw, xmin, xmax, yp),
            "histogram" => render_histogram(&mut img, &fnt, series, &opts, l, pw, t, b, black),
            "stacked" | "stacked_bar" => {
                render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, true, labels)
            }
            "combo" => render_combo(&mut img, &fnt, series, &cats, l, r, pw, b, &yp, ymin, ymax, black, labels),
            "waterfall" => render_waterfall(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, labels),
            "ohlc" => render_ohlc(&mut img, series, l, pw, b, &yp, false),
            "candlestick" => render_ohlc(&mut img, series, l, pw, b, &yp, true),
            "boxplot" => render_boxplot(&mut img, series, l, pw, b, &yp),
            _ => render_bars(&mut img, &fnt, series, &cats, l, pw, b, &yp, black, false, labels),
        }
        // optional least-squares trendline over scatter points
        if scatter && opts.get("trendline").and_then(Value::as_bool) == Some(true) {
            for (si, s) in series.iter().enumerate() {
                let pts = series_points(s);
                if let Some((m, c)) = linfit(&pts) {
                    let color = series_color(s, si);
                    let x0 = xmin;
                    let x1 = xmax;
                    let px = |x: f64| l as f64 + (x - xmin) / (xmax - xmin) * pw;
                    draw_line_segment_mut(&mut img, (px(x0) as f32, yp(m * x0 + c)), (px(x1) as f32, yp(m * x1 + c)), color);
                }
            }
        }
        // category labels under the x axis for the bar-family
        if matches!(kind.as_str(), "waterfall" | "combo" | "ohlc" | "candlestick" | "boxplot") {
            let n = cats.len();
            let slot = if n > 0 { pw / n as f64 } else { pw };
            for (ci, cat) in cats.iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.25;
                draw_text_mut(&mut img, black, x as i32, b + 6, PxScale::from(12.0), &fnt, cat);
            }
        }
        draw_axis_titles(&mut img, &fnt, &opts, l, r, t, b, black);
    }

    // Shared legend (series names, or pie/funnel categories).
    if opts.get("legend").and_then(Value::as_bool) != Some(false) {
        let entries = legend_entries(&kind, series, &cats);
        draw_legend(&mut img, &fnt, &entries, w as i32, t, black);
    }

    let handle = insert_image(DynamicImage::ImageRgba8(img));
    Ok(json!({"handle": handle, "width": w, "height": h, "type": kind}))
}

/// Error unless an `series` array is present.
fn require_series(opts: &Value) -> Result<()> {
    if opts.get("series").and_then(Value::as_array).is_none() {
        return Err(anyhow!("missing series (expected array)"));
    }
    Ok(())
}

/// (label, color) legend entries: category slices for pie/funnel, otherwise
/// the named series.
fn legend_entries(kind: &str, series: &[Value], cats: &[String]) -> Vec<(String, Rgba<u8>)> {
    if matches!(kind, "pie" | "donut" | "doughnut" | "funnel") {
        cats.iter().enumerate().map(|(i, c)| (c.clone(), palette(i))).collect()
    } else {
        series
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.get("name").and_then(Value::as_str).map(|n| (n.to_string(), series_color(s, i)))
            })
            .collect()
    }
}

/// Top-right legend with color swatches.
fn draw_legend(img: &mut RgbaImage, fnt: &FontRef, entries: &[(String, Rgba<u8>)], w: i32, t: i32, black: Rgba<u8>) {
    if entries.is_empty() {
        return;
    }
    let box_w = 1 + entries.iter().map(|(n, _)| n.len()).max().unwrap_or(4) as i32 * 7 + 22;
    let x0 = (w - box_w - 10).max(0);
    let mut y = t + 4;
    for (name, color) in entries {
        draw_filled_rect_mut(img, Rect::at(x0, y).of_size(12, 12), *color);
        draw_text_mut(img, black, x0 + 16, y - 1, PxScale::from(12.0), fnt, name);
        y += 18;
    }
}

/// X/Y axis titles from `x_label` / `y_label` opts.
#[allow(clippy::too_many_arguments)]
fn draw_axis_titles(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, l: i32, r: i32, _t: i32, b: i32, black: Rgba<u8>) {
    if let Some(x) = opts.get("x_label").and_then(Value::as_str) {
        let cx = (l + r) / 2 - x.len() as i32 * 4;
        draw_text_mut(img, black, cx, b + 22, PxScale::from(14.0), fnt, x);
    }
    if let Some(y) = opts.get("y_label").and_then(Value::as_str) {
        // raster text can't rotate cheaply; place it at the top of the y axis
        draw_text_mut(img, black, l - 52, 26, PxScale::from(14.0), fnt, y);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bars(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    cats: &[String],
    l: i32,
    pw: f64,
    b: i32,
    yp: &dyn Fn(f64) -> f32,
    black: Rgba<u8>,
    stacked: bool,
    labels: bool,
) {
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    if stacked {
        let mut cum = vec![0.0f64; ncat];
        let barw = (slot * 0.8).max(1.0);
        for (si, s) in series.iter().enumerate() {
            let color = series_color(s, si);
            for (ci, v) in series_nums(s).into_iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.1;
                let y0 = yp(cum[ci] + v);
                let y1 = yp(cum[ci]);
                let hgt = (y1 - y0).max(1.0) as u32;
                draw_filled_rect_mut(
                    img,
                    Rect::at(x as i32, y0 as i32).of_size(barw as u32, hgt),
                    color,
                );
                cum[ci] += v;
            }
        }
    } else {
        let barw = (slot * 0.8 / nser as f64).max(1.0);
        for (si, s) in series.iter().enumerate() {
            let color = series_color(s, si);
            for (ci, v) in series_nums(s).into_iter().enumerate() {
                let x = l as f64 + ci as f64 * slot + slot * 0.1 + si as f64 * barw;
                let top = yp(v);
                let (y0, y1) = if top < base { (top, base) } else { (base, top) };
                let hgt = (y1 - y0).max(1.0) as u32;
                draw_filled_rect_mut(
                    img,
                    Rect::at(x as i32, y0 as i32).of_size(barw.max(1.0) as u32, hgt),
                    color,
                );
                if labels {
                    draw_text_mut(img, black, x as i32, top as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
                }
            }
        }
    }
    for (ci, cat) in cats.iter().enumerate() {
        let x = l as f64 + ci as f64 * slot + slot * 0.25;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(12.0), fnt, cat);
    }
}

/// Compact number formatting for data labels.
fn fmt_num(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Histogram of the first series' raw data, binned into `bins` (opt, default
/// 10) equal-width buckets; bars are bucket counts.
#[allow(clippy::too_many_arguments)]
fn render_histogram(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    opts: &Value,
    l: i32,
    pw: f64,
    t: i32,
    b: i32,
    black: Rgba<u8>,
) {
    let data = series.first().map(series_nums).unwrap_or_default();
    if data.is_empty() {
        return;
    }
    let nbins = opts.get("bins").and_then(Value::as_u64).unwrap_or(10).clamp(1, 200) as usize;
    let (lo, hi) = data.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| {
        (a.min(v), b.max(v))
    });
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let mut counts = vec![0u32; nbins];
    for &v in &data {
        let mut idx = ((v - lo) / span * nbins as f64) as usize;
        if idx >= nbins {
            idx = nbins - 1;
        }
        counts[idx] += 1;
    }
    let maxc = *counts.iter().max().unwrap_or(&1) as f64;
    let ph = (b - t).max(1) as f64;
    let slot = pw / nbins as f64;
    let color = series_color(series.first().unwrap_or(&Value::Null), 0);
    for (i, &c) in counts.iter().enumerate() {
        let x = l as f64 + i as f64 * slot + slot * 0.05;
        let bh = c as f64 / maxc * ph;
        let y0 = b as f64 - bh;
        draw_filled_rect_mut(
            img,
            Rect::at(x as i32, y0 as i32).of_size((slot * 0.9).max(1.0) as u32, bh.max(1.0) as u32),
            color,
        );
    }
    draw_text_mut(img, black, l, b + 6, PxScale::from(12.0), fnt, &format!("{lo:.1}"));
    draw_text_mut(img, black, (l as f64 + pw - 30.0) as i32, b + 6, PxScale::from(12.0), fnt, &format!("{hi:.1}"));
}

#[allow(clippy::too_many_arguments)]
fn render_line_area(
    img: &mut RgbaImage,
    series: &[Value],
    l: i32,
    r: i32,
    b: i32,
    ymin: f64,
    ymax: f64,
    fill: bool,
) {
    let pw = (r - l).max(1) as f64;
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        if data.is_empty() {
            continue;
        }
        let n = data.len();
        let xat = |i: usize| {
            l as f64 + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 }
        };
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        if fill {
            let mut poly: Vec<Point<i32>> = Vec::with_capacity(n + 2);
            poly.push(Point::new(xat(0) as i32, b));
            for (i, v) in data.iter().enumerate() {
                poly.push(Point::new(xat(i) as i32, yv(*v) as i32));
            }
            poly.push(Point::new(xat(n - 1) as i32, b));
            // dedup consecutive identical points (draw_polygon_mut requirement)
            poly.dedup();
            if poly.len() >= 3 && poly.first() != poly.last() {
                let mut fillc = color;
                fillc.0[3] = 120;
                draw_polygon_mut(img, &poly, fillc);
            }
        }
        for i in 1..n {
            draw_line_segment_mut(
                img,
                (xat(i - 1) as f32, yv(data[i - 1])),
                (xat(i) as f32, yv(data[i])),
                color,
            );
        }
    }
}

fn render_scatter(
    img: &mut RgbaImage,
    series: &[Value],
    l: f64,
    pw: f64,
    xmin: f64,
    xmax: f64,
    yp: impl Fn(f64) -> f32,
) {
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        for (x, y) in series_points(s) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            draw_filled_circle_mut(img, (px as i32, yp(y) as i32), 4, color);
        }
    }
}

fn render_bubble(
    img: &mut RgbaImage,
    series: &[Value],
    l: f64,
    pw: f64,
    xmin: f64,
    xmax: f64,
    yp: impl Fn(f64) -> f32,
) {
    let maxs = series
        .iter()
        .flat_map(series_points3)
        .map(|(_, _, s)| s)
        .fold(1.0f64, f64::max);
    for (si, s) in series.iter().enumerate() {
        let mut color = series_color(s, si);
        color.0[3] = 150;
        for (x, y, sz) in series_points3(s) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            let r = (sz / maxs).sqrt() * 24.0;
            draw_filled_circle_mut(img, (px as i32, yp(y) as i32), r.max(2.0) as i32, color);
        }
    }
}

/// Radar/spider chart: each category is a spoke; each series is a polygon of
/// its values around the spokes.
#[allow(clippy::too_many_arguments)]
fn render_radar(
    img: &mut RgbaImage,
    fnt: &FontRef,
    series: &[Value],
    cats: &[String],
    l: i32,
    t: i32,
    r: i32,
    b: i32,
    black: Rgba<u8>,
    grid: Rgba<u8>,
) {
    let nax = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if nax < 3 {
        return;
    }
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 20).max(20) as f64;
    let maxv = series.iter().flat_map(series_nums).fold(1.0f64, f64::max);
    let ang = |i: usize| -std::f64::consts::FRAC_PI_2 + i as f64 / nax as f64 * std::f64::consts::TAU;
    // rings + spokes
    for ring in 1..=4 {
        let rr = radius * ring as f64 / 4.0;
        let pts: Vec<Point<i32>> = (0..nax)
            .map(|i| Point::new(cx + (rr * ang(i).cos()) as i32, cy + (rr * ang(i).sin()) as i32))
            .collect();
        for i in 0..nax {
            let a = pts[i];
            let bb = pts[(i + 1) % nax];
            draw_line_segment_mut(img, (a.x as f32, a.y as f32), (bb.x as f32, bb.y as f32), grid);
        }
    }
    for i in 0..nax {
        let x = cx + (radius * ang(i).cos()) as i32;
        let y = cy + (radius * ang(i).sin()) as i32;
        draw_line_segment_mut(img, (cx as f32, cy as f32), (x as f32, y as f32), grid);
        if let Some(c) = cats.get(i) {
            draw_text_mut(img, black, x - 10, y - 6, PxScale::from(11.0), fnt, c);
        }
    }
    // series polygons (outline via line segments)
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let pt = |i: usize, v: f64| {
            let rr = v / maxv * radius;
            (cx as f32 + (rr * ang(i).cos()) as f32, cy as f32 + (rr * ang(i).sin()) as f32)
        };
        for i in 0..data.len() {
            let a = pt(i, data[i]);
            let bb = pt((i + 1) % data.len(), data[(i + 1) % data.len()]);
            draw_line_segment_mut(img, a, bb, color);
        }
    }
}

/// Raster sankey: distinct sources left, targets right, straight quad bands
/// proportional to value. (SVG output uses smooth bezier ribbons.)
fn render_sankey(img: &mut RgbaImage, opts: &Value, w: f64, h: f64) {
    let links = opts.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    if links.is_empty() {
        return;
    }
    let val = |lk: &Value| lk.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let src = |lk: &Value| lk.get("source").and_then(Value::as_u64).unwrap_or(0) as usize;
    let tgt = |lk: &Value| lk.get("target").and_then(Value::as_u64).unwrap_or(0) as usize;
    let total: f64 = links.iter().map(&val).sum();
    if total <= 0.0 {
        return;
    }
    let mut sources: Vec<usize> = links.iter().map(&src).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut targets: Vec<usize> = links.iter().map(&tgt).collect();
    targets.sort_unstable();
    targets.dedup();
    let (lx, rx) = (80.0, w - 100.0);
    let (top, bot) = (50.0, h - 30.0);
    let avail = bot - top;
    let nodew = 16.0;
    let band = |ids: &[usize], is_src: bool| {
        let mut pos = std::collections::HashMap::new();
        let mut y = top;
        for &id in ids {
            let sum: f64 = links.iter().filter(|lk| if is_src { src(lk) == id } else { tgt(lk) == id }).map(&val).sum();
            let ht = sum / total * avail;
            pos.insert(id, (y, ht));
            y += ht + 8.0;
        }
        pos
    };
    let spos = band(&sources, true);
    let tpos = band(&targets, false);
    for (&_id, &(y, ht)) in spos.iter() {
        draw_filled_rect_mut(img, Rect::at(lx as i32, y as i32).of_size(nodew as u32, ht.max(1.0) as u32), parse_color(Some(&Value::String("#4472c4".into()))));
    }
    for (&_id, &(y, ht)) in tpos.iter() {
        draw_filled_rect_mut(img, Rect::at(rx as i32, y as i32).of_size(nodew as u32, ht.max(1.0) as u32), parse_color(Some(&Value::String("#ed7d31".into()))));
    }
    let mut soff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    let mut toff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (i, lk) in links.iter().enumerate() {
        let bh = val(lk) / total * avail;
        let (sy0, _) = *spos.get(&src(lk)).unwrap_or(&(top, 0.0));
        let (ty0, _) = *tpos.get(&tgt(lk)).unwrap_or(&(top, 0.0));
        let so = soff.entry(src(lk)).or_insert(0.0);
        let sy = sy0 + *so;
        *so += bh;
        let to = toff.entry(tgt(lk)).or_insert(0.0);
        let ty = ty0 + *to;
        *to += bh;
        let x0 = (lx + nodew) as i32;
        let x1 = rx as i32;
        let mut c = palette(i);
        c.0[3] = 120;
        let poly = vec![
            Point::new(x0, sy as i32),
            Point::new(x1, ty as i32),
            Point::new(x1, (ty + bh) as i32),
            Point::new(x0, (sy + bh) as i32),
        ];
        let mut p = poly;
        p.dedup();
        if p.len() >= 3 {
            draw_polygon_mut(img, &p, c);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_pie(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, donut: bool, labels: bool, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 10).max(10) as f64;
    let mut angle = -std::f64::consts::FRAC_PI_2; // start at top
    for (i, v) in data.iter().enumerate() {
        let sweep = v / total * std::f64::consts::TAU;
        let steps = (sweep / 0.1).ceil().max(2.0) as usize;
        let mut poly: Vec<Point<i32>> = Vec::with_capacity(steps + 2);
        poly.push(Point::new(cx, cy));
        for k in 0..=steps {
            let a = angle + sweep * (k as f64 / steps as f64);
            poly.push(Point::new(
                cx + (radius * a.cos()) as i32,
                cy + (radius * a.sin()) as i32,
            ));
        }
        poly.dedup();
        if poly.len() >= 3 {
            draw_polygon_mut(img, &poly, palette(i));
        }
        if labels {
            let mid = angle + sweep / 2.0;
            let lx = cx + (radius * 0.65 * mid.cos()) as i32;
            let ly = cy + (radius * 0.65 * mid.sin()) as i32;
            let pct = v / total * 100.0;
            let text = cats.get(i).map(|c| format!("{c} {pct:.0}%")).unwrap_or_else(|| format!("{pct:.0}%"));
            draw_text_mut(img, black, lx - 14, ly - 6, PxScale::from(11.0), fnt, &text);
        }
        angle += sweep;
    }
    if donut {
        draw_filled_circle_mut(img, (cx, cy), (radius * 0.55) as i32, Rgba([255, 255, 255, 255]));
    }
}

/// Stepped line: hold each value then jump to the next.
#[allow(clippy::too_many_arguments)]
fn render_step(img: &mut RgbaImage, series: &[Value], l: i32, r: i32, b: i32, ymin: f64, ymax: f64, ) {
    let pw = (r - l).max(1) as f64;
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let n = data.len();
        if n == 0 {
            continue;
        }
        let xat = |i: usize| l as f64 + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        for i in 1..n {
            let (x0, x1) = (xat(i - 1) as f32, xat(i) as f32);
            let (y0, y1) = (yv(data[i - 1]), yv(data[i]));
            draw_line_segment_mut(img, (x0, y0), (x1, y0), color); // horizontal hold
            draw_line_segment_mut(img, (x1, y0), (x1, y1), color); // vertical step
        }
    }
}

/// Combination chart: each series renders as a bar (default) or a line when
/// it carries `kind:"line"`, sharing one y scale.
#[allow(clippy::too_many_arguments)]
fn render_combo(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, r: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32, ymin: f64, ymax: f64, black: Rgba<u8>, labels: bool) {
    let bar_series: Vec<&Value> = series.iter().filter(|s| s.get("kind").and_then(Value::as_str) != Some("line")).collect();
    let line_series: Vec<&Value> = series.iter().filter(|s| s.get("kind").and_then(Value::as_str) == Some("line")).collect();
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    let nbar = bar_series.len().max(1);
    let barw = (slot * 0.7 / nbar as f64).max(1.0);
    for (bi, s) in bar_series.iter().enumerate() {
        let color = series_color(s, series.iter().position(|x| std::ptr::eq(x, *s)).unwrap_or(bi));
        for (ci, v) in series_nums(s).into_iter().enumerate() {
            let x = l as f64 + ci as f64 * slot + slot * 0.15 + bi as f64 * barw;
            let top = yp(v);
            let (y0, y1) = if top < base { (top, base) } else { (base, top) };
            draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
            if labels {
                draw_text_mut(img, black, x as i32, top as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
            }
        }
    }
    for s in &line_series {
        let color = series_color(s, series.iter().position(|x| std::ptr::eq(x, *s)).unwrap_or(0));
        let data = series_nums(s);
        let n = data.len();
        let xat = |i: usize| l as f64 + ci_center(i, slot);
        let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
        for i in 1..n {
            draw_line_segment_mut(img, (xat(i - 1) as f32, yv(data[i - 1])), (xat(i) as f32, yv(data[i])), color);
        }
        for (i, v) in data.iter().enumerate() {
            draw_filled_circle_mut(img, (xat(i) as i32, yv(*v) as i32), 3, color);
        }
    }
    let _ = r;
}

/// Center x of category `i` within its slot.
fn ci_center(i: usize, slot: f64) -> f64 {
    i as f64 * slot + slot * 0.5
}

/// Waterfall: running cumulative bars from the first series' deltas; rising
/// steps in green, falling in red, with connector lines.
#[allow(clippy::too_many_arguments)]
fn render_waterfall(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, pw: f64, _b: i32, yp: &dyn Fn(f64) -> f32, black: Rgba<u8>, labels: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let up = Rgba([0x55, 0xaa, 0x55, 255]);
    let down = Rgba([0xcc, 0x55, 0x55, 255]);
    let barw = (slot * 0.7).max(1.0);
    let mut cum = 0.0;
    for (i, &v) in data.iter().enumerate() {
        let prev = cum;
        cum += v;
        let (y0, y1) = (yp(prev.max(cum)), yp(prev.min(cum)));
        let x = l as f64 + i as f64 * slot + slot * 0.15;
        let color = if v >= 0.0 { up } else { down };
        draw_filled_rect_mut(img, Rect::at(x as i32, y0 as i32).of_size(barw as u32, (y1 - y0).max(1.0) as u32), color);
        if i + 1 < n {
            let yc = yp(cum);
            draw_line_segment_mut(img, ((x + barw) as f32, yc), ((x + slot) as f32, yc), Rgba([150, 150, 150, 255]));
        }
        if labels {
            draw_text_mut(img, black, x as i32, y0 as i32 - 14, PxScale::from(11.0), fnt, &fmt_num(v));
        }
        let _ = cats;
    }
}

/// OHLC bars or candlesticks. `data:[[open,high,low,close],...]` for the
/// first series.
fn render_ohlc(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32, candle: bool) {
    use imageproc::drawing::draw_hollow_rect_mut;
    let data = series.first().map(series_ohlc).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let up = Rgba([0x33, 0x99, 0x55, 255]);
    let down = Rgba([0xcc, 0x44, 0x44, 255]);
    let bw = (slot * 0.5).max(2.0);
    for (i, o) in data.iter().enumerate() {
        let [open, high, low, close] = *o;
        let cx = l as f64 + i as f64 * slot + slot * 0.5;
        let color = if close >= open { up } else { down };
        // wick
        draw_line_segment_mut(img, (cx as f32, yp(high)), (cx as f32, yp(low)), color);
        if candle {
            let (top, bot) = (open.max(close), open.min(close));
            let y = yp(top);
            let height = (yp(bot) - y).max(1.0);
            let rect = Rect::at((cx - bw / 2.0) as i32, y as i32).of_size(bw as u32, height as u32);
            if close >= open {
                draw_hollow_rect_mut(img, rect, color);
            } else {
                draw_filled_rect_mut(img, rect, color);
            }
        } else {
            // open tick (left), close tick (right)
            draw_line_segment_mut(img, ((cx - bw / 2.0) as f32, yp(open)), (cx as f32, yp(open)), color);
            draw_line_segment_mut(img, (cx as f32, yp(close)), ((cx + bw / 2.0) as f32, yp(close)), color);
        }
        let _ = b;
    }
}

/// Box-and-whisker per series: min / q1 / median / q3 / max from raw data.
fn render_boxplot(img: &mut RgbaImage, series: &[Value], l: i32, pw: f64, b: i32, yp: &dyn Fn(f64) -> f32) {
    let n = series.len().max(1);
    let slot = pw / n as f64;
    let bw = (slot * 0.4).max(3.0);
    for (si, s) in series.iter().enumerate() {
        let Some([mn, q1, med, q3, mx]) = five_number(&series_nums(s)) else { continue };
        let color = series_color(s, si);
        let cx = l as f64 + si as f64 * slot + slot * 0.5;
        let (x0, x1) = ((cx - bw / 2.0) as f32, (cx + bw / 2.0) as f32);
        // whiskers
        draw_line_segment_mut(img, (cx as f32, yp(mn)), (cx as f32, yp(q1)), color);
        draw_line_segment_mut(img, (cx as f32, yp(q3)), (cx as f32, yp(mx)), color);
        draw_line_segment_mut(img, (x0, yp(mn)), (x1, yp(mn)), color);
        draw_line_segment_mut(img, (x0, yp(mx)), (x1, yp(mx)), color);
        // box (q1..q3) + median line
        let y = yp(q3);
        let height = (yp(q1) - y).max(1.0);
        use imageproc::drawing::draw_hollow_rect_mut;
        draw_hollow_rect_mut(img, Rect::at(x0 as i32, y as i32).of_size(bw as u32, height as u32), color);
        draw_line_segment_mut(img, (x0, yp(med)), (x1, yp(med)), color);
        let _ = b;
    }
}

/// Funnel: centered descending bands from the first series' values.
#[allow(clippy::too_many_arguments)]
fn render_funnel(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, labels: bool, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(0.0f64, f64::max).max(f64::EPSILON);
    let cx = (l + r) / 2;
    let fullw = (r - l) as f64 * 0.8;
    let band_h = ((b - t) as f64 / n as f64) * 0.85;
    let gap = ((b - t) as f64 / n as f64) * 0.15;
    for (i, &v) in data.iter().enumerate() {
        let half = fullw * (v / maxv) / 2.0;
        let y = t as f64 + i as f64 * (band_h + gap);
        let poly = vec![
            Point::new((cx as f64 - half) as i32, y as i32),
            Point::new((cx as f64 + half) as i32, y as i32),
            Point::new((cx as f64 + half) as i32, (y + band_h) as i32),
            Point::new((cx as f64 - half) as i32, (y + band_h) as i32),
        ];
        let mut p = poly;
        p.dedup();
        if p.len() >= 3 {
            draw_polygon_mut(img, &p, palette(i));
        }
        if labels {
            let text = cats.get(i).map(|c| format!("{c}: {}", fmt_num(v))).unwrap_or_else(|| fmt_num(v));
            draw_text_mut(img, black, cx - text.len() as i32 * 3, (y + band_h / 2.0 - 6.0) as i32, PxScale::from(12.0), fnt, &text);
        }
    }
}

/// Semicircular gauge: `value` against `max` (default 100).
#[allow(clippy::too_many_arguments)]
fn render_gauge(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let value = opts.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let max = opts.get("max").and_then(Value::as_f64).unwrap_or(100.0).max(f64::EPSILON);
    let frac = (value / max).clamp(0.0, 1.0);
    let cx = (l + r) / 2;
    let cy = (t + b) * 2 / 3;
    let radius = ((r - l).min((b - t) * 2) / 2 - 20).max(20) as f64;
    let thick = (radius * 0.28) as i32;
    // background arc (gray) then filled arc (palette) over the top semicircle
    let arc = |img: &mut RgbaImage, frac: f64, color: Rgba<u8>| {
        let start = std::f64::consts::PI; // 180°
        let steps = 120;
        for k in 0..=((steps as f64 * frac) as usize) {
            let a = start + (k as f64 / steps as f64) * std::f64::consts::PI;
            let x = cx + (radius * a.cos()) as i32;
            let y = cy + (radius * a.sin()) as i32;
            draw_filled_circle_mut(img, (x, y), thick / 2, color);
        }
    };
    arc(img, 1.0, Rgba([220, 220, 220, 255]));
    arc(img, frac, palette(0));
    let text = format!("{}/{}", fmt_num(value), fmt_num(max));
    draw_text_mut(img, black, cx - text.len() as i32 * 5, cy - 10, PxScale::from(22.0), fnt, &text);
}

/// Heatmap of a `matrix:[[..],..]` (rows) or series-of-data, colored
/// white→blue by normalized value.
#[allow(clippy::too_many_arguments)]
fn render_heatmap(img: &mut RgbaImage, fnt: &FontRef, opts: &Value, cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    // rows: explicit matrix, else each series' data is a row
    let rows: Vec<Vec<f64>> = if let Some(m) = opts.get("matrix").and_then(Value::as_array) {
        m.iter().map(|row| row.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()).collect()
    } else {
        opts.get("series").and_then(Value::as_array).map(|s| s.iter().map(series_nums).collect()).unwrap_or_default()
    };
    let nr = rows.len();
    let nc = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if nr == 0 || nc == 0 {
        return;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for row in &rows {
        for &v in row {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let cw = (r - l) as f64 / nc as f64;
    let ch = (b - t) as f64 / nr as f64;
    for (ri, row) in rows.iter().enumerate() {
        for (ci, &v) in row.iter().enumerate() {
            let frac = ((v - lo) / span).clamp(0.0, 1.0);
            // white (low) -> deep blue (high)
            let color = Rgba([
                (255.0 - frac * 187.0) as u8,
                (255.0 - frac * 141.0) as u8,
                255,
                255,
            ]);
            let x = l as f64 + ci as f64 * cw;
            let y = t as f64 + ri as f64 * ch;
            draw_filled_rect_mut(img, Rect::at(x as i32, y as i32).of_size(cw.max(1.0) as u32, ch.max(1.0) as u32), color);
        }
    }
    for (ci, cat) in cats.iter().enumerate().take(nc) {
        let x = l as f64 + ci as f64 * cw + cw * 0.2;
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(11.0), fnt, cat);
    }
}

/// Least-squares fit of `points` → `(slope, intercept)`, or None if
/// degenerate.
fn linfit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if n < 2.0 {
        return None;
    }
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(ax, ay), &(x, y)| (ax + x, ay + y));
    let (sxx, sxy) = points.iter().fold((0.0, 0.0), |(axx, axy), &(x, y)| (axx + x * x, axy + x * y));
    let denom = n * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return None;
    }
    let m = (n * sxy - sx * sy) / denom;
    Some((m, (sy - m * sx) / n))
}

/// Stacked area: each series' band is filled on top of the cumulative sum.
fn render_stacked_area(img: &mut RgbaImage, series: &[Value], l: i32, r: i32, b: i32, ymin: f64, ymax: f64) {
    let pw = (r - l).max(1) as f64;
    let ncat = series.iter().map(|s| series_nums(s).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let xat = |i: usize| l as f64 + if ncat > 1 { i as f64 / (ncat - 1) as f64 * pw } else { pw / 2.0 };
    let yv = |v: f64| (b as f64 - (v - ymin) / (ymax - ymin) * (b as f64 - 44.0).max(1.0)) as f32;
    let mut cum = vec![0.0f64; ncat];
    for (si, s) in series.iter().enumerate() {
        let color = series_color(s, si);
        let data = series_nums(s);
        let mut poly: Vec<Point<i32>> = Vec::new();
        // top edge (cum + value) left→right
        for ci in 0..ncat {
            let v = data.get(ci).copied().unwrap_or(0.0);
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci] + v) as i32));
        }
        // bottom edge (cum) right→left
        for ci in (0..ncat).rev() {
            poly.push(Point::new(xat(ci) as i32, yv(cum[ci]) as i32));
        }
        poly.dedup();
        if poly.len() >= 3 && poly.first() != poly.last() {
            let mut fc = color;
            fc.0[3] = 150;
            draw_polygon_mut(img, &poly, fc);
        }
        for ci in 0..ncat {
            cum[ci] += data.get(ci).copied().unwrap_or(0.0);
        }
    }
}

/// Squarish treemap of the first series via area-correct recursive binary
/// split (sorted desc, halved by cumulative value, split along the long axis).
#[allow(clippy::too_many_arguments)]
fn render_treemap(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let mut items: Vec<(usize, f64)> = data.iter().enumerate().map(|(i, &v)| (i, v.max(0.0))).collect();
    items.retain(|&(_, v)| v > 0.0);
    if items.is_empty() {
        return;
    }
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let rect = (l as f64, t as f64, (r - l) as f64, (b - t) as f64);
    let mut placed: Vec<(usize, (f64, f64, f64, f64))> = Vec::new();
    treemap_layout(&items, rect, &mut placed);
    for (idx, (x, y, w, h)) in placed {
        draw_filled_rect_mut(img, Rect::at(x as i32 + 1, y as i32 + 1).of_size((w as u32).saturating_sub(2).max(1), (h as u32).saturating_sub(2).max(1)), palette(idx));
        if let Some(name) = cats.get(idx) {
            if w > 24.0 && h > 14.0 {
                draw_text_mut(img, black, x as i32 + 4, y as i32 + 4, PxScale::from(11.0), fnt, name);
            }
        }
    }
}

fn treemap_layout(items: &[(usize, f64)], rect: (f64, f64, f64, f64), out: &mut Vec<(usize, (f64, f64, f64, f64))>) {
    if items.len() == 1 {
        out.push((items[0].0, rect));
        return;
    }
    let total: f64 = items.iter().map(|&(_, v)| v).sum();
    let half = total / 2.0;
    let (mut acc, mut i) = (0.0, 0usize);
    while i < items.len() - 1 && acc + items[i].1 < half {
        acc += items[i].1;
        i += 1;
    }
    let (a, c) = items.split_at(i + 1);
    let suma: f64 = a.iter().map(|&(_, v)| v).sum();
    let frac = if total > 0.0 { suma / total } else { 0.5 };
    let (x, y, w, h) = rect;
    let (ra, rc) = if w >= h {
        ((x, y, w * frac, h), (x + w * frac, y, w * (1.0 - frac), h))
    } else {
        ((x, y, w, h * frac), (x, y + h * frac, w, h * (1.0 - frac)))
    };
    treemap_layout(a, ra, out);
    treemap_layout(c, rc, out);
}

/// Polar / rose chart: equal-angle wedges, radius proportional to value.
#[allow(clippy::too_many_arguments)]
fn render_polar(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(f64::EPSILON, f64::max);
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    let radius = ((r - l).min(b - t) / 2 - 20).max(20) as f64;
    for ring in 1..=4 {
        draw_hollow_circle_safe(img, cx, cy, (radius * ring as f64 / 4.0) as i32, grid);
    }
    let step = std::f64::consts::TAU / n as f64;
    for (i, &v) in data.iter().enumerate() {
        let a0 = -std::f64::consts::FRAC_PI_2 + i as f64 * step;
        let a1 = a0 + step * 0.9;
        let rr = v / maxv * radius;
        let steps = 10;
        let mut poly: Vec<Point<i32>> = vec![Point::new(cx, cy)];
        for k in 0..=steps {
            let a = a0 + (a1 - a0) * k as f64 / steps as f64;
            poly.push(Point::new(cx + (rr * a.cos()) as i32, cy + (rr * a.sin()) as i32));
        }
        poly.dedup();
        if poly.len() >= 3 {
            draw_polygon_mut(img, &poly, palette(i));
        }
        if let Some(c) = cats.get(i) {
            let am = (a0 + a1) / 2.0;
            draw_text_mut(img, black, cx + ((radius + 6.0) * am.cos()) as i32 - 8, cy + ((radius + 6.0) * am.sin()) as i32 - 6, PxScale::from(11.0), fnt, c);
        }
    }
}

/// `draw_hollow_circle_mut` panics on r<=0; guard it.
fn draw_hollow_circle_safe(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, color: Rgba<u8>) {
    use imageproc::drawing::draw_hollow_circle_mut;
    if r > 0 {
        draw_hollow_circle_mut(img, (cx, cy), r, color);
    }
}

/// Bullet graphs: one horizontal bullet per series — qualitative range bands,
/// a measure bar, and a target tick. Series: `{name, data:[value]|value,
/// target, ranges:[r1,r2,...]}`.
#[allow(clippy::too_many_arguments)]
fn render_bullet(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let n = series.len().max(1);
    let row_h = ((b - t) / n as i32).max(12);
    for (si, s) in series.iter().enumerate() {
        let value = s.get("value").and_then(Value::as_f64).or_else(|| series_nums(s).first().copied()).unwrap_or(0.0);
        let target = s.get("target").and_then(Value::as_f64);
        let ranges: Vec<f64> = s.get("ranges").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let scale_max = ranges.iter().cloned().fold(value.max(target.unwrap_or(0.0)), f64::max).max(f64::EPSILON);
        let y0 = t + si as i32 * row_h + 4;
        let bh = (row_h - 10).max(6);
        let px = |v: f64| l as f64 + v / scale_max * (r - l) as f64;
        // range bands light→dark
        let mut prev = 0.0;
        for (ri, &rmax) in ranges.iter().enumerate() {
            let shade = (220 - ri as i32 * 40).clamp(120, 220) as u8;
            draw_filled_rect_mut(img, Rect::at(px(prev) as i32, y0).of_size((px(rmax) - px(prev)).max(1.0) as u32, bh as u32), Rgba([shade, shade, shade, 255]));
            prev = rmax;
        }
        if ranges.is_empty() {
            draw_filled_rect_mut(img, Rect::at(l, y0).of_size((r - l) as u32, bh as u32), grid);
        }
        // measure bar (thinner, centered)
        let mbh = (bh / 2).max(3);
        draw_filled_rect_mut(img, Rect::at(l, y0 + (bh - mbh) / 2).of_size((px(value) - l as f64).max(1.0) as u32, mbh as u32), palette(si));
        // target tick
        if let Some(tg) = target {
            let tx = px(tg) as f32;
            draw_line_segment_mut(img, (tx, y0 as f32 - 2.0), (tx, (y0 + bh) as f32 + 2.0), Rgba([20, 20, 20, 255]));
        }
        if let Some(name) = s.get("name").and_then(Value::as_str) {
            draw_text_mut(img, black, l, y0 - 2, PxScale::from(11.0), fnt, name);
        }
    }
}

/// Pareto: bars sorted descending (left axis) + cumulative-percent line
/// (right axis 0–100%).
#[allow(clippy::too_many_arguments)]
fn render_pareto(img: &mut RgbaImage, fnt: &FontRef, series: &[Value], cats: &[String], l: i32, t: i32, r: i32, b: i32, black: Rgba<u8>, grid: Rgba<u8>) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let mut pairs: Vec<(String, f64)> = data
        .iter()
        .enumerate()
        .map(|(i, &v)| (cats.get(i).cloned().unwrap_or_else(|| format!("{}", i + 1)), v))
        .collect();
    pairs.sort_by(|a, c| c.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = pairs.iter().map(|(_, v)| v).sum();
    if total <= 0.0 {
        return;
    }
    let maxv = pairs.iter().map(|(_, v)| *v).fold(f64::EPSILON, f64::max);
    draw_line_segment_mut(img, (l as f32, b as f32), (r as f32, b as f32), black);
    draw_line_segment_mut(img, (l as f32, t as f32), (l as f32, b as f32), black);
    draw_line_segment_mut(img, (r as f32, t as f32), (r as f32, b as f32), black);
    let pw = (r - l) as f64;
    let ph = (b - t) as f64;
    let slot = pw / pairs.len().max(1) as f64;
    let barw = (slot * 0.7) as u32;
    // left-axis gridlines / labels
    for i in 0..=5 {
        let y = b as f32 - (i as f32 / 5.0) * ph as f32;
        draw_line_segment_mut(img, (l as f32, y), (r as f32, y), grid);
        draw_text_mut(img, black, 4, y as i32 - 6, PxScale::from(11.0), fnt, &fmt_num(maxv * i as f64 / 5.0));
        draw_text_mut(img, black, r + 2, y as i32 - 6, PxScale::from(11.0), fnt, &format!("{}%", i * 20));
    }
    let mut cum = 0.0;
    let mut prev: Option<(f32, f32)> = None;
    for (i, (name, v)) in pairs.iter().enumerate() {
        let x = l as f64 + i as f64 * slot + slot * 0.15;
        let bh = v / maxv * ph;
        draw_filled_rect_mut(img, Rect::at(x as i32, (b as f64 - bh) as i32).of_size(barw.max(1), bh.max(1.0) as u32), palette(0));
        cum += v;
        let cy = b as f32 - (cum / total) as f32 * ph as f32;
        let cxp = (x + barw as f64 / 2.0) as f32;
        draw_filled_circle_mut(img, (cxp as i32, cy as i32), 3, Rgba([0xc0, 0x3a, 0x2b, 255]));
        if let Some((px_, py_)) = prev {
            draw_line_segment_mut(img, (px_, py_), (cxp, cy), Rgba([0xc0, 0x3a, 0x2b, 255]));
        }
        prev = Some((cxp, cy));
        draw_text_mut(img, black, x as i32, b + 6, PxScale::from(11.0), fnt, name);
    }
}
