// Minimal PPTX (OOXML PresentationML) writer.
//
// No Rust pptx crate is mature enough to depend on for the 30-year horizon,
// so the package owns the OOXML layer directly: a pptx is a zip of XML parts.
// This emits the minimal part set LibreOffice / PowerPoint require to open a
// deck — [Content_Types].xml, package + presentation rels, one slide master,
// one slide layout, one theme, and one slide part per input slide. Each
// slide's title + body lines land as <a:p>/<a:t> runs in a title text body,
// which the reader side (`extract_paragraphs` over `a:p`) round-trips.

fn pptx_content_types(n: usize) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
"#,
    );
    for i in 1..=n {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/slides/slide{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\n"
        ));
    }
    s.push_str("</Types>\n");
    s
}

fn pptx_root_rels() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
</Relationships>
"#
}

fn pptx_presentation(n: usize) -> String {
    let mut sldids = String::new();
    for i in 1..=n {
        // slide rIds start after the master (rId1) -> rId{i+1}
        sldids.push_str(&format!(
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            255 + i,
            i + 1
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst>{sldids}</p:sldIdLst>
<p:sldSz cx="12192000" cy="6858000"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>
"#
    )
}

fn pptx_presentation_rels(n: usize) -> String {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>
"#,
    );
    for i in 1..=n {
        s.push_str(&format!(
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{i}.xml\"/>\n",
            i + 1
        ));
    }
    s.push_str(&format!(
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\"/>\n</Relationships>\n",
        n + 2
    ));
    s
}

/// One DrawingML paragraph for a slide body item. A string uses defaults; an
/// object `{text, bold, italic, size (pt), color}` sets run properties.
/// `default_pt` is the fallback size (pptx `sz` is hundredths of a point).
fn pptx_para(item: &Value, default_pt: u32) -> String {
    let (text, bold, italic, size_pt, color) = match item {
        Value::Object(o) => (
            cell_to_string(o.get("text").unwrap_or(&Value::Null)),
            o.get("bold").and_then(Value::as_bool).unwrap_or(false),
            o.get("italic").and_then(Value::as_bool).unwrap_or(false),
            o.get("size").and_then(Value::as_f64).map(|s| s as u32).unwrap_or(default_pt),
            o.get("color").and_then(Value::as_str).map(|c| c.trim_start_matches('#').to_string()),
        ),
        other => (cell_to_string(other), false, false, default_pt, None),
    };
    let mut rpr = format!("sz=\"{}\"", size_pt * 100);
    if bold {
        rpr.push_str(" b=\"1\"");
    }
    if italic {
        rpr.push_str(" i=\"1\"");
    }
    let fill = color
        .map(|c| format!("<a:solidFill><a:srgbClr val=\"{c}\"/></a:solidFill>"))
        .unwrap_or_default();
    format!(
        "<a:p><a:r><a:rPr lang=\"en-US\" {rpr}>{fill}</a:rPr><a:t>{}</a:t></a:r></a:p>",
        xml_escape(&text)
    )
}

fn pptx_slide(title: &str, body: &[Value]) -> String {
    let mut paras = pptx_para(&Value::String(title.to_string()), 28);
    // Title paragraph is bold by convention.
    paras = paras.replacen("sz=\"2800\"", "sz=\"2800\" b=\"1\"", 1);
    for item in body {
        paras.push_str(&pptx_para(item, 18));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
<p:sp>
<p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="838200" y="365125"/><a:ext cx="10515600" cy="6127750"/></a:xfrm></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/>{paras}</p:txBody>
</p:sp>
</p:spTree></p:cSld>
<p:clrMapOvr><a:overrideClrMapping bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/></p:clrMapOvr>
</p:sld>
"#
    )
}

fn pptx_slide_rels() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>
"#
}

fn pptx_slide_master() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>
"#
}

fn pptx_slide_master_rels() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/>
</Relationships>
"#
}

fn pptx_slide_layout() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1">
<p:cSld name="Blank"><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>
</p:sldLayout>
"#
}

fn pptx_slide_layout_rels() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>
"#
}

fn pptx_theme() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office Theme">
<a:themeElements>
<a:clrScheme name="Office">
<a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
<a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
<a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
<a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:fontScheme name="Office">
<a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
<a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="Office">
<a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>
<a:lnStyleLst><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>
<a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements>
</a:theme>
"#
}

fn write_pptx(path: &str, slides: &[(String, Vec<Value>)]) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let n = slides.len().max(1);
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let put = |zw: &mut zip::ZipWriter<Cursor<Vec<u8>>>, name: &str, data: &str| -> Result<()> {
        zw.start_file(name, opt)?;
        zw.write_all(data.as_bytes())?;
        Ok(())
    };

    put(&mut zw, "[Content_Types].xml", &pptx_content_types(n))?;
    put(&mut zw, "_rels/.rels", pptx_root_rels())?;
    put(&mut zw, "ppt/presentation.xml", &pptx_presentation(n))?;
    put(&mut zw, "ppt/_rels/presentation.xml.rels", &pptx_presentation_rels(n))?;
    put(&mut zw, "ppt/slideMasters/slideMaster1.xml", pptx_slide_master())?;
    put(&mut zw, "ppt/slideMasters/_rels/slideMaster1.xml.rels", pptx_slide_master_rels())?;
    put(&mut zw, "ppt/slideLayouts/slideLayout1.xml", pptx_slide_layout())?;
    put(&mut zw, "ppt/slideLayouts/_rels/slideLayout1.xml.rels", pptx_slide_layout_rels())?;
    put(&mut zw, "ppt/theme/theme1.xml", pptx_theme())?;

    if slides.is_empty() {
        put(&mut zw, "ppt/slides/slide1.xml", &pptx_slide("", &[]))?;
        put(&mut zw, "ppt/slides/_rels/slide1.xml.rels", pptx_slide_rels())?;
    } else {
        for (i, (title, body)) in slides.iter().enumerate() {
            let idx = i + 1;
            put(&mut zw, &format!("ppt/slides/slide{idx}.xml"), &pptx_slide(title, body))?;
            put(
                &mut zw,
                &format!("ppt/slides/_rels/slide{idx}.xml.rels"),
                pptx_slide_rels(),
            )?;
        }
    }

    let cursor = zw.finish()?;
    std::fs::write(path, cursor.into_inner())?;
    Ok(())
}
