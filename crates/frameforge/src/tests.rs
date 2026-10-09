use super::*;
use std::io::Write;
fn zip(entries: &[(&str, Vec<u8>, bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, data, deflate) in entries {
        let local = out.len() as u32;
        let compressed = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        } else {
            data.clone()
        };
        let method = if *deflate { 8u16 } else { 0 };
        let crc = crc32fast::hash(data);
        out.extend_from_slice(b"PK\x03\x04");
        for n in [20u16, 0, method, 0, 0] {
            out.extend(n.to_le_bytes());
        }
        for n in [crc, compressed.len() as u32, data.len() as u32] {
            out.extend(n.to_le_bytes());
        }
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(compressed);
        directory.extend_from_slice(b"PK\x01\x02");
        for n in [20u16, 20, 0, method, 0, 0] {
            directory.extend(n.to_le_bytes());
        }
        for n in [crc, (out.len() - local as usize - 30 - name.len()) as u32, data.len() as u32] {
            directory.extend(n.to_le_bytes());
        }
        for n in [name.len() as u16, 0, 0, 0, 0] {
            directory.extend(n.to_le_bytes());
        }
        directory.extend(0u32.to_le_bytes());
        directory.extend(local.to_le_bytes());
        directory.extend(name.as_bytes());
    }
    let at = out.len();
    out.extend(&directory);
    out.extend_from_slice(b"PK\x05\x06");
    for n in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend(n.to_le_bytes());
    }
    out.extend((directory.len() as u32).to_le_bytes());
    out.extend((at as u32).to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}
