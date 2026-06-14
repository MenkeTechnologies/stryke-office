// PDF manipulation (merge / split / rotate / info) via `lopdf`.
//
// These operate on existing PDFs — parsing the object graph, which is why
// they use `lopdf` rather than the from-scratch emitters in `pdf_build` /
// `chart_svg`. The merge routine is a faithful port of lopdf's own
// `examples/merge.rs` (minus the bookmark/outline layering); rotate ports
// `examples/rotate.rs`.

use lopdf::{Document, Object, ObjectId};
use std::collections::BTreeMap;

/// Generate a blank PDF with N empty pages of a given size — a building block
/// for templates, spacers, or a base to stamp onto. opts: output (required),
/// pages => page count (default 1), size => named paper size a4|letter|legal|a3|a5
/// (default a4), or explicit width / height in points (override the named size).
/// Returns `{ ok, path, pages, width, height }`.
fn op_pdf_blank(opts: Value) -> Result<Value> {
    use lopdf::Dictionary;
    let out = req_str(&opts, "output")?.to_string();
    let pages_n = opts
        .get("pages")
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .unwrap_or(1);
    // Named paper sizes in points (1/72").
    let (mut w, mut h) = match opts.get("size").and_then(Value::as_str) {
        Some("letter") => (612.0, 792.0),
        Some("legal") => (612.0, 1008.0),
        Some("a3") => (842.0, 1191.0),
        Some("a5") => (420.0, 595.0),
        _ => (595.0, 842.0), // a4
    };
    if let Some(x) = opts.get("width").and_then(Value::as_f64) {
        w = x;
    }
    if let Some(y) = opts.get("height").and_then(Value::as_f64) {
        h = y;
    }

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let media_box = || {
        Object::Array(vec![
            0.into(),
            0.into(),
            Object::Real(w as f32),
            Object::Real(h as f32),
        ])
    };
    let mut kids: Vec<Object> = Vec::with_capacity(pages_n as usize);
    for _ in 0..pages_n {
        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set("Parent", pages_id);
        page.set("MediaBox", media_box());
        kids.push(Object::Reference(doc.add_object(Object::Dictionary(page))));
    }
    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Count", Object::Integer(pages_n as i64));
    pages.set("Kids", Object::Array(kids));
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", pages_id);
    let catalog_id = doc.add_object(Object::Dictionary(catalog));
    doc.trailer.set("Root", catalog_id);
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "pages": pages_n, "width": w, "height": h }))
}

/// Concatenate several PDFs into one. opts: inputs => [paths], path => output.
fn op_pdf_merge(opts: Value) -> Result<Value> {
    let inputs: Vec<String> = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if inputs.is_empty() {
        return Err(anyhow!("no input PDFs to merge"));
    }
    let out = req_str(&opts, "path")?.to_string();

    let mut max_id = 1;
    let mut documents_pages: BTreeMap<ObjectId, Object> = BTreeMap::new();
    let mut documents_objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
    let mut document = Document::with_version("1.5");

    for p in &inputs {
        let mut doc = Document::load(p).map_err(|e| anyhow!("load {p}: {e}"))?;
        doc.renumber_objects_with(max_id);
        max_id = doc.max_id + 1;
        // collect each page object (cloned) before moving the object map
        for object_id in doc.get_pages().into_values() {
            if let Ok(obj) = doc.get_object(object_id) {
                documents_pages.insert(object_id, obj.to_owned());
            }
        }
        documents_objects.extend(doc.objects);
    }

    // Catalog + Pages are mandatory; collect them and pass everything else
    // straight through.
    let mut catalog_object: Option<(ObjectId, Object)> = None;
    let mut pages_object: Option<(ObjectId, Object)> = None;
    for (object_id, object) in documents_objects.into_iter() {
        match object.type_name().unwrap_or(b"") {
            b"Catalog" => {
                catalog_object = Some((catalog_object.map(|(id, _)| id).unwrap_or(object_id), object));
            }
            b"Pages" => {
                if let Ok(dictionary) = object.as_dict() {
                    let mut dictionary = dictionary.clone();
                    if let Some((_, ref old)) = pages_object {
                        if let Ok(old_dict) = old.as_dict() {
                            dictionary.extend(old_dict);
                        }
                    }
                    pages_object = Some((
                        pages_object.map(|(id, _)| id).unwrap_or(object_id),
                        Object::Dictionary(dictionary),
                    ));
                }
            }
            b"Page" | b"Outlines" | b"Outline" => {} // pages handled below; outlines unsupported
            _ => {
                document.objects.insert(object_id, object);
            }
        }
    }

    let Some((pages_id, pages_obj)) = pages_object else {
        return Err(anyhow!("no Pages root found in inputs"));
    };
    let Some((catalog_id, catalog_obj)) = catalog_object else {
        return Err(anyhow!("no Catalog root found in inputs"));
    };

    // Re-parent every page to the merged Pages node.
    for (object_id, object) in documents_pages.iter() {
        if let Ok(dictionary) = object.as_dict() {
            let mut dictionary = dictionary.clone();
            dictionary.set("Parent", pages_id);
            document.objects.insert(*object_id, Object::Dictionary(dictionary));
        }
    }

    // Rebuild Pages with the full Kids list + Count.
    if let Ok(dict) = pages_obj.as_dict() {
        let mut dict = dict.clone();
        dict.set("Count", documents_pages.len() as u32);
        dict.set(
            "Kids",
            documents_pages.keys().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
        );
        document.objects.insert(pages_id, Object::Dictionary(dict));
    }
    // Rebuild Catalog → Pages.
    if let Ok(dict) = catalog_obj.as_dict() {
        let mut dict = dict.clone();
        dict.set("Pages", pages_id);
        dict.remove(b"Outlines");
        document.objects.insert(catalog_id, Object::Dictionary(dict));
    }
    document.trailer.set("Root", catalog_id);
    document.max_id = document.objects.len() as u32;
    document.renumber_objects();
    document.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": documents_pages.len(), "merged": inputs.len()}))
}

/// Extract a subset of pages into a new PDF. opts: path => input, pages =>
/// [1-based page numbers to keep], output => path. Kept pages stay in their
/// original order.
fn op_pdf_split(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let keep: std::collections::BTreeSet<u32> = opts
        .get("pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing pages (expected array of 1-based page numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();
    if keep.is_empty() {
        return Err(anyhow!("no pages selected"));
    }
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let total = doc.get_pages().len() as u32;
    let remove: Vec<u32> = (1..=total).filter(|p| !keep.contains(p)).collect();
    doc.delete_pages(&remove);
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": (total as usize).saturating_sub(remove.len())}))
}

/// Rotate pages by `angle` degrees (multiple of 90). opts: path => input,
/// angle, output => path, pages => [1-based subset] (default: all).
fn op_pdf_rotate(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let angle = opts.get("angle").and_then(Value::as_i64).unwrap_or(90);
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut rotated = 0usize;
    for (num, page_id) in doc.get_pages() {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        if let Some(dict) = doc.get_object_mut(page_id).ok().and_then(|o| o.as_dict_mut().ok()) {
            let current = dict.get(b"Rotate").and_then(|o| o.as_i64()).unwrap_or(0);
            dict.set("Rotate", (current + angle).rem_euclid(360));
            rotated += 1;
        }
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "rotated": rotated, "angle": angle}))
}

