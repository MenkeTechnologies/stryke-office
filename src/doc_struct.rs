// Structured reads from word-processor documents — the read-side mirror of the
// `doc_write` block model. `doc_read` flattens a document to a paragraph list;
// these recover the structure that flattening drops. Pure `quick-xml` over the
// already-unzipped part, no extra crates.

/// The element names that delimit a table in a given format. docx tables live
/// in `word/document.xml`; odt tables in `content.xml`.
struct TableTags {
    table: &'static [u8],
    row: &'static [u8],
    cell: &'static [u8],
    /// Paragraph boundary inside a cell — used to join multi-paragraph cells
    /// with a newline rather than running the text together.
    para: &'static [u8],
}

impl TableTags {
    const DOCX: TableTags = TableTags {
        table: b"w:tbl",
        row: b"w:tr",
        cell: b"w:tc",
        para: b"w:p",
    };
    const ODT: TableTags = TableTags {
        table: b"table:table",
        row: b"table:table-row",
        cell: b"table:table-cell",
        para: b"text:p",
    };
}

/// Walk `xml` and return every table as `{ rows: [[cell, …], …] }`.
///
/// `table`/`row`/`cell` boundaries are tracked by depth so the structure of
/// flat tables (what `doc_write` emits, and the overwhelming common case) is
/// recovered exactly. A nested table's structural rows/cells are ignored and
/// its text folds into the enclosing cell — a graceful degradation rather than
/// a corrupted grid.
fn extract_tables(xml: &[u8], t: TableTags) -> Vec<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut tables: Vec<Value> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();

    let mut table_depth = 0i32;
    let mut capturing = false; // inside an outermost-table cell

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = e.name();
                let n = n.as_ref();
                if n == t.table {
                    table_depth += 1;
                    if table_depth == 1 {
                        rows = Vec::new();
                    }
                } else if table_depth == 1 && n == t.row {
                    row = Vec::new();
                } else if table_depth == 1 && n == t.cell {
                    cell = String::new();
                    capturing = true;
                } else if capturing && n == t.para && !cell.is_empty() {
                    cell.push('\n');
                }
            }
            Ok(Event::Text(e)) => {
                if capturing {
                    if let Ok(txt) = e.xml10_content() {
                        cell.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let n = e.name();
                let n = n.as_ref();
                if table_depth == 1 && n == t.cell {
                    capturing = false;
                    row.push(std::mem::take(&mut cell));
                } else if table_depth == 1 && n == t.row {
                    rows.push(Value::Array(
                        std::mem::take(&mut row).into_iter().map(Value::String).collect(),
                    ));
                } else if n == t.table {
                    table_depth -= 1;
                    if table_depth == 0 {
                        tables.push(json!({ "rows": std::mem::take(&mut rows) }));
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    tables
}

/// Extract every table from a docx/odt as row-major string grids. Returns
/// `{ tables: [{ rows: [[cell, …], …] }, …], count }`.
fn op_doc_tables(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let (xml, tags) = match ext_of(path).as_str() {
        "docx" => (read_zip_entry(&bytes, "word/document.xml")?, TableTags::DOCX),
        "odt" => (read_zip_entry(&bytes, "content.xml")?, TableTags::ODT),
        other => return Err(anyhow!("unsupported document table format: {other}")),
    };
    let tables = extract_tables(&xml, tags);
    Ok(json!({ "tables": tables, "count": tables.len() }))
}

// ── ordered structural read (headings + paragraphs + tables in document order) ─

/// Map a paragraph style name to a heading level. `Heading1`..`Heading9` →
/// 1..9 (`Heading` with no digit → 1); anything else is body text.
fn heading_level_from_style(style: &str) -> Option<u64> {
    let lower = style.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("heading")?;
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    Some(digits.parse().unwrap_or(1).clamp(1, 9))
}

/// Append a `table_depth == 1` table grid as a `{kind:"table", rows}` block.
/// Shared by both readers; mutates the running row/cell/grid state.
fn flush_table_block(rows: &mut Vec<Value>, blocks: &mut Vec<Value>) {
    blocks.push(json!({ "kind": "table", "rows": std::mem::take(rows) }));
}

/// Ordered structural read of a docx body: emits `heading`/`para`/`table`
/// blocks in document order. Paragraphs nested in table cells are folded into
/// the table grid, not emitted as top-level blocks.
fn extract_blocks_docx(xml: &[u8]) -> Vec<Value> {
    use quick_xml::events::{BytesStart, Event};
    let t = TableTags::DOCX;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut blocks: Vec<Value> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut table_depth = 0i32;
    let mut capturing_cell = false;

    let mut in_para = false;
    let mut para_text = String::new();
    let mut heading: Option<u64> = None;

    // <w:pStyle w:val="Heading1"/> may arrive as Start or Empty.
    let note_style = |e: &BytesStart, heading: &mut Option<u64>| {
        if e.name().as_ref() == b"w:pStyle" {
            if let Some(v) = attr(e, b"w:val") {
                if let Some(level) = heading_level_from_style(&v) {
                    *heading = Some(level);
                }
            }
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let n = name.as_ref();
                if n == t.table {
                    table_depth += 1;
                    if table_depth == 1 {
                        rows = Vec::new();
                    }
                } else if table_depth >= 1 {
                    if table_depth == 1 && n == t.row {
                        row = Vec::new();
                    } else if table_depth == 1 && n == t.cell {
                        cell = String::new();
                        capturing_cell = true;
                    } else if capturing_cell && n == t.para && !cell.is_empty() {
                        cell.push('\n');
                    }
                } else if n == b"w:p" {
                    in_para = true;
                    para_text.clear();
                    heading = None;
                } else if in_para {
                    note_style(&e, &mut heading);
                }
            }
            Ok(Event::Empty(e)) => {
                if in_para && table_depth == 0 {
                    note_style(&e, &mut heading);
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(txt) = e.xml10_content() {
                    if table_depth >= 1 {
                        if capturing_cell {
                            cell.push_str(&txt);
                        }
                    } else if in_para {
                        para_text.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let n = name.as_ref();
                if table_depth >= 1 {
                    if table_depth == 1 && n == t.cell {
                        capturing_cell = false;
                        row.push(std::mem::take(&mut cell));
                    } else if table_depth == 1 && n == t.row {
                        rows.push(Value::Array(
                            std::mem::take(&mut row).into_iter().map(Value::String).collect(),
                        ));
                    } else if n == t.table {
                        table_depth -= 1;
                        if table_depth == 0 {
                            flush_table_block(&mut rows, &mut blocks);
                        }
                    }
                } else if n == b"w:p" && in_para {
                    in_para = false;
                    let text = std::mem::take(&mut para_text);
                    match heading {
                        Some(level) => {
                            blocks.push(json!({"kind":"heading","level":level,"text":text}))
                        }
                        None => blocks.push(json!({"kind":"para","text":text})),
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

/// Ordered structural read of an odt body: `text:h` → heading (level from
/// `text:outline-level`), `text:p` → para, `table:table` → table.
fn extract_blocks_odt(xml: &[u8]) -> Vec<Value> {
    use quick_xml::events::Event;
    let t = TableTags::ODT;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();

    let mut blocks: Vec<Value> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut table_depth = 0i32;
    let mut capturing_cell = false;

    let mut in_para = false;
    let mut para_text = String::new();
    let mut heading: Option<u64> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let n = name.as_ref();
                if n == t.table {
                    table_depth += 1;
                    if table_depth == 1 {
                        rows = Vec::new();
                    }
                } else if table_depth >= 1 {
                    if table_depth == 1 && n == t.row {
                        row = Vec::new();
                    } else if table_depth == 1 && n == t.cell {
                        cell = String::new();
                        capturing_cell = true;
                    } else if capturing_cell && n == t.para && !cell.is_empty() {
                        cell.push('\n');
                    }
                } else if n == b"text:h" {
                    in_para = true;
                    para_text.clear();
                    heading = Some(
                        attr(&e, b"text:outline-level")
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .unwrap_or(1)
                            .clamp(1, 9),
                    );
                } else if n == t.para {
                    in_para = true;
                    para_text.clear();
                    heading = None;
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(txt) = e.xml10_content() {
                    if table_depth >= 1 {
                        if capturing_cell {
                            cell.push_str(&txt);
                        }
                    } else if in_para {
                        para_text.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let n = name.as_ref();
                if table_depth >= 1 {
                    if table_depth == 1 && n == t.cell {
                        capturing_cell = false;
                        row.push(std::mem::take(&mut cell));
                    } else if table_depth == 1 && n == t.row {
                        rows.push(Value::Array(
                            std::mem::take(&mut row).into_iter().map(Value::String).collect(),
                        ));
                    } else if n == t.table {
                        table_depth -= 1;
                        if table_depth == 0 {
                            flush_table_block(&mut rows, &mut blocks);
                        }
                    }
                } else if in_para && (n == b"text:h" || n == t.para) {
                    in_para = false;
                    let text = std::mem::take(&mut para_text);
                    match heading {
                        Some(level) => {
                            blocks.push(json!({"kind":"heading","level":level,"text":text}))
                        }
                        None => blocks.push(json!({"kind":"para","text":text})),
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

/// The heading outline of a docx/odt document: `{ outline: [{ level, text }],
/// count }` in document order. The document analogue of `pdf_outline`; useful
/// for navigation or generating a table of contents.
fn op_doc_outline(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let blocks = doc_blocks_or_paras(path)?;
    let outline: Vec<Value> = blocks
        .iter()
        .filter(|b| b.get("kind").and_then(Value::as_str) == Some("heading"))
        .map(|b| {
            json!({
                "level": b.get("level").and_then(Value::as_u64).unwrap_or(1),
                "text": b.get("text").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    Ok(json!({ "count": outline.len(), "outline": outline }))
}

// ── html -> blocks ────────────────────────────────────────────────────────────

/// Heading level for an `h1`..`h6` tag name (case-insensitive).
fn html_heading_level(name: &str) -> Option<u64> {
    let b = name.as_bytes();
    (b.len() == 2 && (b[0] == b'h' || b[0] == b'H') && (b'1'..=b'6').contains(&b[1]))
        .then(|| (b[1] - b'0') as u64)
}

/// Parse an HTML subset (h1-h6, p, table/tr/td-th, ul/ol/li) into `doc_write`
/// blocks. Inline markup (b/i/…) is flattened to text; void/unclosed tags are
/// tolerated (the reader runs with end-name checking off).
fn parse_html_blocks(xml: &[u8]) -> Vec<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut blocks: Vec<Value> = Vec::new();

    let mut in_table = 0i32;
    let (mut rows, mut row, mut cell) = (Vec::<Value>::new(), Vec::<String>::new(), String::new());
    let mut in_cell = false;
    let mut in_list = 0i32;
    let mut ordered = false;
    let (mut items, mut item) = (Vec::<Value>::new(), String::new());
    let mut in_li = false;
    let mut in_block = false; // inside a top-level p / hN
    let mut block_level: u64 = 0; // 0 = paragraph, 1..6 = heading
    let mut cur = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_lowercase();
                match name.as_str() {
                    "table" => {
                        in_table += 1;
                        if in_table == 1 {
                            rows = Vec::new();
                        }
                    }
                    "tr" if in_table >= 1 => row = Vec::new(),
                    "td" | "th" if in_table >= 1 => {
                        cell = String::new();
                        in_cell = true;
                    }
                    "ul" | "ol" => {
                        in_list += 1;
                        if in_list == 1 {
                            ordered = name == "ol";
                            items = Vec::new();
                        }
                    }
                    "li" if in_list >= 1 => {
                        item = String::new();
                        in_li = true;
                    }
                    _ => {
                        if in_table == 0 && in_list == 0 {
                            if name == "p" {
                                in_block = true;
                                block_level = 0;
                                cur.clear();
                            } else if let Some(lv) = html_heading_level(&name) {
                                in_block = true;
                                block_level = lv;
                                cur.clear();
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.xml10_content() {
                    if in_cell {
                        cell.push_str(&t);
                    } else if in_li {
                        item.push_str(&t);
                    } else if in_block {
                        cur.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_lowercase();
                match name.as_str() {
                    "td" | "th" if in_table >= 1 => {
                        in_cell = false;
                        row.push(std::mem::take(&mut cell));
                    }
                    "tr" if in_table >= 1 => rows.push(Value::Array(
                        std::mem::take(&mut row).into_iter().map(Value::String).collect(),
                    )),
                    "table" => {
                        in_table -= 1;
                        if in_table == 0 {
                            blocks.push(json!({ "kind": "table", "rows": std::mem::take(&mut rows) }));
                        }
                    }
                    "li" if in_list >= 1 => {
                        in_li = false;
                        items.push(Value::String(std::mem::take(&mut item)));
                    }
                    "ul" | "ol" => {
                        in_list -= 1;
                        if in_list == 0 {
                            blocks.push(json!({
                                "kind": "list",
                                "ordered": ordered,
                                "items": std::mem::take(&mut items),
                            }));
                        }
                    }
                    _ if in_block && (name == "p" || html_heading_level(&name).is_some()) => {
                        in_block = false;
                        let text = std::mem::take(&mut cur).split_whitespace().collect::<Vec<_>>().join(" ");
                        if block_level == 0 {
                            blocks.push(json!({ "kind": "para", "text": text }));
                        } else {
                            blocks.push(json!({ "kind": "heading", "level": block_level, "text": text }));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

/// Convert an HTML file into a document, preserving headings, paragraphs,
/// lists, and tables. opts: input (.html), output (target; format from ext —
/// docx/odt/pdf/md/…), format => override. Returns `{ ok, path, blocks }`.
fn op_html_to_doc(opts: Value) -> Result<Value> {
    let input = req_str(&opts, "input")?;
    let output = req_str(&opts, "output")?.to_string();
    let bytes = std::fs::read(input)?;
    let blocks = parse_html_blocks(&bytes);
    let n = blocks.len();
    let mut wopts = json!({ "path": output, "blocks": blocks });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_doc_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "blocks": n }))
}

// ── markdown -> blocks ────────────────────────────────────────────────────────

/// Parse an ATX heading line (`# `..`###### `) into a heading block.
fn md_heading(t: &str) -> Option<Value> {
    if !t.starts_with('#') {
        return None;
    }
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) || !t[hashes..].starts_with(' ') {
        return None;
    }
    Some(json!({ "kind": "heading", "level": hashes as u64, "text": t[hashes..].trim() }))
}

/// Parse a list item line, returning (ordered, text).
fn md_list_item(t: &str) -> Option<(bool, String)> {
    for p in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(p) {
            return Some((false, r.trim().to_string()));
        }
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        if let Some(r) = t[digits.len()..].strip_prefix(". ") {
            return Some((true, r.trim().to_string()));
        }
    }
    None
}

/// Is this line a Markdown table separator (`| --- | :--: |`)?
fn md_separator(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.contains('-') && s.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Split a Markdown table row into a JSON array of trimmed cell strings.
fn md_row(t: &str) -> Value {
    let t = t.trim().trim_start_matches('|').trim_end_matches('|');
    Value::Array(
        t.split('|')
            .map(|c| json!(c.trim().replace("\\|", "|")))
            .collect(),
    )
}

fn md_flush_para(para: &mut Vec<String>, blocks: &mut Vec<Value>) {
    if !para.is_empty() {
        blocks.push(json!({ "kind": "para", "text": std::mem::take(para).join(" ") }));
    }
}

/// Parse a Markdown subset into `doc_write` blocks: ATX headings, bullet/ordered
/// lists, pipe tables, and blank-line-separated paragraphs.
fn parse_markdown_blocks(text: &str) -> Vec<Value> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<Value> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() {
            md_flush_para(&mut para, &mut blocks);
            i += 1;
        } else if let Some(h) = md_heading(t) {
            md_flush_para(&mut para, &mut blocks);
            blocks.push(h);
            i += 1;
        } else if t.starts_with('|') && i + 1 < lines.len() && md_separator(lines[i + 1]) {
            md_flush_para(&mut para, &mut blocks);
            let mut rows = vec![md_row(t)];
            i += 2;
            while i < lines.len() && lines[i].trim().starts_with('|') {
                rows.push(md_row(lines[i].trim()));
                i += 1;
            }
            blocks.push(json!({ "kind": "table", "rows": rows }));
        } else if let Some((ordered, item)) = md_list_item(t) {
            md_flush_para(&mut para, &mut blocks);
            let mut items = vec![Value::String(item)];
            i += 1;
            while i < lines.len() {
                match md_list_item(lines[i].trim()) {
                    Some((o2, it2)) if o2 == ordered => {
                        items.push(Value::String(it2));
                        i += 1;
                    }
                    _ => break,
                }
            }
            blocks.push(json!({ "kind": "list", "ordered": ordered, "items": items }));
        } else {
            para.push(t.to_string());
            i += 1;
        }
    }
    md_flush_para(&mut para, &mut blocks);
    blocks
}

/// Convert a Markdown file into a document. opts: input (.md), output (target;
/// format from extension — docx/odt/pdf/html/…), format => override. Headings,
/// lists, and pipe tables are preserved. Returns `{ ok, path, blocks }`.
fn op_md_to_doc(opts: Value) -> Result<Value> {
    let input = req_str(&opts, "input")?;
    let output = req_str(&opts, "output")?.to_string();
    let text = std::fs::read_to_string(input)?;
    let blocks = parse_markdown_blocks(&text);
    let n = blocks.len();
    let mut wopts = json!({ "path": output, "blocks": blocks });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_doc_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "blocks": n }))
}

/// Convert a docx/odt (or any readable document) to structured Markdown,
/// preserving headings and tables. opts: path, output (.md). The inverse of
/// `md_to_doc`. Returns `{ ok, path, blocks }`.
fn op_doc_to_md(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let blocks = doc_blocks_or_paras(path)?;
    let n = blocks.len();
    op_doc_write(json!({ "path": output, "blocks": blocks, "format": "md" }))?;
    Ok(json!({ "ok": true, "path": output, "blocks": n }))
}

/// Convert a docx/odt (or any readable document) to structured HTML, preserving
/// headings and tables. opts: path, output (.html). The inverse of
/// `html_to_doc`. Returns `{ ok, path, blocks }`.
fn op_doc_to_html(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = req_str(&opts, "output")?.to_string();
    let blocks = doc_blocks_or_paras(path)?;
    let n = blocks.len();
    op_doc_write(json!({ "path": output, "blocks": blocks, "format": "html" }))?;
    Ok(json!({ "ok": true, "path": output, "blocks": n }))
}

// ── merge / convert ───────────────────────────────────────────────────────────

/// Read any supported document into `doc_write`-compatible blocks: docx/odt via
/// the structural reader (headings/paras/tables), pdf via its text lines, and
/// the flow formats (html/md/rtf/txt) via their paragraph reader.
fn doc_blocks_or_paras(path: &str) -> Result<Vec<Value>> {
    let paras_to_blocks = |v: Value| -> Vec<Value> {
        v.get("paragraphs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str())
                    .map(|t| json!({ "kind": "para", "text": t }))
                    .collect()
            })
            .unwrap_or_default()
    };
    match ext_of(path).as_str() {
        "docx" | "odt" => Ok(op_doc_blocks(json!({ "path": path }))?
            .get("blocks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()),
        "pdf" => {
            let bytes = std::fs::read(path)?;
            let text =
                lo_core::extract_text_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
            Ok(text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| json!({ "kind": "para", "text": l }))
                .collect())
        }
        _ => Ok(paras_to_blocks(op_doc_read(json!({ "path": path }))?)),
    }
}

/// Concatenate several documents into one. opts: inputs => [paths],
/// output => path, page_breaks => bool (insert a page break between sources;
/// default true), format => override target format. The target format comes
/// from `output`'s extension, so merge doubles as conversion (read docx, write
/// md/html/pdf/…). Returns `{ ok, path, sources, blocks }`.
fn op_doc_merge(opts: Value) -> Result<Value> {
    let inputs = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?;
    let output = req_str(&opts, "output")?.to_string();
    let page_breaks = opts
        .get("page_breaks")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut blocks: Vec<Value> = Vec::new();
    for (i, inp) in inputs.iter().enumerate() {
        let path = inp
            .as_str()
            .ok_or_else(|| anyhow!("input path must be a string"))?;
        if i > 0 && page_breaks {
            blocks.push(json!({ "kind": "pagebreak" }));
        }
        blocks.append(&mut doc_blocks_or_paras(path)?);
    }

    let n = blocks.len();
    let mut wopts = json!({ "path": output, "blocks": blocks });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_doc_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "sources": inputs.len(), "blocks": n }))
}

/// Append blocks to an existing document. opts: path, blocks => [doc_write
/// blocks], output (default: in place), page_break => bool (insert a break
/// before the appended content), format. The existing content is read into the
/// block model, so the target format follows the output extension. Returns
/// `{ ok, path, blocks, added }`.
fn op_doc_append(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let add = opts
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing blocks (expected array)"))?;

    let mut blocks = doc_blocks_or_paras(path)?;
    if opts.get("page_break").and_then(Value::as_bool).unwrap_or(false) {
        blocks.push(json!({ "kind": "pagebreak" }));
    }
    let added = add.len();
    blocks.extend(add.iter().cloned());
    let total = blocks.len();

    let mut wopts = json!({ "path": output, "blocks": blocks });
    if let Some(f) = opts.get("format") {
        wopts["format"] = f.clone();
    }
    op_doc_write(wopts)?;
    Ok(json!({ "ok": true, "path": output, "blocks": total, "added": added }))
}

/// Split a document into multiple files at headings of a given level. opts:
/// path, dir => output directory, level => heading level to split at (default
/// 1), format => output extension (default: the source's), prefix => filename
/// stem (default: the source's). Each section starts at a split heading; any
/// content before the first heading becomes its own file. Files are
/// `{dir}/{prefix}-{n}.{ext}`. Returns `{ count, files }`.
fn op_doc_split(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let level = opts.get("level").and_then(Value::as_u64).unwrap_or(1);
    let ext = opts
        .get("format")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| ext_of(path));
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("section")
                .to_string()
        });

    let blocks = doc_blocks_or_paras(path)?;
    let mut sections: Vec<Vec<Value>> = Vec::new();
    let mut cur: Vec<Value> = Vec::new();
    for b in blocks {
        let is_split = b.get("kind").and_then(Value::as_str) == Some("heading")
            && b.get("level").and_then(Value::as_u64) == Some(level);
        if is_split && !cur.is_empty() {
            sections.push(std::mem::take(&mut cur));
        }
        cur.push(b);
    }
    if !cur.is_empty() {
        sections.push(cur);
    }

    let mut files = Vec::new();
    for (i, sec) in sections.into_iter().enumerate() {
        let out = format!("{dir}/{prefix}-{}.{ext}", i + 1);
        op_doc_write(json!({ "path": out, "blocks": sec, "format": ext }))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

// ── full-text search (documents + presentations) ──────────────────────────────

/// Search a document's paragraphs (docx/odt/html/md/rtf/txt) or pdf lines.
/// opts: path, query (required), ignore_case (default false). Returns
/// `{ count, matches: [{ paragraph, count, snippet }] }` with 1-based indexes.
fn op_doc_find(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let query = req_str(&opts, "query")?;
    if query.is_empty() {
        return Err(anyhow!("empty query"));
    }
    let ignore_case = opts
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let paras: Vec<String> = match ext_of(path).as_str() {
        "pdf" => {
            let bytes = std::fs::read(path)?;
            lo_core::extract_text_from_pdf(&bytes)
                .map_err(|e| anyhow!("pdf parse: {e}"))?
                .lines()
                .map(str::to_string)
                .collect()
        }
        _ => op_doc_read(json!({ "path": path }))?
            .get("paragraphs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };

    let needle = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };
    let mut total = 0usize;
    let mut matches = Vec::new();
    for (i, p) in paras.iter().enumerate() {
        let hay = if ignore_case {
            p.to_lowercase()
        } else {
            p.clone()
        };
        let c = hay.matches(&needle).count();
        if c > 0 {
            total += c;
            let idx = hay.find(&needle).unwrap_or(0);
            matches.push(json!({
                "paragraph": i + 1,
                "count": c,
                "snippet": pdf_snippet(&hay, idx, needle.len()),
            }));
        }
    }
    Ok(json!({ "count": total, "matches": matches }))
}

/// Search a presentation's slide text and speaker notes (pptx/odp). opts: path,
/// query (required), ignore_case (default false). Returns `{ count, matches:
/// [{ slide, where, value }] }` — `where` is "text" or "notes", slide 1-based.
fn op_slides_find(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let query = req_str(&opts, "query")?;
    if query.is_empty() {
        return Err(anyhow!("empty query"));
    }
    let ignore_case = opts
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let needle = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };

    let read = op_slides_read(json!({ "path": path }))?;
    let slides = read
        .get("slides")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut matches = Vec::new();
    for (i, slide) in slides.iter().enumerate() {
        for field in ["text", "notes"] {
            let Some(lines) = slide.get(field).and_then(Value::as_array) else {
                continue;
            };
            for line in lines.iter().filter_map(Value::as_str) {
                let hay = if ignore_case {
                    line.to_lowercase()
                } else {
                    line.to_string()
                };
                if hay.contains(&needle) {
                    matches.push(json!({ "slide": i + 1, "where": field, "value": line }));
                }
            }
        }
    }
    Ok(json!({ "count": matches.len(), "matches": matches }))
}

// ── statistics (Word-style word count, across every readable format) ──────────

/// Word-count style statistics for any document we can read as text — docx,
/// odt, html, md, rtf, txt, and pdf. Returns `{ words, characters,
/// characters_no_spaces, lines, paragraphs, pages? }` (pages only for pdf).
/// Mirrors Word's "Word Count" dialog.
fn op_doc_stats(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let mut pages: Option<usize> = None;
    let paras: Vec<String> = match ext_of(path).as_str() {
        "pdf" => {
            let bytes = std::fs::read(path)?;
            pages = lo_core::extract_pages_from_pdf(&bytes).ok().map(|p| p.len());
            let text =
                lo_core::extract_text_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
            text.lines().map(str::to_string).collect()
        }
        _ => op_doc_read(json!({ "path": path }))?
            .get("paragraphs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };

    let paragraphs = paras.iter().filter(|p| !p.trim().is_empty()).count();
    let text = paras.join("\n");
    let words = text.split_whitespace().count();
    let characters = text.chars().count();
    let characters_no_spaces = text.chars().filter(|c| !c.is_whitespace()).count();
    let lines = if text.is_empty() { 0 } else { text.lines().count() };

    let mut out = json!({
        "words": words,
        "characters": characters,
        "characters_no_spaces": characters_no_spaces,
        "lines": lines,
        "paragraphs": paragraphs,
    });
    if let Some(p) = pages {
        out["pages"] = json!(p);
    }
    Ok(out)
}

/// The full text of any readable document as one string.
fn doc_full_text(path: &str) -> Result<String> {
    Ok(match ext_of(path).as_str() {
        "pdf" => {
            let bytes = std::fs::read(path)?;
            lo_core::extract_text_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?
        }
        _ => op_doc_read(json!({ "path": path }))?
            .get("paragraphs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    })
}

/// Common English stopwords filtered when `stopwords` is requested.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "of", "to", "in", "on", "at", "for", "with", "is", "are",
    "was", "were", "be", "been", "by", "as", "that", "this", "it", "its", "from", "not", "no",
    "into", "than", "then",
];

/// Word-frequency analysis for any readable document. opts: path, top (default
/// 20), min_length (default 1), ignore_case (default true), stopwords (default
/// false; filter common English words). Returns `{ total, unique, words:
/// [{ word, count }] }` sorted by count desc then word asc.
fn op_doc_wordfreq(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let top = opts.get("top").and_then(Value::as_u64).unwrap_or(20) as usize;
    let min_len = opts.get("min_length").and_then(Value::as_u64).unwrap_or(1) as usize;
    let ignore_case = opts.get("ignore_case").and_then(Value::as_bool).unwrap_or(true);
    let use_stop = opts.get("stopwords").and_then(Value::as_bool).unwrap_or(false);

    let text = doc_full_text(path)?;
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut total = 0u64;
    for tok in text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
        let w = if ignore_case {
            tok.to_lowercase()
        } else {
            tok.to_string()
        };
        if w.chars().count() < min_len {
            continue;
        }
        if use_stop && STOPWORDS.contains(&w.to_lowercase().as_str()) {
            continue;
        }
        *counts.entry(w).or_insert(0) += 1;
        total += 1;
    }

    let unique = counts.len();
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(top);
    let words: Vec<Value> = ranked
        .into_iter()
        .map(|(w, c)| json!({ "word": w, "count": c }))
        .collect();
    Ok(json!({ "total": total, "unique": unique, "words": words }))
}

// ── hyperlinks ────────────────────────────────────────────────────────────────

/// Extract hyperlinks from a docx. `<w:hyperlink r:id="…">` carries the display
/// text inline; the URL lives in `word/_rels/document.xml.rels` keyed by that
/// id. Internal `w:anchor` links return `#anchor` as the URL.
fn extract_links_docx(doc_xml: &[u8], rels_xml: &[u8]) -> Vec<Value> {
    use quick_xml::events::Event;
    let map = rels_id_target_map(rels_xml);
    let mut reader = quick_xml::Reader::from_reader(doc_xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut url = String::new();
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:hyperlink" => {
                depth += 1;
                if depth == 1 {
                    text.clear();
                    url = match attr(&e, b"r:id") {
                        Some(rid) => map.get(&rid).cloned().unwrap_or_default(),
                        None => attr(&e, b"w:anchor").map(|a| format!("#{a}")).unwrap_or_default(),
                    };
                }
            }
            Ok(Event::Text(e)) => {
                if depth > 0 {
                    if let Ok(t) = e.xml10_content() {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"w:hyperlink" => {
                depth -= 1;
                if depth == 0 {
                    out.push(json!({
                        "text": std::mem::take(&mut text),
                        "url": std::mem::take(&mut url),
                    }));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Extract hyperlinks from an odt: `<text:a xlink:href="…">display</text:a>`.
fn extract_links_odt(xml: &[u8]) -> Vec<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut url = String::new();
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"text:a" => {
                depth += 1;
                if depth == 1 {
                    text.clear();
                    url = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"xlink:href")
                        .and_then(|a| {
                            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()
                                .map(|c| c.into_owned())
                        })
                        .unwrap_or_default();
                }
            }
            Ok(Event::Text(e)) => {
                if depth > 0 {
                    if let Ok(t) = e.xml10_content() {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"text:a" => {
                depth -= 1;
                if depth == 0 {
                    out.push(json!({
                        "text": std::mem::take(&mut text),
                        "url": std::mem::take(&mut url),
                    }));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Extract every hyperlink from a docx/odt as `{ links: [{text, url}], count }`.
fn op_doc_links(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let links = match ext_of(path).as_str() {
        "docx" => {
            let doc = read_zip_entry(&bytes, "word/document.xml")?;
            let rels = read_zip_entry(&bytes, "word/_rels/document.xml.rels").unwrap_or_default();
            extract_links_docx(&doc, &rels)
        }
        "odt" => extract_links_odt(&read_zip_entry(&bytes, "content.xml")?),
        other => return Err(anyhow!("unsupported document link format: {other}")),
    };
    Ok(json!({ "links": links, "count": links.len() }))
}

/// Ordered structural read of a docx/odt: `{ blocks: [{kind:"heading",level,
/// text} | {kind:"para",text} | {kind:"table",rows}], count }`, in document
/// order. The read-side mirror of `doc_write`'s block model.
fn op_doc_blocks(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let blocks = match ext_of(path).as_str() {
        "docx" => extract_blocks_docx(&read_zip_entry(&bytes, "word/document.xml")?),
        "odt" => extract_blocks_odt(&read_zip_entry(&bytes, "content.xml")?),
        other => return Err(anyhow!("unsupported document read format: {other}")),
    };
    Ok(json!({ "blocks": blocks, "count": blocks.len() }))
}