fn manifest(layers: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"format":"frameforge-project","formatVersion":1,"project":{"name":"Test","layers":layers},"assets":[]})).unwrap()
}
fn archive(layers: Value) -> Archive {
    read_archive(&zip(&[("manifest.json", manifest(layers), true)])).unwrap()
}
fn text(id: &str) -> Value {
    json!({"id":id,"name":id,"type":"text","text":"Hello","x":0,"y":0,"w":80,"h":50,"font":"Inter","size":20})
}
#[test]
fn stored_and_deflated() {
    for d in [false, true] {
        assert!(read_archive(&zip(&[("manifest.json", manifest(json!([])), d)])).is_ok());
    }
}
#[test]
fn oversize_entry() {
    assert!(read_archive(&zip(&[("manifest.json", vec![0; 32 * 1024 * 1024 + 1], false)])).is_err());
}
#[test]
fn lying_deflate_bomb() {
    let mut b = zip(&[("manifest.json", vec![0; 32 * 1024 * 1024 + 1], true)]);
    let cd = b.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    b[22..26].copy_from_slice(&1u32.to_le_bytes());
    b[cd + 24..cd + 28].copy_from_slice(&1u32.to_le_bytes());
    assert!(read_archive(&b).unwrap_err().contains("inflate"));
}
#[test]
fn entry_count() {
    assert!(read_archive(&zip(&vec![("manifest.json", vec![], false); 257])).is_err());
}
#[test]
fn traversal() {
    assert!(read_archive(&zip(&[("../manifest.json", manifest(json!([])), false)])).is_err());
}
#[test]
fn duplicate_paths() {
    let m = manifest(json!([]));
    assert!(read_archive(&zip(&[("manifest.json", m.clone(), false), ("manifest.json", m, false)])).is_err());
}
#[test]
fn bad_version() {
    let mut m: Value = serde_json::from_slice(&manifest(json!([]))).unwrap();
    m["formatVersion"] = json!(2);
    assert!(read_archive(&zip(&[("manifest.json", serde_json::to_vec(&m).unwrap(), false)])).is_err());
}
#[test]
fn layer_limit() {
    assert!(read_archive(&zip(&[("manifest.json", manifest(json!(vec![text("a"); 501])), false)])).is_err());
}
#[test]
fn encrypted() {
    let mut b = zip(&[("manifest.json", manifest(json!([])), false)]);
    let cd = b.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    b[cd + 8] = 1;
    assert!(read_archive(&b).is_err());
}
#[test]
fn corrupt_crc() {
    let mut b = zip(&[("manifest.json", manifest(json!([])), false)]);
    b[43] ^= 1;
    assert!(read_archive(&b).is_err());
}
#[test]
fn truncated_zip() {
    for n in 0..80 {
        assert!(read_archive(&vec![0; n]).is_err());
    }
}
#[test]
fn percent_geometry() {
    assert_eq!(convert::pixel_rect(&json!({"x":10,"y":20,"w":50,"h":50}), [1280., 720.]).unwrap(), [128., 144., 640., 360.]);
}
#[test]
fn contain_cover() {
    let s = [10., 20., 100., 100.];
    assert_eq!(convert::image_rect(&json!({"fit":"contain"}), s, [200., 100.]).unwrap(), [10., 45., 100., 50.]);
    assert_eq!(convert::image_rect(&json!({"fit":"cover"}), s, [200., 100.]).unwrap(), [-40., 20., 200., 100.]);
}
#[test]
fn anchor_scale_offset() {
    assert_eq!(
        convert::image_rect(&json!({"fit":"full-bleed","contentScale":2,"contentAnchorX":"right","contentOffsetY":0.5}), [0., 0., 100., 100.], [200., 100.])
            .unwrap(),
        [-300., 0., 400., 200.]
    );
}
#[test]
fn text_mapping() {
    let s = convert::text_style(&json!({"font":"Montserrat","bold":true,"size":99,"color":"#f00","textAlign":"right"})).unwrap();
    assert_eq!(s["weight"], 900.);
    assert_eq!(s["size"], 99.);
    assert_eq!(s["align"], "right");
    assert_eq!(convert::text_style(&json!({"font":"Impact","bold":true})).unwrap()["font"], "Anton");
}
#[test]
fn stroke_mapping() {
    let st = convert::stroke(&json!({"stroke":true,"strokeWidth":0.04}), 100.).unwrap().unwrap();
    assert_eq!(st["params"]["size"], 4.);
    assert_eq!(st["params"]["position"], "outside");
}
#[test]
fn shadow_mapping() {
    let st = convert::shadow(&json!({"color":"rgba(1,2,3,.5)","offsetX":3,"offsetY":4,"blur":8})).unwrap().unwrap();
    assert_eq!(st["params"]["distance"], 5.);
    assert_eq!(st["params"]["opacity"], 50.);
    assert_eq!(st["params"]["size"], 8.);
    assert_eq!(st["params"]["useGlobalLight"], false);
    assert!((st["params"]["angle"].as_f64().unwrap().to_radians().cos() * -5. - 3.).abs() < 1e-6);
}
#[test]
fn default_shadow() {
    let st = convert::shadow(&json!(true)).unwrap().unwrap();
    assert_eq!(st["params"]["size"], 7.);
    assert_eq!(st["params"]["opacity"], 85.);
    assert!(convert::shadow(&json!(false)).unwrap().is_none());
}
#[test]
fn colors() {
    assert_eq!(convert::color("#ff000080").unwrap(), [1., 0., 0., 128. / 255.]);
    assert!(convert::color("#☃abc").is_err());
}
#[test]
fn z_order() {
    let mut s = Session::new();
    let r = import_into(&mut s, &archive(json!([text("top"), text("bottom")])), &ImportOptions::default()).unwrap();
    let doc = &s.active().unwrap().doc;
    assert_eq!(doc.layers.last().unwrap().id.0, r.layers["top"]);
    assert_eq!(doc.layers[1].id.0, r.layers["bottom"]);
}
#[test]
fn missing_font_warning() {
    let mut l = text("missing");
    l["font"] = json!("Not a real font P1");
    let mut s = Session::new();
    let r = import_into(&mut s, &archive(json!([l])), &ImportOptions::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("missing") && w.contains("font")));
    assert_eq!(r.missing_fonts, vec!["Not a real font P1"]);
}
#[test]
fn unsupported_warning() {
    let mut s = Session::new();
    let r = import_into(&mut s, &archive(json!([{"id":"shape","name":"Circle","type":"shape"}])), &ImportOptions::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("Circle") && w.contains("shape")));
}
#[test]
fn feature_warning() {
    let mut l = text("headline");
    l["imageAdjustments"] = json!({"contrast":2});
    let r = import_into(&mut Session::new(), &archive(json!([l])), &ImportOptions::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("headline") && w.contains("imageAdjustments")));
}
#[test]
fn failed_import_rolls_back() {
    let mut l = text("bad");
    l["w"] = json!(0);
    let mut s = Session::new();
    assert!(import_into(&mut s, &archive(json!([l])), &ImportOptions::default()).is_err());
    assert!(s.documents().is_empty());
}