/// A PDF text-string dictionary value as UTF-8 (lossy).
fn pdf_dict_str(dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
    match dict.get(key).ok()? {
        Object::String(bytes, _) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// A PDF number (Integer or Real) as f64.
fn obj_num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// MediaBox (width, height) of a page, defaulting to A4.
fn pdf_page_size(doc: &Document, page_id: ObjectId) -> (f64, f64) {
    doc.get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"MediaBox").ok())
        .and_then(|o| o.as_array().ok())
        .and_then(|a| Some((obj_num(a.get(2)?)?, obj_num(a.get(3)?)?)))
        .unwrap_or((595.0, 842.0))
}

/// A 4-element rectangle (e.g. MediaBox/CropBox) of a page, if present.
fn pdf_page_box(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<[f64; 4]> {
    let a = doc
        .get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(key).ok())
        .and_then(|o| o.as_array().ok())?;
    Some([
        obj_num(a.first()?)?,
        obj_num(a.get(1)?)?,
        obj_num(a.get(2)?)?,
        obj_num(a.get(3)?)?,
    ])
}

/// Parse a JSON 4-number array into `[f64; 4]`.
fn four_floats(v: &Value) -> Option<[f64; 4]> {
    let a = v.as_array()?;
    Some([
        a.first()?.as_f64()?,
        a.get(1)?.as_f64()?,
        a.get(2)?.as_f64()?,
        a.get(3)?.as_f64()?,
    ])
}

/// Add an inline Helvetica font named `name` to a page's Resources.
fn ensure_helvetica(doc: &mut Document, page_id: ObjectId, name: &str) -> Result<()> {
    use lopdf::Dictionary;
    let res = doc.get_or_create_resources(page_id).map_err(|e| anyhow!("resources: {e}"))?;
    let rdict = res.as_dict_mut().map_err(|e| anyhow!("resources dict: {e}"))?;
    if !rdict.has(b"Font") {
        rdict.set("Font", Dictionary::new());
    }
    let fonts = rdict
        .get_mut(b"Font")
        .ok()
        .and_then(|o| o.as_dict_mut().ok())
        .ok_or_else(|| anyhow!("font dict"))?;
    let mut fd = Dictionary::new();
    fd.set("Type", "Font");
    fd.set("Subtype", "Type1");
    fd.set("BaseFont", "Helvetica");
    fonts.set(name, Object::Dictionary(fd));
    Ok(())
}

/// RGB 0..1 of a color value (default light gray for watermarks).
fn pdf_color01(v: Option<&Value>, default: [u8; 3]) -> (f64, f64, f64) {
    let c = match v {
        Some(_) => parse_color(v),
        None => image::Rgba([default[0], default[1], default[2], 255]),
    };
    (c.0[0] as f64 / 255.0, c.0[1] as f64 / 255.0, c.0[2] as f64 / 255.0)
}

/// Stamp a rotated text watermark across every page. opts: path => input,
/// output, text, size (60), color (default light gray), angle (deg, 45).
fn op_pdf_watermark(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let text = req_str(&opts, "text")?.to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(60.0);
    let (r, g, b) = pdf_color01(opts.get("color"), [200, 200, 200]);
    let angle = opts.get("angle").and_then(Value::as_f64).unwrap_or(45.0);
    let (sin, cos) = angle.to_radians().sin_cos();
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut stamped = 0usize;
    for (_, page_id) in doc.get_pages() {
        ensure_helvetica(&mut doc, page_id, "SOFW")?;
        let (pw, ph) = pdf_page_size(&doc, page_id);
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("Tf", vec!["SOFW".into(), size.into()]),
            Operation::new("Tm", vec![cos.into(), sin.into(), (-sin).into(), cos.into(), (pw * 0.12).into(), (ph * 0.35).into()]),
            Operation::new("Tj", vec![Object::string_literal(text.clone())]),
            Operation::new("ET", vec![]),
            Operation::new("Q", vec![]),
        ];
        doc.add_to_page_content(page_id, Content { operations: ops }).map_err(|e| anyhow!("stamp page: {e}"))?;
        stamped += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "stamped": stamped}))
}

/// Add footer page numbers to every page. opts: path => input, output,
/// format ("{n} / {total}"), size (10), color (black), y (24).
fn op_pdf_page_numbers(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let fmt = opts.get("format").and_then(Value::as_str).unwrap_or("{n} / {total}").to_string();
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(10.0);
    let (r, g, b) = pdf_color01(opts.get("color"), [40, 40, 40]);
    let y = opts.get("y").and_then(Value::as_f64).unwrap_or(24.0);
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let total = pages.len();
    for (num, page_id) in pages {
        ensure_helvetica(&mut doc, page_id, "SOFP")?;
        let (pw, _) = pdf_page_size(&doc, page_id);
        let text = fmt.replace("{n}", &num.to_string()).replace("{total}", &total.to_string());
        let x = (pw / 2.0 - text.len() as f64 * size * 0.25).max(4.0);
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("Tf", vec!["SOFP".into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ];
        doc.add_to_page_content(page_id, Content { operations: ops }).map_err(|e| anyhow!("number page: {e}"))?;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": total}))
}

/// Page count, version, and document-info metadata. opts: path => input.
fn op_pdf_info(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages().len();
    let mut out = json!({"pages": pages, "version": doc.version});
    // Document info dictionary (Title/Author/...), if present.
    if let Some(info) = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_dict().ok())
    {
        for (json_key, pdf_key) in [
            ("title", b"Title".as_slice()),
            ("author", b"Author"),
            ("subject", b"Subject"),
            ("creator", b"Creator"),
            ("producer", b"Producer"),
            ("keywords", b"Keywords"),
        ] {
            if let Some(v) = pdf_dict_str(info, pdf_key) {
                out[json_key] = Value::String(v);
            }
        }
    }
    // First page geometry: MediaBox (+ derived width/height) and CropBox if set.
    if let Some((_, pid)) = doc.get_pages().into_iter().next() {
        if let Some(mb) = pdf_page_box(&doc, pid, b"MediaBox") {
            out["mediabox"] = json!(mb);
            out["width"] = json!(mb[2] - mb[0]);
            out["height"] = json!(mb[3] - mb[1]);
        }
        if let Some(cb) = pdf_page_box(&doc, pid, b"CropBox") {
            out["cropbox"] = json!(cb);
        }
    }
    Ok(out)
}

/// Per-page dimensions of a PDF in points (1/72 inch). opts: path. Returns
/// `{ pages: [{ page, width, height }], count }` with 1-based page numbers.
/// Useful for layout analysis when pages differ in size.
fn op_pdf_page_sizes(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages: Vec<Value> = doc
        .get_pages()
        .into_iter()
        .map(|(num, pid)| {
            let (w, h) = pdf_page_size(&doc, pid);
            json!({ "page": num, "width": w, "height": h })
        })
        .collect();
    let count = pages.len();
    Ok(json!({ "pages": pages, "count": count }))
}

/// Set the crop box (visible region) on pages. opts: path, output, either
/// `box` => [x0, y0, x1, y1] applied to all selected pages, or
/// `margins` => [left, bottom, right, top] inset from each page's MediaBox.
/// `pages` => [1-based subset] (default all). Returns `{ ok, path, cropped }`.
fn op_pdf_crop(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let explicit = opts.get("box").and_then(four_floats);
    let margins = opts.get("margins").and_then(four_floats);
    if explicit.is_none() && margins.is_none() {
        return Err(anyhow!("need box [x0,y0,x1,y1] or margins [l,b,r,t]"));
    }
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let mut plan: Vec<(ObjectId, [f64; 4])> = Vec::new();
    for (num, pid) in &pages {
        if let Some(set) = &subset {
            if !set.contains(num) {
                continue;
            }
        }
        let bx = if let Some(b) = explicit {
            b
        } else {
            let mb = pdf_page_box(&doc, *pid, b"MediaBox").unwrap_or([0.0, 0.0, 595.0, 842.0]);
            let m = margins.unwrap();
            [mb[0] + m[0], mb[1] + m[1], mb[2] - m[2], mb[3] - m[3]]
        };
        plan.push((*pid, bx));
    }
    let cropped = plan.len();
    for (pid, bx) in plan {
        if let Ok(d) = doc.get_object_mut(pid).and_then(|o| o.as_dict_mut()) {
            d.set(
                "CropBox",
                Object::Array(bx.iter().map(|&v| Object::Real(v as f32)).collect()),
            );
        }
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "cropped": cropped }))
}

