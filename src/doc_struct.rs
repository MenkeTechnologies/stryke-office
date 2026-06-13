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