/// External media only. Set FRAMEFORGE_FIXTURES to a directory containing the two
/// fixture sets, FRAMEFORGE_FONTS_DIR to TTF conversions, and FRAMEFORGE_EVIDENCE
/// to an output directory outside the repository.
#[test]
#[ignore]
fn golden_fixtures() {
    let roots = std::env::var("FRAMEFORGE_FIXTURES").unwrap();
    let fonts = std::path::PathBuf::from(std::env::var("FRAMEFORGE_FONTS_DIR").unwrap());
    let evidence = std::path::PathBuf::from(std::env::var("FRAMEFORGE_EVIDENCE").unwrap());
    std::fs::create_dir_all(&evidence).unwrap();
    let mut fixtures = Vec::new();
    for root in std::env::split_paths(&roots) {
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                for e in std::fs::read_dir(p).unwrap().flatten() {
                    if e.path().extension().is_some_and(|e| e == "frameforge") {
                        fixtures.push(e.path());
                    }
                }
            } else if p.extension().is_some_and(|e| e == "frameforge") {
                fixtures.push(p);
            }
        }
    }
    fixtures.sort();
    let mut metrics = Vec::new();
    for fixture in fixtures {
        let set = fixture.parent().unwrap().file_name().unwrap().to_string_lossy();
        if set != "ccn-glm-pilot" && set != "tomme-oneill" {
            continue;
        }
        let stem = fixture.file_stem().unwrap().to_string_lossy();
        if !["concept-1", "concept-2", "concept-3", "concept-4"].contains(&stem.as_ref()) {
            continue;
        }
        let label = format!("{set}-{stem}");
        let a = read_archive(&std::fs::read(&fixture).unwrap()).unwrap();
        let resolver = |f: &str, w: u16| font_from_dir(&fonts, f, w);
        let mut s = Session::new();
        let report = import_into(&mut s, &a, &ImportOptions { font_resolver: Some(&resolver) }).unwrap();
        let doc = &s.active().unwrap().doc;
        let export = photocraft_io::export(doc, "png", &Default::default()).unwrap();
        std::fs::write(evidence.join(format!("{label}.photocraft.png")), &export.bytes).unwrap();
        if label == "ccn-glm-pilot-concept-1" {
            let psd = photocraft_io::export(doc, "psd", &Default::default()).unwrap();
            std::fs::write(evidence.join("round-trip.psd"), psd.bytes).unwrap();
        }
        let ours = image::load_from_memory(&export.bytes).unwrap().to_rgba8();
        let reference = image::open(fixture.with_extension("png")).unwrap().to_rgba8();
        assert_eq!(ours.dimensions(), reference.dimensions());
        let (w, h) = ours.dimensions();
        let mut side = image::RgbaImage::new(w * 3, h);
        let mut total = 0u64;
        let mut gt32 = 0u64;
        let mut gt64 = 0u64;
        for y in 0..h {
            for x in 0..w {
                let p = ours.get_pixel(x, y);
                let r = reference.get_pixel(x, y);
                let mut diff = [0u8; 4];
                let mut max = 0;
                for c in 0..4 {
                    let d = p[c].abs_diff(r[c]);
                    total += u64::from(d);
                    max = max.max(d);
                    diff[c] = d.saturating_mul(4);
                }
                diff[3] = 255;
                gt32 += u64::from(max > 32);
                gt64 += u64::from(max > 64);
                side.put_pixel(x, y, *r);
                side.put_pixel(x + w, y, *p);
                side.put_pixel(x + w * 2, y, image::Rgba(diff));
            }
        }
        side.save(evidence.join(format!("{label}.side-by-side.png"))).unwrap();
        let pixels = f64::from(w) * f64::from(h);
        let row = json!({"fixture":label,"mean_abs_diff":total as f64/(pixels*4.),"pct_pixels_maxdiff_gt_32":gt32 as f64*100./pixels,"pct_pixels_maxdiff_gt_64":gt64 as f64*100./pixels,"warnings":report.warnings});
        eprintln!("{row}");
        metrics.push(row);
    }
    assert_eq!(metrics.len(), 7, "must compare all seven authorized fixtures");
    std::fs::write(evidence.join("metrics.json"), serde_json::to_vec_pretty(&metrics).unwrap()).unwrap();
}