// ── security: encrypt / decrypt ───────────────────────────────────────────────

/// Translate permission-name strings into lopdf `Permissions` bits. A missing
/// `permissions` key grants everything; an unknown name is ignored. Names:
/// print, modify, copy, annotate, fill, accessibility, assemble, print_hq.
fn pdf_permissions(opts: &Value) -> lopdf::Permissions {
    use lopdf::Permissions;
    let Some(names) = opts.get("permissions").and_then(Value::as_array) else {
        return Permissions::all();
    };
    let mut perms = Permissions::empty();
    for name in names.iter().filter_map(Value::as_str) {
        perms |= match name {
            "print" => Permissions::PRINTABLE,
            "modify" => Permissions::MODIFIABLE,
            "copy" => Permissions::COPYABLE,
            "annotate" => Permissions::ANNOTABLE,
            "fill" => Permissions::FILLABLE,
            "accessibility" => Permissions::COPYABLE_FOR_ACCESSIBILITY,
            "assemble" => Permissions::ASSEMBLABLE,
            "print_hq" => Permissions::PRINTABLE_IN_HIGH_QUALITY,
            _ => continue,
        };
    }
    perms
}

/// Ensure the trailer carries a file `/ID`. The standard security handler
/// derives its file-encryption key from the ID, so an ID-less PDF (e.g. one of
/// our from-scratch builder outputs) can't be encrypted until one exists. The
/// bytes are a stable hash of `seed` — the PDF spec only asks the ID be a
/// per-file identifier, not cryptographically random; the password supplies the
/// security.
fn ensure_pdf_id(doc: &mut Document, seed: &[u8]) {
    use lopdf::StringFormat;
    use std::hash::{Hash, Hasher};
    if doc.trailer.get(b"ID").is_ok() {
        return;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut h);
    let lo = h.finish();
    (seed, lo).hash(&mut h);
    let hi = h.finish();
    let mut id = Vec::with_capacity(16);
    id.extend_from_slice(&lo.to_le_bytes());
    id.extend_from_slice(&hi.to_le_bytes());
    let s = Object::String(id, StringFormat::Hexadecimal);
    doc.trailer.set("ID", Object::Array(vec![s.clone(), s]));
}

/// Password-protect an existing PDF (standard security handler). opts:
/// path => input, output => path, owner_password (""), user_password (""),
/// aes => bool (AES-128 / V4; default RC4 / V2), key_length (V2 bits, default
/// 128), permissions => [names] (default: all granted). Ports the version
/// selection from lopdf's `examples/encrypt.rs`.
fn op_pdf_encrypt(opts: Value) -> Result<Value> {
    use lopdf::encryption::crypt_filters::{Aes128CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion};
    use std::sync::Arc;

    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let owner = opts
        .get("owner_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let user = opts
        .get("user_password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let aes = opts.get("aes").and_then(flag_of).unwrap_or(false);
    let key_length = opts
        .get("key_length")
        .and_then(Value::as_u64)
        .unwrap_or(128) as usize;
    let permissions = pdf_permissions(&opts);

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    if doc.is_encrypted() {
        return Err(anyhow!("already encrypted"));
    }
    ensure_pdf_id(&mut doc, path.as_bytes());
    let state = if aes {
        let cf: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
        EncryptionState::try_from(EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), cf)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions,
        })
        .map_err(|e| anyhow!("build encryption state: {e}"))?
    } else {
        EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: owner,
            user_password: user,
            key_length,
            permissions,
        })
        .map_err(|e| anyhow!("build encryption state: {e}"))?
    };
    doc.encrypt(&state).map_err(|e| anyhow!("encrypt: {e}"))?;
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    let method = if aes {
        "aes-128".to_string()
    } else {
        format!("rc4-{key_length}")
    };
    Ok(json!({"ok": true, "path": out, "method": method}))
}

/// Strip password protection from a PDF. opts: path => input, output => path,
/// password (owner or user; default ""). `load_with_password` authenticates,
/// decrypts every string/stream, and drops the trailer /Encrypt entry, so the
/// saved file is plaintext.
fn op_pdf_decrypt(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let password = opts.get("password").and_then(Value::as_str).unwrap_or("");
    let mut doc =
        Document::load_with_password(path, password).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out}))
}

/// Shrink a PDF: drop unreferenced objects, then deflate content into object
/// streams. opts: path => input, output => path. Returns byte sizes before /
/// after and the delta.
fn op_pdf_compress(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.prune_objects();
    doc.compress();
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    let after = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(json!({
        "ok": true,
        "path": out,
        "before": before,
        "after": after,
        "saved": before.saturating_sub(after),
    }))
}

// ── page management: delete / reorder ─────────────────────────────────────────

