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