#[test]
fn smart_image_cover_mask_and_properties() {
    let image = image::RgbaImage::from_pixel(20, 10, image::Rgba([10, 100, 200, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let bytes = bytes.into_inner();
    let m = json!({"format":"frameforge-project","formatVersion":1,"project":{"layers":[{"id":"image","name":"Picture","type":"image","assetRef":"asset-0001","x":10,"y":10,"w":20,"h":20,"fit":"cover","rotation":20,"visible":false,"locked":true}]},"assets":[{"ref":"asset-0001","path":"assets/asset-0001","size":bytes.len()}]});
    let a = read_archive(&zip(&[("manifest.json", serde_json::to_vec(&m).unwrap(), true), ("assets/asset-0001", bytes, false)])).unwrap();
    let mut s = Session::new();
    let r = import_into(&mut s, &a, &ImportOptions::default()).unwrap();
    let layer = s.active().unwrap().doc.layers.last().unwrap();
    assert_eq!(layer.id.0, r.layers["image"]);
    assert_eq!(layer.content.kind_name(), "Smart Object");
    assert!(layer.vector_mask.is_some());
    assert!(layer.locks.all);
    assert!(!layer.visible);
    assert_eq!(layer.name, "Picture");
}
#[test]
fn text_shrinks_without_dropping_authored_lines() {
    let mut l = text("fitted");
    l["text"] = json!("A VERY LONG LINE\nSECOND LINE");
    l["size"] = json!(110);
    l["w"] = json!(15);
    l["h"] = json!(10);
    let mut s = Session::new();
    let r = import_into(&mut s, &archive(json!([l])), &ImportOptions::default()).unwrap();
    let info = s.execute("type.info", json!({"layer":r.layers["fitted"]})).unwrap();
    let lines = info["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines.last().unwrap()["end"], 28);
    assert!(info["runs"][0]["style"]["size_pt"].as_f64().unwrap() <= 110.);
}

#[test]
fn aligned_text_uses_natural_width_and_stays_editable() {
    let mut l = text("aligned");
    l["text"] = json!("HELLO WORLD\nHi");
    l["textAlign"] = json!("center");
    let mut s = Session::new();
    let r = import_into(&mut s, &archive(json!([l])), &ImportOptions::default()).unwrap();
    let info = s.execute("type.info", json!({"layer":r.layers["aligned"]})).unwrap();
    let lines = info["lines"].as_array().unwrap();
    assert!(lines[1]["x0"].as_f64().unwrap() > lines[0]["x0"].as_f64().unwrap());
    assert!(lines[0]["x0"].as_f64().unwrap() < 1.);
    s.execute("type.edit", json!({"layer":r.layers["aligned"],"text":"Edited"})).unwrap();
    assert_eq!(s.execute("type.info", json!({"layer":r.layers["aligned"]})).unwrap()["text"], "Edited");
}

#[test]
fn total_expansion_limit() {
    let mut entries = vec![("manifest.json", manifest(json!([])), true)];
    for name in ["assets/asset-0001", "assets/asset-0002", "assets/asset-0003", "assets/asset-0004"] {
        entries.push((name, vec![0; 32 * 1024 * 1024], true));
    }
    assert!(read_archive(&zip(&entries)).unwrap_err().contains("total"));
}
#[test]
fn unsupported_compression() {
    let mut b = zip(&[("manifest.json", manifest(json!([])), false)]);
    let cd = b.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    b[cd + 10..cd + 12].copy_from_slice(&12u16.to_le_bytes());
    assert!(read_archive(&b).unwrap_err().contains("compression"));
}

#[test]
fn import_preserves_but_ignores_type_tool_defaults() {
    let mut s = Session::new();
    let (mut cs, mut ps) = s.type_defaults.clone().unwrap_or_default();
    cs.size_pt = 100.;
    cs.faux_bold = true;
    ps.start_indent_pt = 50.;
    s.type_defaults = Some((cs, ps));
    let saved = s.type_defaults.clone();
    let r = import_into(&mut s, &archive(json!([text("clean")])), &ImportOptions::default()).unwrap();
    assert_eq!(s.type_defaults, saved);
    let info = s.execute("type.info", json!({"layer":r.layers["clean"]})).unwrap();
    assert_eq!(info["runs"][0]["style"]["faux_bold"], false);
    assert!(info["lines"][0]["x0"].as_f64().unwrap() < 1.);
}
#[test]
fn css_font_weight_mapping() {
    assert_eq!(convert::text_style(&json!({"font":"Montserrat","fontWeight":"bold"})).unwrap()["weight"], 900.);
    assert_eq!(convert::text_style(&json!({"font":"Montserrat","fontWeight":"400"})).unwrap()["weight"], 400.);
}