/// Remove pages from a PDF. opts: path => input, output => path,
/// pages => [1-based page numbers to delete]. Returns the remaining count.
fn op_pdf_delete(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let drop: Vec<u32> = opts
        .get("pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing pages (expected array of 1-based page numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();
    if drop.is_empty() {
        return Err(anyhow!("no pages selected"));
    }
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.delete_pages(&drop);
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": doc.get_pages().len()}))
}

/// Parse a page selection — either a JSON array of 1-based numbers or a human
/// range spec string like "1-3,5,8-10". Ascending and descending ranges are both
/// honored ("3-1" → 3,2,1); whitespace is ignored. Order and repeats are
/// preserved (no dedup/sort) so callers can extract pages in any sequence.
fn pdf_parse_page_spec(v: &Value) -> Result<Vec<u32>> {
    match v {
        Value::Array(arr) => Ok(arr.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect()),
        Value::String(s) => {
            let mut out = Vec::new();
            for tok in s.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                if let Some((a, b)) = tok.split_once('-') {
                    let a: u32 = a.trim().parse().map_err(|_| anyhow!("bad range: {tok}"))?;
                    let b: u32 = b.trim().parse().map_err(|_| anyhow!("bad range: {tok}"))?;
                    if a <= b {
                        out.extend(a..=b);
                    } else {
                        out.extend((b..=a).rev());
                    }
                } else {
                    out.push(tok.parse().map_err(|_| anyhow!("bad page: {tok}"))?);
                }
            }
            Ok(out)
        }
        _ => Err(anyhow!("pages must be an array or a range-spec string")),
    }
}

/// Extract a subset of pages into a single new PDF, in the order requested — the
/// "keep only these pages" complement to `pdf_delete` (which removes) and the
/// single-file counterpart to `pdf_split_ranges` (which emits many files). opts:
/// path => input, output => path, pages => array of 1-based page numbers OR a
/// range-spec string like "1-3,5,8-10" (ascending/descending ranges and repeats
/// honored). Page order follows the spec. Returns `{ ok, path, pages }`.
fn op_pdf_extract(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let order = pdf_parse_page_spec(
        opts.get("pages")
            .ok_or_else(|| anyhow!("missing pages (array or range-spec string)"))?,
    )?;
    if order.is_empty() {
        return Err(anyhow!("no pages selected"));
    }
    // Delegate to the page-tree reparent logic in pdf_reorder, which already
    // subsets, reorders, and bakes MediaBoxes for a self-contained output.
    op_pdf_reorder(json!({ "path": path, "output": out, "order": order }))
}

/// Remove pages whose extracted text is empty (clean scanned spacers / blank
/// leaves). opts: path => input, output => path. NOTE: a page is "blank" only by
/// its *text* layer — an image-only page has no text and will be dropped, so use
/// on text PDFs. Never removes every page (a fully-blank document is left
/// intact). Returns `{ ok, path, removed, pages }`.
fn op_pdf_remove_blank(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let bytes = std::fs::read(path)?;
    let pages = lo_core::extract_pages_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
    // 1-based page numbers whose text layer is all-whitespace.
    let blank: Vec<u32> = pages
        .iter()
        .enumerate()
        .filter(|(_, t)| t.trim().is_empty())
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    if blank.is_empty() || blank.len() == pages.len() {
        // Nothing to do, or every page is blank — copy through unchanged.
        std::fs::write(&out, &bytes)?;
        return Ok(json!({ "ok": true, "path": out, "removed": 0, "pages": pages.len() }));
    }
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    doc.delete_pages(&blank);
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "removed": blank.len(), "pages": doc.get_pages().len() }))
}

/// Reorder (and/or subset) a PDF's pages. opts: path => input, output => path,
/// order => [1-based page numbers in the desired output order]. Pages omitted
/// from `order` are dropped; a page may be repeated. The effective MediaBox is
/// baked onto each page and every page is reparented to the root page tree, so
/// the flattened result is self-contained (documents relying on Resources
/// inherited from intermediate page-tree nodes are not rewritten).
fn op_pdf_reorder(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let order: Vec<u32> = opts
        .get("order")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing order (expected array of 1-based page numbers)"))?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect();
    if order.is_empty() {
        return Err(anyhow!("empty order"));
    }
    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let pages_root = doc
        .catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(|o| o.as_reference())
        .map_err(|e| anyhow!("no page tree: {e}"))?;
    // Bake the effective MediaBox onto each page and reparent it to the root, so
    // pointing the root's Kids straight at the leaves loses no inherited size.
    for &pid in pages.values() {
        let (w, h) = pdf_page_size(&doc, pid);
        if let Ok(d) = doc.get_object_mut(pid).and_then(|o| o.as_dict_mut()) {
            if d.get(b"MediaBox").is_err() {
                d.set(
                    "MediaBox",
                    Object::Array(vec![0.into(), 0.into(), w.into(), h.into()]),
                );
            }
            d.set("Parent", Object::Reference(pages_root));
        }
    }
    let kids: Vec<Object> = order
        .iter()
        .filter_map(|n| pages.get(n).map(|id| Object::Reference(*id)))
        .collect();
    if kids.is_empty() {
        return Err(anyhow!("order referenced no existing pages"));
    }
    let count = kids.len() as i64;
    if let Ok(d) = doc.get_object_mut(pages_root).and_then(|o| o.as_dict_mut()) {
        d.set("Kids", Object::Array(kids));
        d.set("Count", count);
    }
    doc.prune_objects();
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({"ok": true, "path": out, "pages": count}))
}

// ── full-text search ──────────────────────────────────────────────────────────

/// A short, char-safe, newline-flattened context window around a match.
fn pdf_snippet(text: &str, idx: usize, match_len: usize) -> String {
    let start = text[..idx]
        .char_indices()
        .rev()
        .take(30)
        .last()
        .map_or(idx, |(i, _)| i);
    let mut end = (idx + match_len + 40).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    text[start..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Search a PDF's text page by page. opts: path, query (required; literal, or a
/// regex when `regex => true`), regex (default false), ignore_case (default
/// false). Returns `{ count, matched_pages, pages: [{ page, count, snippet }] }`
/// — `count` is total occurrences, page numbers are 1-based.
fn op_pdf_search(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let query = req_str(&opts, "query")?;
    if query.is_empty() {
        return Err(anyhow!("empty query"));
    }
    let ignore_case = opts
        .get("ignore_case")
        .and_then(flag_of)
        .unwrap_or(false);
    let regex_mode = opts.get("regex").and_then(flag_of).unwrap_or(false);
    // With `regex => 1` the query is a regular expression (compiled once).
    let re = if regex_mode {
        Some(
            regex::RegexBuilder::new(query)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| anyhow!("invalid regex: {e}"))?,
        )
    } else {
        None
    };
    let bytes = std::fs::read(path)?;
    let pages = lo_core::extract_pages_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;
    let needle = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };

    let mut total = 0usize;
    let mut hits = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let (c, idx, mlen, snip_src) = match &re {
            Some(r) => {
                let cnt = r.find_iter(page).count();
                let first = r.find(page);
                (
                    cnt,
                    first.map_or(0, |m| m.start()),
                    first.map_or(needle.len(), |m| m.len()),
                    page.clone(),
                )
            }
            None => {
                let hay = if ignore_case { page.to_lowercase() } else { page.clone() };
                let cnt = hay.matches(&needle).count();
                let idx = hay.find(&needle).unwrap_or(0);
                (cnt, idx, needle.len(), hay)
            }
        };
        if c > 0 {
            total += c;
            hits.push(json!({
                "page": i + 1,
                "count": c,
                "snippet": pdf_snippet(&snip_src, idx, mlen),
            }));
        }
    }
    Ok(json!({ "count": total, "matched_pages": hits.len(), "pages": hits }))
}

