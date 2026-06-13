// Vector chart rendering — the same chart model as chart_render, emitted as
// SVG markup instead of a raster image. Gives true vector output (scales
// losslessly) for line/scatter/pie/donut/bar/column/stacked/area/histogram/
// sankey. `chart_save` dispatches on the path extension: .svg -> this vector
// path, .pdf -> the chart embedded in a one-page PDF, any raster extension
// (.png/.jpg/.tif/.bmp/.webp/.gif) -> chart_render + img_save.

use std::fmt::Write as _;

fn svg_color(c: Rgba<u8>) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0[0], c.0[1], c.0[2])
}

fn svg_palette(i: usize) -> String {
    svg_color(palette(i))
}

/// Build an SVG document string for a chart spec.
fn chart_to_svg(opts: &Value) -> Result<String> {
    let kind = opts.get("type").and_then(Value::as_str).unwrap_or("bar");
    let w = opts.get("width").and_then(Value::as_u64).unwrap_or(800).max(120) as f64;
    let h = opts.get("height").and_then(Value::as_u64).unwrap_or(600).max(120) as f64;
    let empty_series: Vec<Value> = Vec::new();
    let series = opts.get("series").and_then(Value::as_array).unwrap_or(&empty_series);
    let cats: Vec<String> = opts
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    let title = opts.get("title").and_then(Value::as_str).unwrap_or("");
    let smooth = opts.get("smooth").and_then(Value::as_bool).unwrap_or(false);
    set_palette(opts);
    let bg = opts.get("background").and_then(Value::as_str).unwrap_or("#ffffff");

    let mut s = String::new();
    let _ = write!(
        s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="sans-serif">"#
    );
    let _ = write!(s, r##"<rect width="{w}" height="{h}" fill="{}"/>"##, xml_escape(bg));
    if !title.is_empty() {
        let _ = write!(
            s,
            r##"<text x="{}" y="26" text-anchor="middle" font-size="20" font-weight="bold" fill="#1e1e1e">{}</text>"##,
            w / 2.0,
            xml_escape(title)
        );
    }

    let labels = opts.get("labels").and_then(Value::as_bool).unwrap_or(false);
    let needs_series = !matches!(kind, "sankey" | "gauge" | "heatmap" | "sunburst");
    if needs_series && opts.get("series").and_then(Value::as_array).is_none() {
        return Err(anyhow!("missing series (expected array)"));
    }

    let (l, r, t, b) = (60.0, w - 24.0, 44.0, h - 40.0);

    let mut special = true;
    match kind {
        "sankey" => svg_sankey(&mut s, opts, w, h),
        "pie" | "donut" | "doughnut" => svg_pie(&mut s, series, &cats, w, h, kind != "pie", labels),
        "radar" => svg_radar(&mut s, series, &cats, w, h),
        "funnel" => svg_funnel(&mut s, series, &cats, l, t, r, b, labels),
        "gauge" => svg_gauge(&mut s, opts, l, t, r, b),
        "heatmap" => svg_heatmap(&mut s, opts, &cats, l, t, r, b),
        "treemap" => svg_treemap(&mut s, series, &cats, l, t, r, b),
        "polar" => svg_polar(&mut s, series, &cats, l, t, r, b),
        "bullet" => svg_bullet(&mut s, series, l, t, r, b),
        "pareto" => svg_pareto(&mut s, series, &cats, l, t, r, b),
        "gantt" => svg_gantt(&mut s, series, l, t, r, b),
        "sunburst" => svg_sunburst(&mut s, opts, series, l, t, r, b),
        "waffle" => svg_waffle(&mut s, series, l, t, r, b),
        "slope" => svg_slope(&mut s, series, l, t, r, b),
        _ => special = false,
    }

    if !special {
        let pw = (r - l).max(1.0);
        let ph = (b - t).max(1.0);
        let scatter = kind == "scatter" || kind == "bubble";
        let ohlc_like = kind == "ohlc" || kind == "candlestick";

        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        let (mut xmin, mut xmax) = (f64::INFINITY, f64::NEG_INFINITY);
        if scatter {
            for ser in series {
                for (x, y, _) in series_points3(ser) {
                    ymin = ymin.min(y);
                    ymax = ymax.max(y);
                    xmin = xmin.min(x);
                    xmax = xmax.max(x);
                }
            }
        } else if ohlc_like {
            for ser in series {
                for o in series_ohlc(ser) {
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
            let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
            ymin = ymin.min(0.0);
            for ci in 0..ncat {
                let col: f64 = series.iter().map(|x| series_nums(x).get(ci).copied().unwrap_or(0.0)).sum();
                ymax = ymax.max(col);
            }
        } else if matches!(kind, "range" | "range_bar" | "range_column") {
            for ser in series {
                for (lo, hi) in series_pairs(ser) {
                    ymin = ymin.min(lo);
                    ymax = ymax.max(hi);
                }
            }
        } else if kind == "percent_stacked" {
            ymin = 0.0;
            ymax = 100.0;
        } else if kind == "streamgraph" {
            let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
            let mut maxtot = 0.0f64;
            for ci in 0..ncat {
                let col: f64 = series.iter().map(|x| series_nums(x).get(ci).copied().unwrap_or(0.0)).sum();
                maxtot = maxtot.max(col);
            }
            ymax = maxtot / 2.0;
            ymin = -maxtot / 2.0;
        } else {
            for ser in series {
                for v in series_nums(ser) {
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
        let logy = opts.get("log_y").and_then(Value::as_bool) == Some(true) && ymin > 0.0;
        let lmin = ymin.max(1e-9).log10();
        let lspan = (ymax.max(1e-9).log10() - lmin).max(1e-9);
        let yp = |v: f64| {
            let frac = if logy {
                (v.max(1e-9).log10() - lmin) / lspan
            } else {
                (v - ymin) / (ymax - ymin)
            };
            b - frac * ph
        };

        // axes + gridlines
        let _ = write!(s, r##"<line x1="{l}" y1="{b}" x2="{r}" y2="{b}" stroke="#1e1e1e"/>"##);
        let _ = write!(s, r##"<line x1="{l}" y1="{t}" x2="{l}" y2="{b}" stroke="#1e1e1e"/>"##);
        for i in 0..=5 {
            let v = if logy {
                10f64.powf(lmin + lspan * i as f64 / 5.0)
            } else {
                ymin + (ymax - ymin) * (i as f64) / 5.0
            };
            let y = yp(v);
            let _ = write!(s, r##"<line x1="{l}" y1="{y}" x2="{r}" y2="{y}" stroke="#d2d2d2"/>"##);
            let _ = write!(s, r##"<text x="6" y="{}" font-size="11" fill="#1e1e1e">{}</text>"##, y + 4.0, xml_escape(&fmt_num(v)));
        }

        match kind {
            "line" => svg_line_area(&mut s, series, l, pw, &yp, b, false, smooth),
            "area" => svg_line_area(&mut s, series, l, pw, &yp, b, true, smooth),
            "stacked_area" => svg_stacked_area(&mut s, series, l, pw, &yp),
            "streamgraph" => svg_streamgraph(&mut s, series, l, pw, &yp, ymin, ymax),
            "range" | "range_bar" | "range_column" => svg_range(&mut s, series, l, pw, &yp),
            "percent_stacked" => svg_percent_stacked(&mut s, series, l, pw, &yp),
            "step" => svg_step(&mut s, series, l, pw, &yp),
            "scatter" => svg_scatter(&mut s, series, l, pw, xmin, xmax, &yp),
            "bubble" => svg_bubble(&mut s, series, l, pw, xmin, xmax, &yp),
            "histogram" => svg_histogram(&mut s, series, opts, l, pw, t, b),
            "stacked" | "stacked_bar" => svg_bars(&mut s, series, &cats, l, pw, &yp, true, labels),
            "combo" => svg_combo(&mut s, series, l, pw, &yp, labels),
            "waterfall" => svg_waterfall(&mut s, series, l, pw, &yp, labels),
            "ohlc" => svg_ohlc(&mut s, series, l, pw, &yp, false),
            "candlestick" => svg_ohlc(&mut s, series, l, pw, &yp, true),
            "boxplot" => svg_boxplot(&mut s, series, l, pw, &yp),
            "lollipop" => svg_lollipop(&mut s, series, l, pw, &yp, true),
            "dot" => svg_lollipop(&mut s, series, l, pw, &yp, false),
            _ => svg_bars(&mut s, series, &cats, l, pw, &yp, false, labels),
        }
        // markers on line-family points
        if opts.get("markers").and_then(Value::as_bool) == Some(true)
            && matches!(kind, "line" | "area" | "step" | "stacked_area")
        {
            for (si, ser) in series.iter().enumerate() {
                let col = svg_palette(si);
                let data = series_nums(ser);
                let n = data.len();
                for (i, v) in data.iter().enumerate() {
                    let x = l + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
                    let _ = write!(s, r##"<circle cx="{x:.1}" cy="{:.1}" r="3" fill="{col}"/>"##, yp(*v));
                }
            }
        }
        // horizontal reference lines
        if let Some(refs) = opts.get("reference_lines").and_then(Value::as_array) {
            for rl in refs {
                if let Some(yv) = rl.get("y").and_then(Value::as_f64) {
                    let col = rl.get("color").and_then(Value::as_str).unwrap_or("#cc3333");
                    let _ = write!(s, r##"<line x1="{l}" y1="{:.1}" x2="{r}" y2="{:.1}" stroke="{col}" stroke-dasharray="6,4"/>"##, yp(yv), yp(yv));
                }
            }
        }
        // optional least-squares trendline over scatter points
        if scatter && opts.get("trendline").and_then(Value::as_bool) == Some(true) {
            for (si, ser) in series.iter().enumerate() {
                if let Some((m, c)) = linfit(&series_points(ser)) {
                    let col = svg_palette(si);
                    let px = |x: f64| l + (x - xmin) / (xmax - xmin) * pw;
                    let _ = write!(s, r##"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{col}" stroke-width="1.5" stroke-dasharray="5,4"/>"##, px(xmin), yp(m * xmin + c), px(xmax), yp(m * xmax + c));
                }
            }
        }
        // error bars: per-series `errors` parallel to `data`
        if matches!(kind, "bar" | "column" | "line" | "area" | "scatter") {
            let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0).max(1);
            for ser in series {
                let Some(errs) = ser.get("errors").and_then(Value::as_array) else { continue };
                let data = series_nums(ser);
                for (ci, e) in errs.iter().filter_map(Value::as_f64).enumerate() {
                    let Some(&v) = data.get(ci) else { continue };
                    let x = if scatter {
                        match series_points(ser).get(ci) {
                            Some((px, _)) => l + (px - xmin) / (xmax - xmin) * pw,
                            None => continue,
                        }
                    } else {
                        l + (ci as f64 + 0.5) / ncat as f64 * pw
                    };
                    let (yhi, ylo) = (yp(v + e), yp(v - e));
                    let _ = write!(s, r##"<line x1="{x:.1}" y1="{yhi:.1}" x2="{x:.1}" y2="{ylo:.1}" stroke="#1e1e1e"/>"##);
                    let _ = write!(s, r##"<line x1="{:.1}" y1="{yhi:.1}" x2="{:.1}" y2="{yhi:.1}" stroke="#1e1e1e"/>"##, x - 3.0, x + 3.0);
                    let _ = write!(s, r##"<line x1="{:.1}" y1="{ylo:.1}" x2="{:.1}" y2="{ylo:.1}" stroke="#1e1e1e"/>"##, x - 3.0, x + 3.0);
                }
            }
        }
        // annotations
        if let Some(anns) = opts.get("annotations").and_then(Value::as_array) {
            let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(1).max(1);
            for a in anns {
                let (Some(xi), Some(yv)) = (a.get("x").and_then(Value::as_f64), a.get("y").and_then(Value::as_f64)) else { continue };
                let x = l + if ncat > 1 { xi / (ncat - 1) as f64 * pw } else { pw / 2.0 };
                let col = a.get("color").and_then(Value::as_str).unwrap_or("#c0392b");
                let _ = write!(s, r##"<circle cx="{x:.1}" cy="{:.1}" r="4" fill="{col}"/>"##, yp(yv));
                if let Some(txt) = a.get("text").and_then(Value::as_str) {
                    let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, x + 6.0, yp(yv) - 4.0, xml_escape(txt));
                }
            }
        }
        // category labels
        if !cats.is_empty() && !scatter {
            let slot = pw / cats.len().max(1) as f64;
            for (i, c) in cats.iter().enumerate() {
                let x = l + i as f64 * slot + slot * 0.3;
                let _ = write!(s, r##"<text x="{x}" y="{}" font-size="11" fill="#1e1e1e">{}</text>"##, b + 16.0, xml_escape(c));
            }
        }
        // axis titles
        if let Some(xl) = opts.get("x_label").and_then(Value::as_str) {
            let _ = write!(s, r##"<text x="{}" y="{}" text-anchor="middle" font-size="14" fill="#1e1e1e">{}</text>"##, (l + r) / 2.0, b + 34.0, xml_escape(xl));
        }
        if let Some(yl) = opts.get("y_label").and_then(Value::as_str) {
            let _ = write!(s, r##"<text x="16" y="{cy}" text-anchor="middle" font-size="14" fill="#1e1e1e" transform="rotate(-90 16 {cy})">{}</text>"##, xml_escape(yl), cy = (t + b) / 2.0);
        }
    }

    // shared legend
    if opts.get("legend").and_then(Value::as_bool) != Some(false) {
        svg_legend(&mut s, kind, series, &cats, w, t);
    }

    s.push_str("</svg>");
    Ok(s)
}

/// Top-right SVG legend with color swatches.
fn svg_legend(s: &mut String, kind: &str, series: &[Value], cats: &[String], w: f64, t: f64) {
    let entries: Vec<(String, String)> = if matches!(kind, "pie" | "donut" | "doughnut" | "funnel") {
        cats.iter().enumerate().map(|(i, c)| (c.clone(), svg_palette(i))).collect()
    } else {
        series
            .iter()
            .enumerate()
            .filter_map(|(i, ser)| ser.get("name").and_then(Value::as_str).map(|n| (n.to_string(), svg_palette(i))))
            .collect()
    };
    if entries.is_empty() {
        return;
    }
    let maxlen = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(4) as f64;
    let x0 = w - (maxlen * 7.0 + 24.0) - 10.0;
    let mut y = t + 4.0;
    for (name, col) in &entries {
        let _ = write!(s, r##"<rect x="{x0}" y="{y}" width="12" height="12" fill="{col}"/>"##);
        let _ = write!(s, r##"<text x="{}" y="{}" font-size="12" fill="#1e1e1e">{}</text>"##, x0 + 16.0, y + 11.0, xml_escape(name));
        y += 18.0;
    }
}

fn svg_bars(s: &mut String, series: &[Value], cats: &[String], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, stacked: bool, labels: bool) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0).max(cats.len());
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    if stacked {
        let mut cum = vec![0.0f64; ncat];
        let bw = slot * 0.8;
        for (si, ser) in series.iter().enumerate() {
            let col = svg_palette(si);
            for (ci, v) in series_nums(ser).into_iter().enumerate() {
                let x = l + ci as f64 * slot + slot * 0.1;
                let y0 = yp(cum[ci] + v);
                let height = (yp(cum[ci]) - y0).max(0.0);
                let _ = write!(s, r##"<rect x="{x}" y="{y0}" width="{bw}" height="{height}" fill="{col}"/>"##);
                cum[ci] += v;
            }
        }
    } else {
        let bw = slot * 0.8 / nser as f64;
        let base = yp(0.0);
        for (si, ser) in series.iter().enumerate() {
            let col = svg_palette(si);
            for (ci, v) in series_nums(ser).into_iter().enumerate() {
                let x = l + ci as f64 * slot + slot * 0.1 + si as f64 * bw;
                let top = yp(v);
                let (y, height) = if top < base { (top, base - top) } else { (base, top - base) };
                let _ = write!(s, r##"<rect x="{x}" y="{y}" width="{bw}" height="{}" fill="{col}"/>"##, height.max(0.0));
                if labels {
                    let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" text-anchor="middle" fill="#1e1e1e">{}</text>"##, x + bw / 2.0, top - 3.0, xml_escape(&fmt_num(v)));
                }
            }
        }
    }
}

/// Stepped line (hold then jump).
fn svg_step(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64) {
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        let n = data.len();
        if n == 0 {
            continue;
        }
        let xat = |i: usize| l + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
        let mut d = format!("M {:.1},{:.1}", xat(0), yp(data[0]));
        for i in 1..n {
            let _ = write!(d, " H {:.1} V {:.1}", xat(i), yp(data[i]));
        }
        let _ = write!(s, r##"<path d="{d}" fill="none" stroke="{col}" stroke-width="2"/>"##);
    }
}

/// Combination chart: `kind:"line"` series as polylines, the rest as bars.
fn svg_combo(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, labels: bool) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let bar_idx: Vec<usize> = (0..series.len()).filter(|&i| series[i].get("kind").and_then(Value::as_str) != Some("line")).collect();
    let nbar = bar_idx.len().max(1);
    let bw = slot * 0.7 / nbar as f64;
    let base = yp(0.0);
    for (bi, &i) in bar_idx.iter().enumerate() {
        let col = svg_palette(i);
        for (ci, v) in series_nums(&series[i]).into_iter().enumerate() {
            let x = l + ci as f64 * slot + slot * 0.15 + bi as f64 * bw;
            let top = yp(v);
            let (y, height) = if top < base { (top, base - top) } else { (base, top - base) };
            let _ = write!(s, r##"<rect x="{x}" y="{y}" width="{bw}" height="{}" fill="{col}"/>"##, height.max(0.0));
            if labels {
                let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" text-anchor="middle" fill="#1e1e1e">{}</text>"##, x + bw / 2.0, top - 3.0, xml_escape(&fmt_num(v)));
            }
        }
    }
    for (i, ser) in series.iter().enumerate() {
        if ser.get("kind").and_then(Value::as_str) != Some("line") {
            continue;
        }
        let col = svg_palette(i);
        let pts: String = series_nums(ser).iter().enumerate().map(|(ci, v)| format!("{:.1},{:.1} ", l + ci as f64 * slot + slot * 0.5, yp(*v))).collect();
        let _ = write!(s, r##"<polyline points="{pts}" fill="none" stroke="{col}" stroke-width="2"/>"##);
    }
}

/// Waterfall ribbons from the first series' deltas.
fn svg_waterfall(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, labels: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let bw = slot * 0.7;
    let mut cum = 0.0;
    for (i, &v) in data.iter().enumerate() {
        let prev = cum;
        cum += v;
        let y = yp(prev.max(cum));
        let height = (yp(prev.min(cum)) - y).max(0.0);
        let x = l + i as f64 * slot + slot * 0.15;
        let col = if v >= 0.0 { "#55aa55" } else { "#cc5555" };
        let _ = write!(s, r##"<rect x="{x}" y="{y}" width="{bw}" height="{height}" fill="{col}"/>"##);
        if i + 1 < n {
            let yc = yp(cum);
            let _ = write!(s, r##"<line x1="{:.1}" y1="{yc:.1}" x2="{:.1}" y2="{yc:.1}" stroke="#969696"/>"##, x + bw, x + slot);
        }
        if labels {
            let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" text-anchor="middle" fill="#1e1e1e">{}</text>"##, x + bw / 2.0, y - 3.0, xml_escape(&fmt_num(v)));
        }
    }
}

/// OHLC / candlestick from `data:[[o,h,l,c],...]`.
fn svg_ohlc(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, candle: bool) {
    let data = series.first().map(series_ohlc).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let slot = pw / n as f64;
    let bw = (slot * 0.5).max(2.0);
    for (i, o) in data.iter().enumerate() {
        let [open, high, low, close] = *o;
        let cx = l + i as f64 * slot + slot * 0.5;
        let col = if close >= open { "#339955" } else { "#cc4444" };
        let _ = write!(s, r##"<line x1="{cx:.1}" y1="{:.1}" x2="{cx:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(high), yp(low));
        if candle {
            let (top, bot) = (open.max(close), open.min(close));
            let y = yp(top);
            let height = (yp(bot) - y).max(1.0);
            let fill = if close >= open { "none" } else { col };
            let _ = write!(s, r##"<rect x="{:.1}" y="{y:.1}" width="{bw:.1}" height="{height:.1}" fill="{fill}" stroke="{col}"/>"##, cx - bw / 2.0);
        } else {
            let _ = write!(s, r##"<line x1="{:.1}" y1="{:.1}" x2="{cx:.1}" y2="{:.1}" stroke="{col}"/>"##, cx - bw / 2.0, yp(open), yp(open));
            let _ = write!(s, r##"<line x1="{cx:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(close), cx + bw / 2.0, yp(close));
        }
    }
}

/// Box-and-whisker per series.
fn svg_boxplot(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64) {
    let n = series.len().max(1);
    let slot = pw / n as f64;
    let bw = (slot * 0.4).max(3.0);
    for (si, ser) in series.iter().enumerate() {
        let Some([mn, q1, med, q3, mx]) = five_number(&series_nums(ser)) else { continue };
        let col = svg_palette(si);
        let cx = l + si as f64 * slot + slot * 0.5;
        let (x0, x1) = (cx - bw / 2.0, cx + bw / 2.0);
        let _ = write!(s, r##"<line x1="{cx:.1}" y1="{:.1}" x2="{cx:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(mn), yp(q1));
        let _ = write!(s, r##"<line x1="{cx:.1}" y1="{:.1}" x2="{cx:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(q3), yp(mx));
        let _ = write!(s, r##"<line x1="{x0:.1}" y1="{:.1}" x2="{x1:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(mn), yp(mn));
        let _ = write!(s, r##"<line x1="{x0:.1}" y1="{:.1}" x2="{x1:.1}" y2="{:.1}" stroke="{col}"/>"##, yp(mx), yp(mx));
        let y = yp(q3);
        let height = (yp(q1) - y).max(1.0);
        let _ = write!(s, r##"<rect x="{x0:.1}" y="{y:.1}" width="{bw:.1}" height="{height:.1}" fill="none" stroke="{col}"/>"##);
        let _ = write!(s, r##"<line x1="{x0:.1}" y1="{:.1}" x2="{x1:.1}" y2="{:.1}" stroke="{col}" stroke-width="2"/>"##, yp(med), yp(med));
    }
}

/// Centered descending funnel from the first series.
#[allow(clippy::too_many_arguments)]
fn svg_funnel(s: &mut String, series: &[Value], cats: &[String], l: f64, t: f64, r: f64, b: f64, labels: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(0.0f64, f64::max).max(f64::EPSILON);
    let cx = (l + r) / 2.0;
    let fullw = (r - l) * 0.8;
    let band_h = (b - t) / n as f64 * 0.85;
    let gap = (b - t) / n as f64 * 0.15;
    for (i, &v) in data.iter().enumerate() {
        let half = fullw * (v / maxv) / 2.0;
        let y = t + i as f64 * (band_h + gap);
        let col = svg_palette(i);
        let _ = write!(s, r##"<rect x="{:.1}" y="{y:.1}" width="{:.1}" height="{band_h:.1}" fill="{col}"/>"##, cx - half, half * 2.0);
        if labels {
            let text = cats.get(i).map(|c| format!("{c}: {}", fmt_num(v))).unwrap_or_else(|| fmt_num(v));
            let _ = write!(s, r##"<text x="{cx:.1}" y="{:.1}" font-size="12" text-anchor="middle" fill="#1e1e1e">{}</text>"##, y + band_h / 2.0 + 4.0, xml_escape(&text));
        }
    }
}

/// Semicircular gauge.
fn svg_gauge(s: &mut String, opts: &Value, l: f64, t: f64, r: f64, b: f64) {
    let value = opts.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let max = opts.get("max").and_then(Value::as_f64).unwrap_or(100.0).max(f64::EPSILON);
    let frac = (value / max).clamp(0.0, 1.0);
    let cx = (l + r) / 2.0;
    let cy = (t + b) * 2.0 / 3.0;
    let radius = ((r - l).min((b - t) * 2.0) / 2.0 - 20.0).max(20.0);
    let arc = |s: &mut String, frac: f64, col: &str| {
        let a0 = std::f64::consts::PI;
        let a1 = a0 + frac * std::f64::consts::PI;
        let (x0, y0) = (cx + radius * a0.cos(), cy + radius * a0.sin());
        let (x1, y1) = (cx + radius * a1.cos(), cy + radius * a1.sin());
        let large = if frac > 0.5 { 1 } else { 0 };
        let _ = write!(s, r##"<path d="M {x0:.1},{y0:.1} A {radius:.1},{radius:.1} 0 {large},1 {x1:.1},{y1:.1}" fill="none" stroke="{col}" stroke-width="{:.1}"/>"##, radius * 0.28);
    };
    arc(s, 1.0, "#dcdcdc");
    arc(s, frac, &svg_palette(0));
    let _ = write!(s, r##"<text x="{cx:.1}" y="{:.1}" text-anchor="middle" font-size="22" fill="#1e1e1e">{}/{}</text>"##, cy - 6.0, xml_escape(&fmt_num(value)), xml_escape(&fmt_num(max)));
}

/// Heatmap grid colored white→blue.
fn svg_heatmap(s: &mut String, opts: &Value, cats: &[String], l: f64, t: f64, r: f64, b: f64) {
    let rows: Vec<Vec<f64>> = if let Some(m) = opts.get("matrix").and_then(Value::as_array) {
        m.iter().map(|row| row.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()).collect()
    } else {
        opts.get("series").and_then(Value::as_array).map(|sr| sr.iter().map(series_nums).collect()).unwrap_or_default()
    };
    let nr = rows.len();
    let nc = rows.iter().map(|x| x.len()).max().unwrap_or(0);
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
    let cw = (r - l) / nc as f64;
    let ch = (b - t) / nr as f64;
    for (ri, row) in rows.iter().enumerate() {
        for (ci, &v) in row.iter().enumerate() {
            let frac = ((v - lo) / span).clamp(0.0, 1.0);
            let col = format!("#{:02x}{:02x}ff", (255.0 - frac * 187.0) as u8, (255.0 - frac * 141.0) as u8);
            let _ = write!(s, r##"<rect x="{:.1}" y="{:.1}" width="{cw:.1}" height="{ch:.1}" fill="{col}"/>"##, l + ci as f64 * cw, t + ri as f64 * ch);
        }
    }
    for (ci, c) in cats.iter().enumerate().take(nc) {
        let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, l + ci as f64 * cw + cw * 0.2, b + 14.0, xml_escape(c));
    }
}

/// Stacked area (vector).
fn svg_stacked_area(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let xat = |i: usize| l + if ncat > 1 { i as f64 / (ncat - 1) as f64 * pw } else { pw / 2.0 };
    let mut cum = vec![0.0f64; ncat];
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        let mut pts = String::new();
        for ci in 0..ncat {
            let v = data.get(ci).copied().unwrap_or(0.0);
            let _ = write!(pts, "{:.1},{:.1} ", xat(ci), yp(cum[ci] + v));
        }
        for ci in (0..ncat).rev() {
            let _ = write!(pts, "{:.1},{:.1} ", xat(ci), yp(cum[ci]));
        }
        let _ = write!(s, r##"<polygon points="{pts}" fill="{col}" fill-opacity="0.6"/>"##);
        for ci in 0..ncat {
            cum[ci] += data.get(ci).copied().unwrap_or(0.0);
        }
    }
}

/// Treemap (vector) — shares the area-correct layout with the raster path.
fn svg_treemap(s: &mut String, series: &[Value], cats: &[String], l: f64, t: f64, r: f64, b: f64) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let mut items: Vec<(usize, f64)> = data.iter().enumerate().map(|(i, &v)| (i, v.max(0.0))).collect();
    items.retain(|&(_, v)| v > 0.0);
    if items.is_empty() {
        return;
    }
    items.sort_by(|a, c| c.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut placed: Vec<(usize, (f64, f64, f64, f64))> = Vec::new();
    treemap_layout(&items, (l, t, r - l, b - t), &mut placed);
    for (idx, (x, y, w, h)) in placed {
        let _ = write!(s, r##"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" fill="{}" stroke="#ffffff"/>"##, x + 1.0, y + 1.0, (w - 2.0).max(1.0), (h - 2.0).max(1.0), svg_palette(idx));
        if let Some(name) = cats.get(idx) {
            if w > 24.0 && h > 14.0 {
                let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, x + 4.0, y + 14.0, xml_escape(name));
            }
        }
    }
}

/// Polar / rose chart (vector).
fn svg_polar(s: &mut String, series: &[Value], cats: &[String], l: f64, t: f64, r: f64, b: f64) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let n = data.len();
    if n == 0 {
        return;
    }
    let maxv = data.iter().cloned().fold(f64::EPSILON, f64::max);
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let radius = ((r - l).min(b - t) / 2.0 - 20.0).max(20.0);
    for ring in 1..=4 {
        let _ = write!(s, r##"<circle cx="{cx:.1}" cy="{cy:.1}" r="{:.1}" fill="none" stroke="#d2d2d2"/>"##, radius * ring as f64 / 4.0);
    }
    let step = std::f64::consts::TAU / n as f64;
    for (i, &v) in data.iter().enumerate() {
        let a0 = -std::f64::consts::FRAC_PI_2 + i as f64 * step;
        let a1 = a0 + step * 0.9;
        let rr = v / maxv * radius;
        let (x0, y0) = (cx + rr * a0.cos(), cy + rr * a0.sin());
        let (x1, y1) = (cx + rr * a1.cos(), cy + rr * a1.sin());
        let large = if step * 0.9 > std::f64::consts::PI { 1 } else { 0 };
        let _ = write!(s, r##"<path d="M {cx:.1},{cy:.1} L {x0:.1},{y0:.1} A {rr:.1},{rr:.1} 0 {large},1 {x1:.1},{y1:.1} Z" fill="{}"/>"##, svg_palette(i));
        if let Some(c) = cats.get(i) {
            let am = (a0 + a1) / 2.0;
            let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" text-anchor="middle" fill="#1e1e1e">{}</text>"##, cx + (radius + 8.0) * am.cos(), cy + (radius + 8.0) * am.sin(), xml_escape(c));
        }
    }
}

/// Bullet graphs (vector).
fn svg_bullet(s: &mut String, series: &[Value], l: f64, t: f64, r: f64, b: f64) {
    let n = series.len().max(1);
    let row_h = ((b - t) / n as f64).max(12.0);
    for (si, ser) in series.iter().enumerate() {
        let value = ser.get("value").and_then(Value::as_f64).or_else(|| series_nums(ser).first().copied()).unwrap_or(0.0);
        let target = ser.get("target").and_then(Value::as_f64);
        let ranges: Vec<f64> = ser.get("ranges").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        let scale_max = ranges.iter().cloned().fold(value.max(target.unwrap_or(0.0)), f64::max).max(f64::EPSILON);
        let y0 = t + si as f64 * row_h + 4.0;
        let bh = (row_h - 10.0).max(6.0);
        let px = |v: f64| l + v / scale_max * (r - l);
        let mut prev = 0.0;
        for (ri, &rmax) in ranges.iter().enumerate() {
            let shade = (220 - ri as i32 * 40).clamp(120, 220);
            let _ = write!(s, r##"<rect x="{:.1}" y="{y0:.1}" width="{:.1}" height="{bh:.1}" fill="rgb({shade},{shade},{shade})"/>"##, px(prev), (px(rmax) - px(prev)).max(1.0));
            prev = rmax;
        }
        if ranges.is_empty() {
            let _ = write!(s, r##"<rect x="{l:.1}" y="{y0:.1}" width="{:.1}" height="{bh:.1}" fill="#d2d2d2"/>"##, r - l);
        }
        let mbh = (bh / 2.0).max(3.0);
        let _ = write!(s, r##"<rect x="{l:.1}" y="{:.1}" width="{:.1}" height="{mbh:.1}" fill="{}"/>"##, y0 + (bh - mbh) / 2.0, (px(value) - l).max(1.0), svg_palette(si));
        if let Some(tg) = target {
            let _ = write!(s, r##"<line x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}" stroke="#141414" stroke-width="2"/>"##, px(tg), y0 - 2.0, px(tg), y0 + bh + 2.0);
        }
        if let Some(name) = ser.get("name").and_then(Value::as_str) {
            let _ = write!(s, r##"<text x="{l:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, y0 - 3.0, xml_escape(name));
        }
    }
}

/// Pareto (vector): sorted bars (left axis) + cumulative-% line (right axis).
fn svg_pareto(s: &mut String, series: &[Value], cats: &[String], l: f64, t: f64, r: f64, b: f64) {
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
    let (pw, ph) = (r - l, b - t);
    let _ = write!(s, r##"<line x1="{l}" y1="{b}" x2="{r}" y2="{b}" stroke="#1e1e1e"/><line x1="{l}" y1="{t}" x2="{l}" y2="{b}" stroke="#1e1e1e"/><line x1="{r}" y1="{t}" x2="{r}" y2="{b}" stroke="#1e1e1e"/>"##);
    for i in 0..=5 {
        let y = b - (i as f64 / 5.0) * ph;
        let _ = write!(s, r##"<line x1="{l}" y1="{y:.1}" x2="{r}" y2="{y:.1}" stroke="#d2d2d2"/>"##);
        let _ = write!(s, r##"<text x="4" y="{:.1}" font-size="10" fill="#1e1e1e">{}</text>"##, y + 4.0, xml_escape(&fmt_num(maxv * i as f64 / 5.0)));
        let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="10" fill="#1e1e1e">{}%</text>"##, r + 2.0, y + 4.0, i * 20);
    }
    let slot = pw / pairs.len().max(1) as f64;
    let bw = slot * 0.7;
    let mut cum = 0.0;
    let mut pts = String::new();
    for (i, (name, v)) in pairs.iter().enumerate() {
        let x = l + i as f64 * slot + slot * 0.15;
        let bh = v / maxv * ph;
        let _ = write!(s, r##"<rect x="{x:.1}" y="{:.1}" width="{bw:.1}" height="{:.1}" fill="{}"/>"##, b - bh, bh.max(0.0), svg_palette(0));
        cum += v;
        let cy = b - (cum / total) * ph;
        let _ = write!(pts, "{:.1},{cy:.1} ", x + bw / 2.0);
        let _ = write!(s, r##"<text x="{x:.1}" y="{:.1}" font-size="10" fill="#1e1e1e">{}</text>"##, b + 14.0, xml_escape(name));
    }
    let _ = write!(s, r##"<polyline points="{pts}" fill="none" stroke="#c03a2b" stroke-width="2"/>"##);
}

/// Lollipop / dot plot (vector).
fn svg_lollipop(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, with_stem: bool) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let base = yp(0.0);
    let nser = series.len().max(1);
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (ci, v) in series_nums(ser).into_iter().enumerate() {
            let x = l + ci as f64 * slot + slot * 0.5 + (si as f64 - (nser as f64 - 1.0) / 2.0) * 6.0;
            let y = yp(v);
            if with_stem {
                let _ = write!(s, r##"<line x1="{x:.1}" y1="{base:.1}" x2="{x:.1}" y2="{y:.1}" stroke="{col}" stroke-width="2"/>"##);
            }
            let _ = write!(s, r##"<circle cx="{x:.1}" cy="{y:.1}" r="5" fill="{col}"/>"##);
        }
    }
}

/// Gantt (vector): one time bar per task `{name, start, end, color?}`.
fn svg_gantt(s: &mut String, series: &[Value], l: f64, t: f64, r: f64, b: f64) {
    let tasks: Vec<(&Value, f64, f64)> = series
        .iter()
        .filter_map(|x| Some((x, x.get("start").and_then(Value::as_f64)?, x.get("end").and_then(Value::as_f64)?)))
        .collect();
    if tasks.is_empty() {
        return;
    }
    let lo = tasks.iter().map(|x| x.1).fold(f64::INFINITY, f64::min);
    let hi = tasks.iter().map(|x| x.2).fold(f64::NEG_INFINITY, f64::max);
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let axis_l = l + 20.0;
    let map = |v: f64| axis_l + (v - lo) / span * (r - axis_l);
    let n = tasks.len();
    let row_h = ((b - t) / n as f64).max(10.0);
    let bh = (row_h - 6.0).max(4.0);
    for i in 0..=5 {
        let v = lo + span * i as f64 / 5.0;
        let x = map(v);
        let _ = write!(s, r##"<line x1="{x:.1}" y1="{t}" x2="{x:.1}" y2="{b}" stroke="#d2d2d2"/>"##);
        let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="10" fill="#1e1e1e">{}</text>"##, x - 8.0, b + 12.0, xml_escape(&fmt_num(v)));
    }
    for (i, (ser, start, end)) in tasks.iter().enumerate() {
        let y = t + i as f64 * row_h + 3.0;
        let (x0, x1) = (map(*start), map(*end));
        let col = ser.get("color").and_then(Value::as_str).map(String::from).unwrap_or_else(|| svg_palette(i));
        let _ = write!(s, r##"<rect x="{x0:.1}" y="{y:.1}" width="{:.1}" height="{bh:.1}" fill="{col}"/>"##, (x1 - x0).max(1.0));
        if let Some(name) = ser.get("name").and_then(Value::as_str) {
            let _ = write!(s, r##"<text x="2" y="{:.1}" font-size="10" fill="#1e1e1e">{}</text>"##, y + bh * 0.7, xml_escape(name));
        }
    }
}

/// Sunburst / multi-ring (vector). `rings` innermost first, or series data.
fn svg_sunburst(s: &mut String, opts: &Value, series: &[Value], l: f64, t: f64, r: f64, b: f64) {
    let rings: Vec<Vec<f64>> = if let Some(rs) = opts.get("rings").and_then(Value::as_array) {
        rs.iter().map(|row| row.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()).collect()
    } else {
        series.iter().map(series_nums).collect()
    };
    let nr = rings.len();
    if nr == 0 {
        return;
    }
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let rmax = ((r - l).min(b - t) / 2.0 - 10.0).max(20.0);
    let ring_w = rmax / nr as f64;
    let mut ci = 0usize;
    for (ri, ring) in rings.iter().enumerate() {
        let total: f64 = ring.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let inner = ri as f64 * ring_w;
        let outer = (ri + 1) as f64 * ring_w;
        let mut angle = -std::f64::consts::FRAC_PI_2;
        for &v in ring {
            let sweep = v / total * std::f64::consts::TAU;
            let a1 = angle + sweep;
            let large = if sweep > std::f64::consts::PI { 1 } else { 0 };
            let (ox0, oy0) = (cx + outer * angle.cos(), cy + outer * angle.sin());
            let (ox1, oy1) = (cx + outer * a1.cos(), cy + outer * a1.sin());
            let (ix1, iy1) = (cx + inner * a1.cos(), cy + inner * a1.sin());
            let (ix0, iy0) = (cx + inner * angle.cos(), cy + inner * angle.sin());
            let _ = write!(
                s,
                r##"<path d="M {ox0:.1},{oy0:.1} A {outer:.1},{outer:.1} 0 {large},1 {ox1:.1},{oy1:.1} L {ix1:.1},{iy1:.1} A {inner:.1},{inner:.1} 0 {large},0 {ix0:.1},{iy0:.1} Z" fill="{}"/>"##,
                svg_palette(ci)
            );
            ci += 1;
            angle = a1;
        }
    }
}

/// Floating range bars (vector) from `data:[[lo,hi],...]`.
fn svg_range(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64) {
    let ncat = series.iter().map(|x| series_pairs(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let nser = series.len().max(1);
    let slot = pw / ncat as f64;
    let bw = slot * 0.8 / nser as f64;
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (ci, (lo, hi)) in series_pairs(ser).into_iter().enumerate() {
            let x = l + ci as f64 * slot + slot * 0.1 + si as f64 * bw;
            let (y, height) = (yp(hi), (yp(lo) - yp(hi)).max(1.0));
            let _ = write!(s, r##"<rect x="{x:.1}" y="{y:.1}" width="{bw:.1}" height="{height:.1}" fill="{col}"/>"##);
        }
    }
}

/// 100%-stacked columns (vector).
fn svg_percent_stacked(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let slot = pw / ncat as f64;
    let bw = slot * 0.8;
    let totals: Vec<f64> = (0..ncat)
        .map(|ci| series.iter().map(|x| series_nums(x).get(ci).copied().unwrap_or(0.0).max(0.0)).sum::<f64>().max(f64::EPSILON))
        .collect();
    let mut cum = vec![0.0f64; ncat];
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (ci, v) in series_nums(ser).into_iter().enumerate() {
            let pct = v.max(0.0) / totals[ci] * 100.0;
            let x = l + ci as f64 * slot + slot * 0.1;
            let y = yp(cum[ci] + pct);
            let height = (yp(cum[ci]) - y).max(0.0);
            let _ = write!(s, r##"<rect x="{x:.1}" y="{y:.1}" width="{bw:.1}" height="{height:.1}" fill="{col}"/>"##);
            cum[ci] += pct;
        }
    }
}

/// Streamgraph (vector): centered stacked area.
fn svg_streamgraph(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, _ymin: f64, _ymax: f64) {
    let ncat = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0);
    if ncat == 0 {
        return;
    }
    let xat = |i: usize| l + if ncat > 1 { i as f64 / (ncat - 1) as f64 * pw } else { pw / 2.0 };
    let mut cum: Vec<f64> = (0..ncat)
        .map(|ci| -series.iter().map(|x| series_nums(x).get(ci).copied().unwrap_or(0.0)).sum::<f64>() / 2.0)
        .collect();
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        let mut pts = String::new();
        for ci in 0..ncat {
            let v = data.get(ci).copied().unwrap_or(0.0);
            let _ = write!(pts, "{:.1},{:.1} ", xat(ci), yp(cum[ci] + v));
        }
        for ci in (0..ncat).rev() {
            let _ = write!(pts, "{:.1},{:.1} ", xat(ci), yp(cum[ci]));
        }
        let _ = write!(s, r##"<polygon points="{pts}" fill="{col}" fill-opacity="0.7"/>"##);
        for ci in 0..ncat {
            cum[ci] += data.get(ci).copied().unwrap_or(0.0);
        }
    }
}

/// Waffle chart (vector): 10×10 grid colored by category share.
fn svg_waffle(s: &mut String, series: &[Value], l: f64, t: f64, r: f64, b: f64) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let side = (r - l).min(b - t).max(10.0);
    let cell = side / 10.0;
    let gap = (cell / 8.0).max(1.0);
    let mut counts: Vec<i32> = data.iter().map(|v| (v / total * 100.0).floor() as i32).collect();
    let mut assigned: i32 = counts.iter().sum();
    let mut rema: Vec<(usize, f64)> = data.iter().enumerate().map(|(i, v)| (i, (v / total * 100.0).fract())).collect();
    rema.sort_by(|a, c| c.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut ri = 0;
    while assigned < 100 && !rema.is_empty() {
        counts[rema[ri % rema.len()].0] += 1;
        assigned += 1;
        ri += 1;
    }
    let mut idx = 0usize;
    for (ci, &cnt) in counts.iter().enumerate() {
        for _ in 0..cnt {
            if idx >= 100 {
                break;
            }
            let (gx, gy) = (idx % 10, idx / 10);
            let x = l + gx as f64 * cell + gap;
            let y = b - (gy as f64 + 1.0) * cell + gap;
            let _ = write!(s, r##"<rect x="{x:.1}" y="{y:.1}" width="{:.1}" height="{:.1}" fill="{}"/>"##, (cell - 2.0 * gap).max(1.0), (cell - 2.0 * gap).max(1.0), svg_palette(ci));
            idx += 1;
        }
    }
}

/// Slope chart (vector): first→last value connected across two x positions.
fn svg_slope(s: &mut String, series: &[Value], l: f64, t: f64, r: f64, b: f64) {
    let vals: Vec<(&Value, f64, f64)> = series
        .iter()
        .filter_map(|x| {
            let d = series_nums(x);
            Some((x, *d.first()?, *d.last()?))
        })
        .collect();
    if vals.is_empty() {
        return;
    }
    let lo = vals.iter().flat_map(|v| [v.1, v.2]).fold(f64::INFINITY, f64::min);
    let hi = vals.iter().flat_map(|v| [v.1, v.2]).fold(f64::NEG_INFINITY, f64::max);
    let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
    let yv = |v: f64| b - (v - lo) / span * (b - t);
    let (x0, x1) = (l + 60.0, r - 60.0);
    let _ = write!(s, r##"<line x1="{x0:.1}" y1="{t}" x2="{x0:.1}" y2="{b}" stroke="#d2d2d2"/><line x1="{x1:.1}" y1="{t}" x2="{x1:.1}" y2="{b}" stroke="#d2d2d2"/>"##);
    for (si, (ser, a, c)) in vals.iter().enumerate() {
        let col = svg_palette(si);
        let _ = write!(s, r##"<line x1="{x0:.1}" y1="{:.1}" x2="{x1:.1}" y2="{:.1}" stroke="{col}" stroke-width="2"/>"##, yv(*a), yv(*c));
        let _ = write!(s, r##"<circle cx="{x0:.1}" cy="{:.1}" r="4" fill="{col}"/><circle cx="{x1:.1}" cy="{:.1}" r="4" fill="{col}"/>"##, yv(*a), yv(*c));
        if let Some(name) = ser.get("name").and_then(Value::as_str) {
            let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" text-anchor="end" fill="#1e1e1e">{}</text>"##, x0 - 6.0, yv(*a) + 4.0, xml_escape(name));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn svg_line_area(s: &mut String, series: &[Value], l: f64, pw: f64, yp: &dyn Fn(f64) -> f64, b: f64, fill: bool, smooth: bool) {
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        if data.is_empty() {
            continue;
        }
        let n = data.len();
        let xat = |i: usize| l + if n > 1 { i as f64 / (n - 1) as f64 * pw } else { pw / 2.0 };
        let verts: Vec<(f32, f32)> = if smooth && n >= 3 {
            catmull_rom(&(0..n).map(|i| (xat(i) as f32, yp(data[i]) as f32)).collect::<Vec<_>>(), 16)
        } else {
            (0..n).map(|i| (xat(i) as f32, yp(data[i]) as f32)).collect()
        };
        let pts: String = verts.iter().map(|(x, y)| format!("{x:.1},{y:.1} ")).collect();
        if fill {
            let _ = write!(s, r##"<polygon points="{:.1},{:.1} {pts}{:.1},{:.1}" fill="{col}" fill-opacity="0.45"/>"##, xat(0), b, xat(n - 1), b);
        }
        let _ = write!(s, r##"<polyline points="{pts}" fill="none" stroke="{col}" stroke-width="2"/>"##);
    }
}

fn svg_scatter(s: &mut String, series: &[Value], l: f64, pw: f64, xmin: f64, xmax: f64, yp: &dyn Fn(f64) -> f64) {
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (x, y) in series_points(ser) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            let _ = write!(s, r##"<circle cx="{px:.1}" cy="{:.1}" r="4" fill="{col}"/>"##, yp(y));
        }
    }
}

fn svg_bubble(s: &mut String, series: &[Value], l: f64, pw: f64, xmin: f64, xmax: f64, yp: &dyn Fn(f64) -> f64) {
    let maxs = series.iter().flat_map(series_points3).map(|(_, _, z)| z).fold(1.0f64, f64::max);
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        for (x, y, z) in series_points3(ser) {
            let px = l + (x - xmin) / (xmax - xmin) * pw;
            let r = ((z / maxs).sqrt() * 24.0).max(2.0);
            let _ = write!(s, r##"<circle cx="{px:.1}" cy="{:.1}" r="{r:.1}" fill="{col}" fill-opacity="0.6"/>"##, yp(y));
        }
    }
}

fn svg_radar(s: &mut String, series: &[Value], cats: &[String], w: f64, h: f64) {
    let nax = series.iter().map(|x| series_nums(x).len()).max().unwrap_or(0).max(cats.len());
    if nax < 3 {
        return;
    }
    let (cx, cy) = (w / 2.0, h / 2.0 + 8.0);
    let radius = (w.min(h) / 2.0 - 40.0).max(20.0);
    let maxv = series.iter().flat_map(series_nums).fold(1.0f64, f64::max);
    let ang = |i: usize| -std::f64::consts::FRAC_PI_2 + i as f64 / nax as f64 * std::f64::consts::TAU;
    for ring in 1..=4 {
        let rr = radius * ring as f64 / 4.0;
        let pts: String = (0..nax).map(|i| format!("{:.1},{:.1} ", cx + rr * ang(i).cos(), cy + rr * ang(i).sin())).collect();
        let _ = write!(s, r##"<polygon points="{pts}" fill="none" stroke="#d2d2d2"/>"##);
    }
    for i in 0..nax {
        let (x, y) = (cx + radius * ang(i).cos(), cy + radius * ang(i).sin());
        let _ = write!(s, r##"<line x1="{cx:.1}" y1="{cy:.1}" x2="{x:.1}" y2="{y:.1}" stroke="#d2d2d2"/>"##);
        if let Some(c) = cats.get(i) {
            let _ = write!(s, r##"<text x="{x:.1}" y="{y:.1}" font-size="10" fill="#1e1e1e">{}</text>"##, xml_escape(c));
        }
    }
    for (si, ser) in series.iter().enumerate() {
        let col = svg_palette(si);
        let data = series_nums(ser);
        let pts: String = data.iter().enumerate().map(|(i, v)| {
            let rr = v / maxv * radius;
            format!("{:.1},{:.1} ", cx + rr * ang(i).cos(), cy + rr * ang(i).sin())
        }).collect();
        let _ = write!(s, r##"<polygon points="{pts}" fill="{col}" fill-opacity="0.3" stroke="{col}" stroke-width="2"/>"##);
    }
}

fn svg_histogram(s: &mut String, series: &[Value], opts: &Value, l: f64, pw: f64, t: f64, b: f64) {
    let data = series.first().map(series_nums).unwrap_or_default();
    if data.is_empty() {
        return;
    }
    let nbins = opts.get("bins").and_then(Value::as_u64).unwrap_or(10).clamp(1, 200) as usize;
    let (lo, hi) = data.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, c), &v| (a.min(v), c.max(v)));
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
    let ph = (b - t).max(1.0);
    let slot = pw / nbins as f64;
    let col = svg_palette(0);
    for (i, &c) in counts.iter().enumerate() {
        let x = l + i as f64 * slot + slot * 0.05;
        let bh = c as f64 / maxc * ph;
        let _ = write!(s, r##"<rect x="{x}" y="{}" width="{}" height="{bh}" fill="{col}"/>"##, b - bh, slot * 0.9);
    }
}

fn svg_pie(s: &mut String, series: &[Value], cats: &[String], w: f64, h: f64, donut: bool, labels: bool) {
    let data = series.first().map(series_nums).unwrap_or_default();
    let total: f64 = data.iter().sum();
    if total <= 0.0 {
        return;
    }
    let (cx, cy) = (w / 2.0, h / 2.0 + 10.0);
    let radius = (w.min(h) / 2.0 - 40.0).max(20.0);
    let mut angle = -std::f64::consts::FRAC_PI_2;
    for (i, v) in data.iter().enumerate() {
        let sweep = v / total * std::f64::consts::TAU;
        let (x0, y0) = (cx + radius * angle.cos(), cy + radius * angle.sin());
        let a1 = angle + sweep;
        let (x1, y1) = (cx + radius * a1.cos(), cy + radius * a1.sin());
        let large = if sweep > std::f64::consts::PI { 1 } else { 0 };
        let _ = write!(
            s,
            r##"<path d="M {cx:.1},{cy:.1} L {x0:.1},{y0:.1} A {radius:.1},{radius:.1} 0 {large},1 {x1:.1},{y1:.1} Z" fill="{}"/>"##,
            svg_palette(i)
        );
        if labels {
            let mid = angle + sweep / 2.0;
            let (lx, ly) = (cx + radius * 0.65 * mid.cos(), cy + radius * 0.65 * mid.sin());
            let pct = v / total * 100.0;
            let text = cats.get(i).map(|c| format!("{c} {pct:.0}%")).unwrap_or_else(|| format!("{pct:.0}%"));
            let _ = write!(s, r##"<text x="{lx:.1}" y="{ly:.1}" text-anchor="middle" font-size="11" fill="#1e1e1e">{}</text>"##, xml_escape(&text));
        }
        angle = a1;
    }
    if donut {
        let _ = write!(s, r##"<circle cx="{cx:.1}" cy="{cy:.1}" r="{:.1}" fill="#ffffff"/>"##, radius * 0.55);
    }
}

/// Sankey: nodes `[{name}]`, links `[{source, target, value}]` (indices into
/// nodes). A simple two-stage layout — distinct source nodes on the left,
/// distinct targets on the right — with band widths proportional to value.
fn svg_sankey(s: &mut String, opts: &Value, w: f64, h: f64) {
    let nodes: Vec<String> = opts
        .get("nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|n| n.get("name").and_then(Value::as_str).map(String::from).unwrap_or_default()).collect())
        .unwrap_or_default();
    let links = opts.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    if links.is_empty() {
        return;
    }
    let val = |lk: &Value| lk.get("value").and_then(Value::as_f64).unwrap_or(0.0);
    let src = |lk: &Value| lk.get("source").and_then(Value::as_u64).unwrap_or(0) as usize;
    let tgt = |lk: &Value| lk.get("target").and_then(Value::as_u64).unwrap_or(0) as usize;

    let mut sources: Vec<usize> = links.iter().map(&src).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut targets: Vec<usize> = links.iter().map(&tgt).collect();
    targets.sort_unstable();
    targets.dedup();
    let total: f64 = links.iter().map(&val).sum();
    if total <= 0.0 {
        return;
    }
    let (lx, rx) = (80.0, w - 100.0);
    let (top, bot) = (50.0, h - 30.0);
    let avail = bot - top;
    let nodew = 16.0;

    // y positions of each source/target node stacked by total throughput.
    let band = |ids: &[usize], side_src: bool| -> std::collections::HashMap<usize, (f64, f64)> {
        let mut pos = std::collections::HashMap::new();
        let mut y = top;
        for &id in ids {
            let sum: f64 = links
                .iter()
                .filter(|lk| if side_src { src(lk) == id } else { tgt(lk) == id })
                .map(&val)
                .sum();
            let height = sum / total * avail;
            pos.insert(id, (y, height));
            y += height + 8.0;
        }
        pos
    };
    let spos = band(&sources, true);
    let tpos = band(&targets, false);

    // node rectangles + labels
    for (&id, &(y, ht)) in spos.iter() {
        let _ = write!(s, r##"<rect x="{lx}" y="{y:.1}" width="{nodew}" height="{ht:.1}" fill="#4472c4"/>"##);
        if let Some(name) = nodes.get(id) {
            let _ = write!(s, r##"<text x="{}" y="{:.1}" font-size="11" text-anchor="end" fill="#1e1e1e">{}</text>"##, lx - 4.0, y + ht / 2.0, xml_escape(name));
        }
    }
    for (&id, &(y, ht)) in tpos.iter() {
        let _ = write!(s, r##"<rect x="{:.1}" y="{y:.1}" width="{nodew}" height="{ht:.1}" fill="#ed7d31"/>"##, rx);
        if let Some(name) = nodes.get(id) {
            let _ = write!(s, r##"<text x="{:.1}" y="{:.1}" font-size="11" fill="#1e1e1e">{}</text>"##, rx + nodew + 4.0, y + ht / 2.0, xml_escape(name));
        }
    }

    // flow bands as cubic Bezier ribbons
    let mut soff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    let mut toff: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (i, lk) in links.iter().enumerate() {
        let v = val(lk);
        let bh = v / total * avail;
        let (sy0, _) = *spos.get(&src(lk)).unwrap_or(&(top, 0.0));
        let (ty0, _) = *tpos.get(&tgt(lk)).unwrap_or(&(top, 0.0));
        let so = soff.entry(src(lk)).or_insert(0.0);
        let sy = sy0 + *so;
        *so += bh;
        let to = toff.entry(tgt(lk)).or_insert(0.0);
        let ty = ty0 + *to;
        *to += bh;
        let x0 = lx + nodew;
        let xc = (x0 + rx) / 2.0;
        let _ = write!(
            s,
            r##"<path d="M {x0:.1},{sy:.1} C {xc:.1},{sy:.1} {xc:.1},{ty:.1} {rx:.1},{ty:.1} L {rx:.1},{:.1} C {xc:.1},{:.1} {xc:.1},{:.1} {x0:.1},{:.1} Z" fill="{}" fill-opacity="0.45"/>"##,
            ty + bh, ty + bh, sy + bh, sy + bh, svg_palette(i)
        );
    }
}

fn op_chart_svg(opts: Value) -> Result<Value> {
    let svg = chart_to_svg(&opts)?;
    if let Some(path) = opts.get("path").and_then(Value::as_str) {
        std::fs::write(path, &svg)?;
        return Ok(json!({"ok": true, "path": path, "bytes": svg.len()}));
    }
    Ok(json!({"svg": svg}))
}

/// Render a chart to ANY format, chosen by the path extension:
///   .svg            -> vector SVG
///   .pdf            -> chart embedded (as JPEG) in a one-page PDF
///   .png/.jpg/.jpeg/.tif/.tiff/.bmp/.webp/.gif -> raster via chart_render
fn op_chart_save(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?.to_string();
    let ext = ext_of(&path);
    if ext == "svg" {
        let svg = chart_to_svg(&opts)?;
        std::fs::write(&path, &svg)?;
        return Ok(json!({"ok": true, "path": path, "format": "svg"}));
    }
    // Raster the chart to an image handle, then either save directly or embed
    // into a PDF.
    let rendered = op_chart_render(opts.clone())?;
    let handle = rendered
        .get("handle")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("chart render produced no handle"))?;
    let result = (|| {
        if ext == "pdf" {
            let jpeg = with_image(handle, |img| {
                let mut buf = std::io::Cursor::new(Vec::new());
                img.to_rgb8()
                    .write_to(&mut buf, image::ImageFormat::Jpeg)
                    .map_err(|e| anyhow!("encode jpeg: {e}"))?;
                Ok(buf.into_inner())
            })?;
            let (w, h) = with_image(handle, |img| {
                use image::GenericImageView;
                Ok(img.dimensions())
            })?;
            let pdf = pdf_with_jpeg(&jpeg, w, h);
            std::fs::write(&path, pdf)?;
            Ok(json!({"ok": true, "path": path, "format": "pdf"}))
        } else {
            with_image(handle, |img| {
                img.save(&path).map_err(|e| anyhow!("save {path}: {e}"))?;
                Ok(json!({"ok": true, "path": path, "format": ext}))
            })
        }
    })();
    images().lock().remove(&handle);
    result
}

/// Minimal single-page PDF embedding a JPEG (DCTDecode) at full page size.
fn pdf_with_jpeg(jpeg: &[u8], iw: u32, ih: u32) -> Vec<u8> {
    // A4-ish page sized to the image aspect at 72dpi-ish scaling.
    let pw = 595.0;
    let ph = pw * ih as f64 / iw.max(1) as f64;
    let mut objs: Vec<Vec<u8>> = Vec::new();
    objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objs.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    objs.push(
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.0} {ph:.0}] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>")
            .into_bytes(),
    );
    let mut im = format!(
        "<< /Type /XObject /Subtype /Image /Width {iw} /Height {ih} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        jpeg.len()
    )
    .into_bytes();
    im.extend_from_slice(jpeg);
    im.extend_from_slice(b"\nendstream");
    objs.push(im);
    let content = format!("q {pw:.0} 0 0 {ph:.0} 0 0 cm /Im0 Do Q");
    objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).into_bytes());

    let mut out = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF", objs.len() + 1).as_bytes(),
    );
    out
}
