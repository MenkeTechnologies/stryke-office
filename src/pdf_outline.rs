// PDF document outline (bookmarks) — read + write. The outline is the
// navigation tree shown in a viewer's sidebar: a catalog `/Outlines` dict whose
// items carry a `/Title` and a `/Dest` (or `/A` GoTo action) pointing at a page.
// Writing uses lopdf's own bookmark builder; reading walks the linked-list tree.
// Pure format-internal object-graph work.
//
// `Document`, `Object`, `ObjectId` are in scope from the pdf_ops include.

/// Decode a PDF text string: UTF-16BE (BOM `FE FF`) or PDFDocEncoding (≈latin1).
fn pdf_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

/// One indirect-reference hop.
fn deref<'a>(doc: &'a Document, o: &'a Object) -> &'a Object {
    match o {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(o),
        other => other,
    }
}

/// 1-based page number an outline item points at, via its /Dest or /A GoTo /D.
fn item_page(doc: &Document, item: &lopdf::Dictionary, page_num: &std::collections::HashMap<ObjectId, u32>) -> Option<u32> {
    let dest_obj = item
        .get(b"Dest")
        .ok()
        .or_else(|| {
            let a = item.get(b"A").ok()?;
            let ad = deref(doc, a).as_dict().ok()?;
            ad.get(b"D").ok()
        })?;
    let resolved = deref(doc, dest_obj);
    let arr = resolved.as_array().ok()?;
    let page_ref = arr.first()?.as_reference().ok()?;
    page_num.get(&page_ref).copied()
}

/// Walk an outline item's `/Next` chain, recursing into `/First`.
fn read_items(
    doc: &Document,
    first: ObjectId,
    page_num: &std::collections::HashMap<ObjectId, u32>,
    depth: u32,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cur = Some(first);
    let mut guard = 0;
    while let Some(id) = cur {
        guard += 1;
        if guard > 4096 {
            break; // malformed cyclic chain
        }
        let Ok(item) = doc.get_dictionary(id) else {
            break;
        };
        let title = item
            .get(b"Title")
            .and_then(|o| o.as_str())
            .map(pdf_text)
            .unwrap_or_default();
        let mut node = json!({"title": title});
        if let Some(p) = item_page(doc, item, page_num) {
            node["page"] = json!(p);
        }
        if depth < 32 {
            if let Ok(child) = item.get(b"First").and_then(|o| o.as_reference()) {
                let kids = read_items(doc, child, page_num, depth + 1);
                if !kids.is_empty() {
                    node["children"] = json!(kids);
                }
            }
        }
        out.push(node);
        cur = item.get(b"Next").and_then(|o| o.as_reference()).ok();
    }
    out
}

fn op_pdf_outline(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    // page object id -> 1-based number
    let page_num: std::collections::HashMap<ObjectId, u32> =
        doc.get_pages().into_iter().map(|(n, id)| (id, n)).collect();

    let outline = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .map(|o| deref(&doc, o))
        .and_then(|o| o.as_dict().ok());
    let items = match outline.and_then(|d| d.get(b"First").and_then(|o| o.as_reference()).ok()) {
        Some(first) => read_items(&doc, first, &page_num, 0),
        None => Vec::new(),
    };
    let count = count_items(&items);
    Ok(json!({"outline": items, "count": count}))
}

fn count_items(items: &[Value]) -> usize {
    items
        .iter()
        .map(|it| {
            1 + it
                .get("children")
                .and_then(Value::as_array)
                .map(|c| count_items(c))
                .unwrap_or(0)
        })
        .sum()
}

/// Recursively register bookmarks under `parent`, mapping 1-based page numbers
/// to page object ids. Returns how many bookmarks were added.
fn add_outline_nodes(
    doc: &mut Document,
    nodes: &[Value],
    parent: Option<u32>,
    pages: &std::collections::BTreeMap<u32, ObjectId>,
    npages: u32,
) -> usize {
    let mut n = 0;
    for node in nodes {
        let title = node.get("title").and_then(Value::as_str).unwrap_or("");
        if title.is_empty() {
            continue;
        }
        let page = node
            .get("page")
            .and_then(Value::as_u64)
            .map(|p| (p as u32).clamp(1, npages.max(1)))
            .unwrap_or(1);
        let page_id = pages.get(&page).copied().unwrap_or((0, 0));
        let bold = node.get("bold").and_then(flag_of).unwrap_or(false);
        let italic = node.get("italic").and_then(flag_of).unwrap_or(false);
        let format = (bold as u32) << 1 | (italic as u32);
        let color = node
            .get("color")
            .and_then(Value::as_array)
            .map(|a| {
                let g = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                [g(0), g(1), g(2)]
            })
            .unwrap_or([0.0, 0.0, 0.0]);
        let bm = lopdf::Bookmark::new(title.to_string(), color, format, page_id);
        let id = doc.add_bookmark(bm, parent);
        n += 1;
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            n += add_outline_nodes(doc, children, Some(id), pages, npages);
        }
    }
    n
}

fn op_pdf_set_outline(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let output = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let nodes = opts
        .get("outline")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing outline (expected array of {{title, page, children?}})"))?
        .clone();

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let npages = pages.len() as u32;
    let count = add_outline_nodes(&mut doc, &nodes, None, &pages, npages);

    if let Some(outline_id) = doc.build_outline() {
        let root = doc
            .trailer
            .get(b"Root")
            .and_then(|o| o.as_reference())
            .map_err(|_| anyhow!("no catalog Root"))?;
        if let Ok(cat) = doc.get_dictionary_mut(root) {
            cat.set("Outlines", Object::Reference(outline_id));
        }
    }
    doc.save(&output).map_err(|e| anyhow!("save {output}: {e}"))?;
    Ok(json!({"ok": true, "path": output, "count": count}))
}