// ── burst (one file per page) ─────────────────────────────────────────────────

/// Split a PDF into one file per page. opts: path, dir => output directory,
/// prefix => filename stem (default: the source's stem). Files are written as
/// `{dir}/{prefix}-{n}.pdf` (1-based). Returns `{ count, files }`.
fn op_pdf_burst(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("page")
                .to_string()
        });
    let total = Document::load(path)
        .map_err(|e| anyhow!("load {path}: {e}"))?
        .get_pages()
        .len() as u32;

    let mut files = Vec::new();
    for p in 1..=total {
        let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
        let remove: Vec<u32> = (1..=total).filter(|&x| x != p).collect();
        doc.delete_pages(&remove);
        let out = format!("{dir}/{prefix}-{p}.pdf");
        doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Split a PDF into fixed-size page chunks. opts: path, dir => output directory,
/// size => pages per chunk (required, > 0), prefix => filename stem (default:
/// the source's stem). Files are `{dir}/{prefix}-{n}.pdf` (1-based; the last may
/// be shorter). Returns `{ count, files }`.
fn op_pdf_chunk(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let size = opts
        .get("size")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .ok_or_else(|| anyhow!("missing size (pages per chunk, > 0)"))? as u32;
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("chunk")
                .to_string()
        });
    let total = Document::load(path)
        .map_err(|e| anyhow!("load {path}: {e}"))?
        .get_pages()
        .len() as u32;

    let mut files = Vec::new();
    let mut start = 1u32;
    let mut idx = 1u32;
    while start <= total {
        let end = (start + size - 1).min(total);
        let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
        let remove: Vec<u32> = (1..=total).filter(|&p| p < start || p > end).collect();
        doc.delete_pages(&remove);
        let out = format!("{dir}/{prefix}-{idx}.pdf");
        doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
        files.push(out);
        start += size;
        idx += 1;
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Split a PDF into one file per page range. opts: path, dir => output
/// directory, ranges => [[start, end], …] (1-based, inclusive), prefix =>
/// filename stem (default: the source's stem). Files are `{dir}/{prefix}-{n}.pdf`
/// in range order. Returns `{ count, files }`.
fn op_pdf_split_ranges(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    let ranges = opts
        .get("ranges")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing ranges (expected [[start,end],…])"))?;
    let prefix = opts
        .get("prefix")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("range")
                .to_string()
        });
    let total = Document::load(path)
        .map_err(|e| anyhow!("load {path}: {e}"))?
        .get_pages()
        .len() as u32;

    let mut files = Vec::new();
    for (i, rg) in ranges.iter().enumerate() {
        let a = rg
            .as_array()
            .ok_or_else(|| anyhow!("range must be [start, end]"))?;
        let s = a
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("range start must be a number"))? as u32;
        let e = a.get(1).and_then(Value::as_u64).unwrap_or(s as u64) as u32;
        let (s, e) = (s.min(e), s.max(e));
        let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
        let remove: Vec<u32> = (1..=total).filter(|&p| p < s || p > e).collect();
        doc.delete_pages(&remove);
        let out = format!("{dir}/{prefix}-{}.pdf", i + 1);
        doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
        files.push(out);
    }
    Ok(json!({ "count": files.len(), "files": files }))
}

/// Split a PDF at its top-level bookmark (outline) boundaries — one file per
/// chapter. Each top-level outline entry that has a page starts a section that
/// runs until the next entry's page (the last runs to the end). opts: path, dir
/// => output directory (created if absent), prefix => filename stem. Returns
/// `{ count, files }`. Errors if the PDF has no bookmarks with page numbers.
fn op_pdf_split_bookmarks(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = req_str(&opts, "dir")?;
    std::fs::create_dir_all(dir)?;

    let outline = op_pdf_outline(json!({ "path": path }))?;
    let entries = outline
        .get("outline")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut marks: Vec<u64> = entries
        .iter()
        .filter_map(|e| e.get("page").and_then(Value::as_u64))
        .collect();
    marks.sort_unstable();
    marks.dedup();
    if marks.is_empty() {
        return Err(anyhow!("no bookmarks with page numbers to split on"));
    }

    let total = op_pdf_info(json!({ "path": path }))?
        .get("pages")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let ranges: Vec<Value> = marks
        .iter()
        .enumerate()
        .filter_map(|(i, &start)| {
            let end = if i + 1 < marks.len() {
                marks[i + 1].saturating_sub(1)
            } else {
                total
            };
            (start >= 1 && end >= start).then(|| json!([start, end]))
        })
        .collect();

    let mut sopts = json!({ "path": path, "dir": dir, "ranges": ranges });
    if let Some(p) = opts.get("prefix") {
        sopts["prefix"] = p.clone();
    }
    op_pdf_split_ranges(sopts)
}

/// Extract a PDF's text. opts: path; then either `dir` => write one
/// `page-{n}.txt` per page (returns `{ count, files }`), or `output` => write
/// the whole text joined by `separator` (default "\n\n") to one file (returns
/// `{ ok, path, pages, chars }`).
fn op_pdf_to_text(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let pages = lo_core::extract_pages_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;

    if let Some(dir) = opts.get("dir").and_then(Value::as_str) {
        let mut files = Vec::new();
        for (i, p) in pages.iter().enumerate() {
            let out = format!("{dir}/page-{}.txt", i + 1);
            std::fs::write(&out, p)?;
            files.push(out);
        }
        return Ok(json!({ "ok": true, "count": files.len(), "files": files }));
    }

    let output = req_str(&opts, "output")?.to_string();
    let sep = opts.get("separator").and_then(Value::as_str).unwrap_or("\n\n");
    let text = pages.join(sep);
    std::fs::write(&output, &text)?;
    Ok(json!({ "ok": true, "path": output, "pages": pages.len(), "chars": text.chars().count() }))
}

/// Word/character statistics for a PDF (the PDF analogue of `doc_stats`). opts:
/// path. Counts whitespace-delimited words and characters from extracted page
/// text. Returns `{ pages, words, chars, chars_no_spaces, per_page: [{ page,
/// words, chars }] }`.
fn op_pdf_stats(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let bytes = std::fs::read(path)?;
    let pages = lo_core::extract_pages_from_pdf(&bytes).map_err(|e| anyhow!("pdf parse: {e}"))?;

    let mut words = 0u64;
    let mut chars = 0u64;
    let mut chars_no_spaces = 0u64;
    let per_page: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let w = p.split_whitespace().count() as u64;
            let c = p.chars().count() as u64;
            words += w;
            chars += c;
            chars_no_spaces += p.chars().filter(|ch| !ch.is_whitespace()).count() as u64;
            json!({ "page": i + 1, "words": w, "chars": c })
        })
        .collect();
    Ok(json!({
        "pages": pages.len(),
        "words": words,
        "chars": chars,
        "chars_no_spaces": chars_no_spaces,
        "per_page": per_page,
    }))
}

