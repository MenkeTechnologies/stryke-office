// Embedded-media extraction. Office containers and PDFs carry their raster
// media as discrete parts — OOXML in `*/media/`, ODF in `Pictures/`, PDF as
// image XObject streams. `extract_images` pulls each one out, decodes it into
// the shared image-handle table (so the result flows straight into the image
// surface: resize/convert/save), and optionally writes the originals to a
// directory. This is format-internal plumbing, not generic data work.
//
// `DynamicImage` is already in scope from the image_ops include.

/// An image lifted out of a document.
struct Lifted {
    name: String,
    bytes: Vec<u8>,          // original (container) or re-encoded (pdf) bytes
    img: Option<DynamicImage>,
}

fn is_container_media(name: &str) -> bool {
    !name.ends_with('/')
        && (name.contains("/media/") || name.starts_with("Pictures/") || name.starts_with("Thumbnails/"))
        && name != "Thumbnails/thumbnail.png" // skip the ODF preview thumbnail
}

/// Lift every media part out of an OOXML / ODF zip container.
fn lift_container(bytes: &[u8]) -> Result<Vec<Lifted>> {
    let mut out = Vec::new();
    for name in zip_entry_names(bytes)? {
        if !is_container_media(&name) {
            continue;
        }
        let data = match read_zip_entry(bytes, &name) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let img = image::load_from_memory(&data).ok();
        let base = Path::new(&name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&name)
            .to_string();
        out.push(Lifted { name: base, bytes: data, img });
    }
    Ok(out)
}

/// Build a DynamicImage from a raw, uncompressed PDF sample buffer for the
/// simple device color spaces (8-bit RGB / Gray). Returns None for anything
/// that needs a palette, CMYK conversion, or sub-byte depth.
fn pdf_raw_to_image(samples: &[u8], w: u32, h: u32, cs: &str, bpc: i64) -> Option<DynamicImage> {
    if bpc != 8 || w == 0 || h == 0 {
        return None;
    }
    let n = (w as usize) * (h as usize);
    match cs {
        "DeviceRGB" | "RGB" | "CalRGB" => {
            if samples.len() < n * 3 {
                return None;
            }
            image::RgbImage::from_raw(w, h, samples[..n * 3].to_vec()).map(DynamicImage::ImageRgb8)
        }
        "DeviceGray" | "G" | "CalGray" => {
            if samples.len() < n {
                return None;
            }
            image::GrayImage::from_raw(w, h, samples[..n].to_vec()).map(DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

/// The color-space *name* of an image XObject, following a one-level reference
/// and the leading name of an array form (e.g. `[/ICCBased ...]` -> "ICCBased").
fn pdf_colorspace_name(dict: &lopdf::Dictionary, doc: &lopdf::Document) -> String {
    let resolve = |o: &lopdf::Object| -> Option<lopdf::Object> {
        match o {
            lopdf::Object::Reference(id) => doc.get_object(*id).ok().cloned(),
            other => Some(other.clone()),
        }
    };
    let Ok(cs) = dict.get(b"ColorSpace").or_else(|_| dict.get(b"CS")) else {
        return String::new();
    };
    match resolve(cs) {
        Some(lopdf::Object::Name(n)) => String::from_utf8_lossy(&n).into_owned(),
        Some(lopdf::Object::Array(a)) => a
            .first()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Lift image XObjects out of a PDF. DCTDecode streams are JPEG verbatim;
/// FlateDecode raw samples are reconstructed for device RGB/Gray. Returns the
/// count of images that were recognized but couldn't be decoded.
fn lift_pdf(path: &str) -> Result<(Vec<Lifted>, usize)> {
    use lopdf::{Document, Object};
    let doc = Document::load(path).map_err(|e| anyhow!("load {path}: {e}"))?;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut idx = 0usize;
    for (_id, obj) in doc.objects.iter() {
        let Object::Stream(st) = obj else { continue };
        if st.dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()) != Some(b"Image".as_slice()) {
            continue;
        }
        idx += 1;
        let filters: Vec<String> = st
            .filters()
            .map(|v| v.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect())
            .unwrap_or_default();
        let is_dct = filters.iter().any(|f| f == "DCTDecode");
        let is_jpx = filters.iter().any(|f| f == "JPXDecode");

        if is_dct {
            // The stream content is a complete JPEG.
            let img = image::load_from_memory(&st.content).ok();
            out.push(Lifted { name: format!("image{idx}.jpg"), bytes: st.content.clone(), img });
            continue;
        }
        if is_jpx {
            skipped += 1; // JPEG2000 — not decoded
            continue;
        }
        // Flate/raw: reconstruct from samples.
        let w = st.dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0) as u32;
        let h = st.dict.get(b"Height").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0) as u32;
        let bpc = st.dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
        let cs = pdf_colorspace_name(&st.dict, &doc);
        let samples = st.decompressed_content().unwrap_or_else(|_| st.content.clone());
        match pdf_raw_to_image(&samples, w, h, &cs, bpc) {
            Some(img) => {
                let mut png = Vec::new();
                let ok = img
                    .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                    .is_ok();
                out.push(Lifted {
                    name: format!("image{idx}.png"),
                    bytes: if ok { png } else { Vec::new() },
                    img: Some(img),
                });
            }
            None => skipped += 1,
        }
    }
    Ok((out, skipped))
}

fn op_extract_images(opts: Value) -> Result<Value> {
    let path = req_str(&opts, "path")?;
    let dir = opts.get("dir").and_then(Value::as_str);
    let ext = ext_of(path);

    let (lifted, mut skipped) = if ext == "pdf" {
        lift_pdf(path)?
    } else if is_ooxml(&ext) || is_odf(&ext) {
        (lift_container(&std::fs::read(path)?)?, 0)
    } else {
        return Err(anyhow!("unsupported format for image extraction: {ext}"));
    };

    if let Some(d) = dir {
        std::fs::create_dir_all(d)?;
    }

    let mut images = Vec::new();
    for lf in lifted {
        let mut entry = json!({"name": lf.name});
        if let Some(img) = &lf.img {
            let (w, h) = (img.width(), img.height());
            let handle = insert_image(img.clone());
            entry["handle"] = json!(handle);
            entry["width"] = json!(w);
            entry["height"] = json!(h);
        } else {
            // recognized but not decodable into a handle
            skipped += 1;
        }
        if let Some(d) = dir {
            if !lf.bytes.is_empty() {
                let safe = Path::new(&lf.name)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("image.bin");
                let out_path = Path::new(d).join(safe);
                std::fs::write(&out_path, &lf.bytes)?;
                entry["path"] = json!(out_path.to_string_lossy());
            }
        }
        images.push(entry);
    }

    let count = images.len();
    Ok(json!({"images": images, "count": count, "skipped": skipped}))
}
