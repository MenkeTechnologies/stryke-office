// Plain-text office formats: CSV/TSV spreadsheets and HTML/Markdown/RTF/TXT
// documents. All pure string work — no extra crates. Wired into the same
// sheet_read/sheet_write/doc_read/doc_write dispatch as the binary formats.

// ── CSV / TSV ────────────────────────────────────────────────────────────────

fn csv_field(s: &str, delim: char) -> String {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn write_csv(path: &str, sheets: &[(String, Vec<Vec<Value>>)], delim: char) -> Result<()> {
    let mut out = String::new();
    if let Some((_, rows)) = sheets.first() {
        for row in rows {
            let line: Vec<String> = row
                .iter()
                .map(|c| {
                    let s = match c {
                        Value::Object(o) => cell_to_string(o.get("v").or_else(|| o.get("value")).unwrap_or(&Value::Null)),
                        other => cell_to_string(other),
                    };
                    csv_field(&s, delim)
                })
                .collect();
            out.push_str(&line.join(&delim.to_string()));
            out.push('\n');
        }
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Display string for a sheet cell, unwrapping `{v}`/`{value}` rich cells.
fn sheet_cell_disp(c: &Value) -> String {
    match c {
        Value::Object(o) => {
            cell_to_string(o.get("v").or_else(|| o.get("value")).unwrap_or(&Value::Null))
        }
        other => cell_to_string(other),
    }
}

/// Render the first sheet as an HTML `<table>` (first row → `<th>`, rest `<td>`).
fn write_sheet_html(path: &str, sheets: &[(String, Vec<Vec<Value>>)]) -> Result<()> {
    let mut out = String::from("<table>\n");
    if let Some((_, rows)) = sheets.first() {
        for (i, row) in rows.iter().enumerate() {
            let tag = if i == 0 { "th" } else { "td" };
            out.push_str("<tr>");
            for c in row {
                out.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(&sheet_cell_disp(c))));
            }
            out.push_str("</tr>\n");
        }
    }
    out.push_str("</table>\n");
    std::fs::write(path, out)?;
    Ok(())
}

/// Render the first sheet as a GitHub-flavored Markdown table (first row is the
/// header). Cells with `|`/newlines are escaped so the table stays intact.
fn write_sheet_md(path: &str, sheets: &[(String, Vec<Vec<Value>>)]) -> Result<()> {
    let mut out = String::new();
    if let Some((_, rows)) = sheets.first() {
        let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let esc = |s: String| s.replace('|', "\\|").replace('\n', " ");
        for (i, row) in rows.iter().enumerate() {
            let cells: Vec<String> = (0..ncols)
                .map(|c| esc(row.get(c).map(sheet_cell_disp).unwrap_or_default()))
                .collect();
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
            if i == 0 {
                out.push_str(&format!("| {} |\n", vec!["---"; ncols].join(" | ")));
            }
        }
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Minimal RFC-4180-ish CSV parser (handles quotes, doubled quotes, embedded
/// newlines). Numeric-looking fields come back as JSON numbers.
fn read_csv(path: &str, delim: char) -> Result<Value> {
    let text = std::fs::read_to_string(path)?;
    let mut rows: Vec<Value> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    let push_field = |row: &mut Vec<Value>, field: &mut String| {
        let f = std::mem::take(field);
        row.push(match f.parse::<f64>() {
            Ok(n) if !f.is_empty() => json!(n),
            _ => Value::String(f),
        });
    };
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == delim {
            push_field(&mut row, &mut field);
        } else if c == '\n' {
            push_field(&mut row, &mut field);
            rows.push(Value::Array(std::mem::take(&mut row)));
        } else if c != '\r' {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        push_field(&mut row, &mut field);
        rows.push(Value::Array(row));
    }
    Ok(json!({ "sheets": [{ "name": "Sheet1", "rows": rows }] }))
}

// ── HTML / Markdown / RTF / TXT documents ────────────────────────────────────

/// Iterate a block's runs (or its single text) yielding (text, bold, italic).
fn block_runs(b: &Value) -> Vec<(String, bool, bool)> {
    let one = |v: &Value| {
        (
            v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            v.get("bold").and_then(flag_of).unwrap_or(false),
            v.get("italic").and_then(flag_of).unwrap_or(false),
        )
    };
    if let Some(runs) = b.get("runs").and_then(Value::as_array) {
        runs.iter().map(one).collect()
    } else {
        vec![one(b)]
    }
}

fn write_doc_html(path: &str, blocks: &[Value]) -> Result<()> {
    let mut s = String::from("<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"></head>\n<body>\n");
    for b in blocks {
        match b.get("kind").and_then(Value::as_str).unwrap_or("para") {
            "heading" => {
                let lv = b.get("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 6);
                s.push_str(&format!("<h{lv}>{}</h{lv}>\n", xml_escape(&block_plain_text(b))));
            }
            "pagebreak" => s.push_str("<hr>\n"),
            "image" => {
                if let Some(p) = b.get("path").and_then(Value::as_str) {
                    s.push_str(&format!("<img src=\"{}\">\n", xml_escape(p)));
                }
            }
            "table" => {
                s.push_str("<table border=\"1\">\n");
                if let Some(rows) = b.get("rows").and_then(Value::as_array) {
                    for row in rows {
                        s.push_str("<tr>");
                        if let Some(cells) = row.as_array() {
                            for c in cells {
                                s.push_str(&format!("<td>{}</td>", xml_escape(&block_plain_text(c))));
                            }
                        }
                        s.push_str("</tr>\n");
                    }
                }
                s.push_str("</table>\n");
            }
            _ => {
                s.push_str("<p>");
                for (text, b_, i_) in block_runs(b) {
                    let mut t = xml_escape(&text);
                    if b_ {
                        t = format!("<b>{t}</b>");
                    }
                    if i_ {
                        t = format!("<i>{t}</i>");
                    }
                    s.push_str(&t);
                }
                s.push_str("</p>\n");
            }
        }
    }
    s.push_str("</body>\n</html>\n");
    std::fs::write(path, s)?;
    Ok(())
}

fn write_doc_md(path: &str, blocks: &[Value]) -> Result<()> {
    let mut s = String::new();
    for b in blocks {
        match b.get("kind").and_then(Value::as_str).unwrap_or("para") {
            "heading" => {
                let lv = b.get("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 6) as usize;
                s.push_str(&format!("{} {}\n\n", "#".repeat(lv), block_plain_text(b)));
            }
            "pagebreak" => s.push_str("---\n\n"),
            "image" => {
                if let Some(p) = b.get("path").and_then(Value::as_str) {
                    s.push_str(&format!("![]({p})\n\n"));
                }
            }
            "table" => {
                if let Some(rows) = b.get("rows").and_then(Value::as_array) {
                    for (ri, row) in rows.iter().enumerate() {
                        if let Some(cells) = row.as_array() {
                            let line: Vec<String> = cells.iter().map(block_plain_text).collect();
                            s.push_str(&format!("| {} |\n", line.join(" | ")));
                            if ri == 0 {
                                s.push_str(&format!("|{}|\n", " --- |".repeat(cells.len())));
                            }
                        }
                    }
                    s.push('\n');
                }
            }
            _ => {
                for (text, b_, i_) in block_runs(b) {
                    let mut t = text;
                    if b_ {
                        t = format!("**{t}**");
                    }
                    if i_ {
                        t = format!("_{t}_");
                    }
                    s.push_str(&t);
                }
                s.push_str("\n\n");
            }
        }
    }
    std::fs::write(path, s)?;
    Ok(())
}

fn rtf_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('{', "\\{").replace('}', "\\}")
}

fn write_doc_rtf(path: &str, blocks: &[Value]) -> Result<()> {
    let mut s = String::from("{\\rtf1\\ansi\\deff0\n");
    for b in blocks {
        match b.get("kind").and_then(Value::as_str).unwrap_or("para") {
            "pagebreak" => s.push_str("\\page\n"),
            "heading" => {
                s.push_str(&format!("{{\\b\\fs32 {}}}\\par\n", rtf_escape(&block_plain_text(b))));
            }
            _ => {
                for (text, b_, i_) in block_runs(b) {
                    let mut ctrl = String::new();
                    if b_ {
                        ctrl.push_str("\\b ");
                    }
                    if i_ {
                        ctrl.push_str("\\i ");
                    }
                    s.push_str(&format!("{{{ctrl}{}}}", rtf_escape(&text)));
                }
                s.push_str("\\par\n");
            }
        }
    }
    s.push('}');
    std::fs::write(path, s)?;
    Ok(())
}

/// Read text/markdown/html/rtf as a list of paragraph strings.
fn read_doc_text(path: &str, ext: &str) -> Result<Value> {
    let raw = std::fs::read_to_string(path)?;
    let paras: Vec<Value> = match ext {
        "html" | "htm" => {
            // crude tag strip, splitting on block close tags
            let normalized = raw
                .replace("</p>", "\n")
                .replace("</h1>", "\n")
                .replace("</h2>", "\n")
                .replace("</h3>", "\n")
                .replace("</li>", "\n")
                .replace("<br>", "\n")
                .replace("<br/>", "\n");
            let mut out = String::new();
            let mut in_tag = false;
            for c in normalized.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => out.push(c),
                    _ => {}
                }
            }
            out.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| Value::String(l.to_string())).collect()
        }
        "rtf" => {
            // strip RTF control words and groups
            let mut out = String::new();
            let mut chars = raw.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        while let Some(&n) = chars.peek() {
                            if n.is_alphanumeric() {
                                chars.next();
                            } else {
                                if n == ' ' {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    '{' | '}' => {}
                    _ => out.push(c),
                }
            }
            out.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| Value::String(l.to_string())).collect()
        }
        _ => raw.lines().filter(|l| !l.trim().is_empty()).map(|l| Value::String(l.to_string())).collect(),
    };
    Ok(json!({ "paragraphs": paras }))
}