/// Build a JPEG image XObject `Stream` (DeviceRGB / DCTDecode) from raw JPEG
/// bytes and dimensions. `with_compression(false)` so the writer doesn't deflate
/// the already-compressed JPEG.
fn jpeg_xobject(jpeg: Vec<u8>, w: u32, h: u32) -> lopdf::Stream {
    let mut dict = lopdf::Dictionary::new();
    dict.set("Type", "XObject");
    dict.set("Subtype", "Image");
    dict.set("Width", w as i64);
    dict.set("Height", h as i64);
    dict.set("ColorSpace", "DeviceRGB");
    dict.set("BitsPerComponent", 8i64);
    dict.set("Filter", "DCTDecode");
    lopdf::Stream::new(dict, jpeg).with_compression(false)
}

/// Stamp an image (logo/signature/watermark) onto PDF pages. opts: path =>
/// input, output, image => path, x/y => lower-left position in points
/// (default 36,36), width/height => size in points (default: the image's pixel
/// dimensions), pages => [1-based subset] (default all). Returns
/// `{ ok, path, stamped }`.
fn op_pdf_stamp_image(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let image_path = req_str(&opts, "image")?;

    let dynimg = image::open(image_path).map_err(|e| anyhow!("image {image_path}: {e}"))?;
    let (iw, ih) = (dynimg.width(), dynimg.height());
    let mut buf = std::io::Cursor::new(Vec::new());
    dynimg
        .to_rgb8()
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| anyhow!("encode jpeg: {e}"))?;
    let jpeg = buf.into_inner();

    let x = opts.get("x").and_then(Value::as_f64).unwrap_or(36.0) as f32;
    let y = opts.get("y").and_then(Value::as_f64).unwrap_or(36.0) as f32;
    let w = opts.get("width").and_then(Value::as_f64).unwrap_or(iw as f64) as f32;
    let h = opts.get("height").and_then(Value::as_f64).unwrap_or(ih as f64) as f32;
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let mut stamped = 0usize;
    for (num, pid) in pages {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        let stream = jpeg_xobject(jpeg.clone(), iw, ih);
        doc.insert_image(pid, stream, (x, y), (w, h))
            .map_err(|e| anyhow!("stamp page {num}: {e}"))?;
        stamped += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "stamped": stamped }))
}

/// Assemble one PDF from a mix of inputs in order: image files (png/jpg/gif/bmp/
/// webp/tiff) become fit-to-page pages, existing PDFs are merged in. opts:
/// inputs => [paths], output => path. Returns `{ ok, path, inputs, pages }`.
fn op_pdf_assemble(opts: Value) -> Result<Value> {
    let inputs = opts
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing inputs (expected array of paths)"))?;
    let output = req_str(&opts, "output")?.to_string();
    let is_image = |e: &str| {
        matches!(e, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff")
    };

    let mut parts: Vec<String> = Vec::new();
    let mut temps: Vec<String> = Vec::new();
    let tmpdir = std::env::temp_dir();
    for (i, inp) in inputs.iter().enumerate() {
        let p = inp
            .as_str()
            .ok_or_else(|| anyhow!("input path must be a string"))?;
        let ext = ext_of(p);
        if ext == "pdf" {
            parts.push(p.to_string());
        } else if is_image(&ext) {
            let tmp = tmpdir
                .join(format!("office-assemble-{}-{i}.pdf", std::process::id()))
                .to_string_lossy()
                .into_owned();
            op_images_to_pdf(json!({ "images": [p], "output": tmp }))?;
            parts.push(tmp.clone());
            temps.push(tmp);
        } else {
            return Err(anyhow!("unsupported input for assemble: {ext}"));
        }
    }

    op_pdf_merge(json!({ "inputs": parts, "path": output }))?;
    for t in &temps {
        std::fs::remove_file(t).ok();
    }
    let pages = Document::load(&output).map(|d| d.get_pages().len()).unwrap_or(0);
    Ok(json!({ "ok": true, "path": output, "inputs": inputs.len(), "pages": pages }))
}

/// Insert one PDF's pages into another after a given page. opts: path => base,
/// insert => PDF to splice in, output, position => 1-based page after which to
/// insert (0 = before all, default = end). Returns `{ ok, path, pages }`.
fn op_pdf_insert(opts: Value) -> Result<Value> {
    let base = req_str(&opts, "path")?;
    let insert = req_str(&opts, "insert")?;
    let output = req_str(&opts, "output")?.to_string();
    let total = Document::load(base)
        .map_err(|e| anyhow!("load {base}: {e}"))?
        .get_pages()
        .len() as u32;
    let position = opts
        .get("position")
        .and_then(Value::as_u64)
        .unwrap_or(total as u64)
        .min(total as u64) as u32;

    let tmpdir = std::env::temp_dir();
    let pid = std::process::id();
    let mut parts: Vec<String> = Vec::new();
    let mut temps: Vec<String> = Vec::new();
    if position > 0 {
        let a = tmpdir
            .join(format!("office-ins-a-{pid}.pdf"))
            .to_string_lossy()
            .into_owned();
        let pages: Vec<u32> = (1..=position).collect();
        op_pdf_split(json!({ "path": base, "pages": pages, "output": a }))?;
        parts.push(a.clone());
        temps.push(a);
    }
    parts.push(insert.to_string());
    if position < total {
        let b = tmpdir
            .join(format!("office-ins-b-{pid}.pdf"))
            .to_string_lossy()
            .into_owned();
        let pages: Vec<u32> = (position + 1..=total).collect();
        op_pdf_split(json!({ "path": base, "pages": pages, "output": b }))?;
        parts.push(b.clone());
        temps.push(b);
    }

    op_pdf_merge(json!({ "inputs": parts, "path": output }))?;
    for t in &temps {
        std::fs::remove_file(t).ok();
    }
    let pages = Document::load(&output).map(|d| d.get_pages().len()).unwrap_or(0);
    Ok(json!({ "ok": true, "path": output, "pages": pages }))
}

/// Draw filled or stroked rectangles on PDF pages — for color blocks, covering
/// regions, or highlights. opts: path, output, rects => [[x, y, w, h], …]
/// (points; y from the bottom), color (default black), fill (default true),
/// pages => [1-based subset] (default all). Returns `{ ok, path, pages, rects }`.
fn op_pdf_draw_rect(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let boxes: Vec<[f64; 4]> = opts
        .get("rects")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing rects (expected [[x,y,w,h],…])"))?
        .iter()
        .filter_map(four_floats)
        .collect();
    if boxes.is_empty() {
        return Err(anyhow!("no valid rects"));
    }
    let (r, g, b) = pdf_color01(opts.get("color"), [0, 0, 0]);
    let fill = opts.get("fill").and_then(flag_of).unwrap_or(true);
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut drawn = 0usize;
    for (num, page_id) in doc.get_pages() {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        let mut ops = vec![Operation::new("q", vec![])];
        for bx in &boxes {
            ops.push(Operation::new(
                if fill { "rg" } else { "RG" },
                vec![r.into(), g.into(), b.into()],
            ));
            ops.push(Operation::new(
                "re",
                vec![bx[0].into(), bx[1].into(), bx[2].into(), bx[3].into()],
            ));
            ops.push(Operation::new(if fill { "f" } else { "S" }, vec![]));
        }
        ops.push(Operation::new("Q", vec![]));
        doc.add_to_page_content(page_id, Content { operations: ops })
            .map_err(|e| anyhow!("draw page {num}: {e}"))?;
        drawn += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "pages": drawn, "rects": boxes.len() }))
}

/// Add text at a position on PDF pages (labels, stamps, annotations). opts:
/// path, output, text, x/y => position in points (default 72,72; y from the
/// bottom), size (default 12), color (default black), pages => [1-based subset]
/// (default all). Returns `{ ok, path, pages }`.
fn op_pdf_add_text(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let text = req_str(&opts, "text")?.to_string();
    let x = opts.get("x").and_then(Value::as_f64).unwrap_or(72.0);
    let y = opts.get("y").and_then(Value::as_f64).unwrap_or(72.0);
    let size = opts.get("size").and_then(Value::as_f64).unwrap_or(12.0);
    let (r, g, b) = pdf_color01(opts.get("color"), [0, 0, 0]);
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut added = 0usize;
    for (num, page_id) in doc.get_pages() {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        ensure_helvetica(&mut doc, page_id, "SOFT")?;
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("Tf", vec!["SOFT".into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::string_literal(text.clone())]),
            Operation::new("ET", vec![]),
        ];
        doc.add_to_page_content(page_id, Content { operations: ops })
            .map_err(|e| anyhow!("text page {num}: {e}"))?;
        added += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "pages": added }))
}

/// Draw lines on PDF pages (dividers, underlines, signature lines). opts: path,
/// output, lines => [[x0, y0, x1, y1], …] (points; y from the bottom), color
/// (default black), width (line width in points, default 1), pages => [1-based
/// subset] (default all). Returns `{ ok, path, pages, lines }`.
fn op_pdf_draw_line(opts: Value) -> Result<Value> {
    use lopdf::content::{Content, Operation};
    let path = req_str(&opts, "path")?;
    let out = req_str(&opts, "output")?.to_string();
    let segs: Vec<[f64; 4]> = opts
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing lines (expected [[x0,y0,x1,y1],…])"))?
        .iter()
        .filter_map(four_floats)
        .collect();
    if segs.is_empty() {
        return Err(anyhow!("no valid lines"));
    }
    let (r, g, b) = pdf_color01(opts.get("color"), [0, 0, 0]);
    let width = opts.get("width").and_then(Value::as_f64).unwrap_or(1.0);
    let subset: Option<std::collections::BTreeSet<u32>> = opts
        .get("pages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u32)).collect());

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut drawn = 0usize;
    for (num, page_id) in doc.get_pages() {
        if let Some(set) = &subset {
            if !set.contains(&num) {
                continue;
            }
        }
        let mut ops = vec![
            Operation::new("q", vec![]),
            Operation::new("RG", vec![r.into(), g.into(), b.into()]),
            Operation::new("w", vec![width.into()]),
        ];
        for s in &segs {
            ops.push(Operation::new("m", vec![s[0].into(), s[1].into()]));
            ops.push(Operation::new("l", vec![s[2].into(), s[3].into()]));
            ops.push(Operation::new("S", vec![]));
        }
        ops.push(Operation::new("Q", vec![]));
        doc.add_to_page_content(page_id, Content { operations: ops })
            .map_err(|e| anyhow!("line page {num}: {e}"))?;
        drawn += 1;
    }
    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "pages": drawn, "lines": segs.len() }))
}

/// Add a clickable URI link annotation to a PDF page (the PDF analogue of a
/// docx hyperlink). opts: path, output (default in place), page => 1-based page
/// number (default 1), url (required), rect => `[x0,y0,x1,y1]` clickable area in
/// PDF points (default the whole page). Returns `{ ok, path, page }`.
fn op_pdf_add_link(opts: Value) -> Result<Value> {
    use lopdf::Dictionary;
    let path = req_str(&opts, "path")?;
    let out = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let page = opts.get("page").and_then(Value::as_u64).unwrap_or(1) as u32;
    let url = req_str(&opts, "url")?.to_string();

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let pid = *pages
        .get(&page)
        .ok_or_else(|| anyhow!("page {page} out of range (1..={})", pages.len()))?;
    let rect = match opts.get("rect") {
        Some(v) => four_floats(v).ok_or_else(|| anyhow!("rect must be [x0,y0,x1,y1]"))?,
        None => {
            let (w, h) = pdf_page_size(&doc, pid);
            [0.0, 0.0, w, h]
        }
    };

    let mut action = Dictionary::new();
    action.set("S", Object::Name(b"URI".to_vec()));
    action.set("URI", Object::string_literal(url));
    let action_id = doc.add_object(Object::Dictionary(action));

    let mut annot = Dictionary::new();
    annot.set("Type", Object::Name(b"Annot".to_vec()));
    annot.set("Subtype", Object::Name(b"Link".to_vec()));
    annot.set(
        "Rect",
        Object::Array(rect.iter().map(|&f| Object::Real(f as f32)).collect()),
    );
    annot.set("Border", Object::Array(vec![0.into(), 0.into(), 0.into()]));
    annot.set("A", Object::Reference(action_id));
    let annot_id = doc.add_object(Object::Dictionary(annot));

    let page_dict = doc
        .get_object_mut(pid)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dict: {e}"))?;
    match page_dict.get(b"Annots").ok().and_then(|o| o.as_array().ok()) {
        Some(existing) => {
            let mut a = existing.clone();
            a.push(Object::Reference(annot_id));
            page_dict.set("Annots", Object::Array(a));
        }
        None => {
            page_dict.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        }
    }

    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "page": page }))
}

/// Extract every URI link annotation from a PDF (the analogue of `doc_links`).
/// opts: path. Returns `{ links: [{ page, url, rect }], count }` with 1-based
/// page numbers and the clickable rectangle in PDF points.
fn op_pdf_links(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut links = Vec::new();
    for (num, pid) in doc.get_pages() {
        let Some(annots) = doc
            .get_object(pid)
            .ok()
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|o| o.as_array().ok())
        else {
            continue;
        };
        for a in annots {
            // An annotation may be an inline dict or a reference to one.
            let dict = match a {
                Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
                Object::Dictionary(d) => Some(d),
                _ => None,
            };
            let Some(dict) = dict else { continue };
            if dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Link".as_slice()) {
                continue;
            }
            // Resolve the action dict (inline or referenced) and read /URI.
            let action = match dict.get(b"A").ok() {
                Some(Object::Reference(id)) => {
                    doc.get_object(*id).ok().and_then(|o| o.as_dict().ok())
                }
                Some(Object::Dictionary(d)) => Some(d),
                _ => None,
            };
            let Some(action) = action else { continue };
            if action.get(b"S").and_then(|o| o.as_name()).ok() != Some(b"URI".as_slice()) {
                continue;
            }
            let Some(url) = pdf_dict_str(action, b"URI") else {
                continue;
            };
            let rect: Vec<f64> = dict
                .get(b"Rect")
                .ok()
                .and_then(|o| o.as_array().ok())
                .map(|a| a.iter().filter_map(obj_num).collect())
                .unwrap_or_default();
            links.push(json!({ "page": num, "url": url, "rect": rect }));
        }
    }
    let count = links.len();
    Ok(json!({ "links": links, "count": count }))
}

/// List every annotation in a PDF (the generalization of `pdf_links` — covers
/// highlights, text/comment notes, links, …). opts: path. Returns
/// `{ annotations: [{ page, subtype, rect, contents?, uri? }], count }` with
/// 1-based page numbers; `contents` is the markup text when present and `uri` the
/// target for link annotations.
fn op_pdf_annotations(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut annots = Vec::new();
    for (num, pid) in doc.get_pages() {
        let Some(list) = doc
            .get_object(pid)
            .ok()
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|o| o.as_array().ok())
        else {
            continue;
        };
        for a in list {
            let dict = match a {
                Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
                Object::Dictionary(d) => Some(d),
                _ => None,
            };
            let Some(dict) = dict else { continue };
            let subtype = dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .ok()
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let rect: Vec<f64> = dict
                .get(b"Rect")
                .ok()
                .and_then(|o| o.as_array().ok())
                .map(|a| a.iter().filter_map(obj_num).collect())
                .unwrap_or_default();
            let mut entry = json!({ "page": num, "subtype": subtype, "rect": rect });
            if let Some(c) = pdf_dict_str(dict, b"Contents") {
                if !c.is_empty() {
                    entry["contents"] = json!(c);
                }
            }
            // Link annotations carry their target in /A /URI.
            let action = match dict.get(b"A").ok() {
                Some(Object::Reference(id)) => {
                    doc.get_object(*id).ok().and_then(|o| o.as_dict().ok())
                }
                Some(Object::Dictionary(d)) => Some(d),
                _ => None,
            };
            if let Some(uri) = action.and_then(|act| pdf_dict_str(act, b"URI")) {
                entry["uri"] = json!(uri);
            }
            annots.push(entry);
        }
    }
    let count = annots.len();
    Ok(json!({ "annotations": annots, "count": count }))
}

/// Strip annotations from a PDF (links, comments, highlights, …) to sanitize a
/// file before sharing. opts: path, output (default in place), subtype => keep
/// all annotations except this one `/Subtype` (e.g. "Link", "Highlight",
/// "Text"); omit to remove every annotation. Returns `{ ok, path, removed }`.
fn op_pdf_remove_annotations(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let out = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let subtype = opts.get("subtype").and_then(Value::as_str);

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    let mut removed = 0u64;
    for pid in page_ids {
        let Some(annots) = doc
            .get_object(pid)
            .ok()
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|o| o.as_array().ok())
            .cloned()
        else {
            continue;
        };
        // Decide what to keep using immutable reads, then apply one mutation.
        let kept: Vec<Object> = match subtype {
            Some(want) => annots
                .into_iter()
                .filter(|a| {
                    let st = match a {
                        Object::Reference(id) => {
                            doc.get_object(*id).ok().and_then(|o| o.as_dict().ok())
                        }
                        Object::Dictionary(d) => Some(d),
                        _ => None,
                    }
                    .and_then(|d| d.get(b"Subtype").and_then(|o| o.as_name()).ok())
                    .map(|n| n.to_vec());
                    let matches = st.as_deref() == Some(want.as_bytes());
                    if matches {
                        removed += 1;
                    }
                    !matches
                })
                .collect(),
            None => {
                removed += annots.len() as u64;
                Vec::new()
            }
        };
        if let Ok(d) = doc.get_object_mut(pid).and_then(|o| o.as_dict_mut()) {
            if kept.is_empty() {
                d.remove(b"Annots");
            } else {
                d.set("Annots", Object::Array(kept));
            }
        }
    }

    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "removed": removed }))
}

/// Add a highlight annotation over a rectangle on a PDF page. opts: path, output
/// (default in place), page => 1-based page number (default 1), rect =>
/// `[x0,y0,x1,y1]` in PDF points (required), color => `[r,g,b]` 0–255 (default
/// yellow), opacity => 0..1 (default 1.0). Returns `{ ok, path, page }`.
fn op_pdf_highlight(opts: Value) -> Result<Value> {
    use lopdf::Dictionary;
    let path = req_str(&opts, "path")?;
    let out = opts
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or(path)
        .to_string();
    let page = opts.get("page").and_then(Value::as_u64).unwrap_or(1) as u32;
    let rect = four_floats(
        opts.get("rect")
            .ok_or_else(|| anyhow!("missing rect [x0,y0,x1,y1]"))?,
    )
    .ok_or_else(|| anyhow!("rect must be [x0,y0,x1,y1]"))?;
    let (r, g, b) = pdf_color01(opts.get("color"), [255, 255, 0]);
    let opacity = opts.get("opacity").and_then(Value::as_f64).unwrap_or(1.0);
    let [x0, y0, x1, y1] = rect;

    let mut doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let pages = doc.get_pages();
    let pid = *pages
        .get(&page)
        .ok_or_else(|| anyhow!("page {page} out of range (1..={})", pages.len()))?;

    let real = |f: f64| Object::Real(f as f32);
    let mut annot = Dictionary::new();
    annot.set("Type", Object::Name(b"Annot".to_vec()));
    annot.set("Subtype", Object::Name(b"Highlight".to_vec()));
    annot.set("Rect", Object::Array(vec![real(x0), real(y0), real(x1), real(y1)]));
    // QuadPoints: UL, UR, LL, LR of the highlighted quad.
    annot.set(
        "QuadPoints",
        Object::Array(vec![
            real(x0), real(y1), real(x1), real(y1), real(x0), real(y0), real(x1), real(y0),
        ]),
    );
    annot.set("C", Object::Array(vec![real(r), real(g), real(b)]));
    annot.set("CA", real(opacity));
    let annot_id = doc.add_object(Object::Dictionary(annot));

    let page_dict = doc
        .get_object_mut(pid)
        .and_then(|o| o.as_dict_mut())
        .map_err(|e| anyhow!("page dict: {e}"))?;
    match page_dict.get(b"Annots").ok().and_then(|o| o.as_array().ok()) {
        Some(existing) => {
            let mut a = existing.clone();
            a.push(Object::Reference(annot_id));
            page_dict.set("Annots", Object::Array(a));
        }
        None => {
            page_dict.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        }
    }

    doc.save(&out).map_err(|e| anyhow!("save {out}: {e}"))?;
    Ok(json!({ "ok": true, "path": out, "page": page }))
}
