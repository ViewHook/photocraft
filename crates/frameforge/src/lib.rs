//! Import editable FrameForge projects. All archives stay bounded and in memory.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
mod archive;
pub mod convert;
pub mod fonts;
pub use archive::read_archive;
use convert::{number, string};
use photocraft_engine::Session;
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub type Result<T> = std::result::Result<T, String>;
#[derive(Debug, Clone)]
pub struct Archive {
    pub project: Value,
    pub assets: BTreeMap<String, Vec<u8>>,
}
pub type FontResolver<'a> = dyn Fn(&str, u16) -> Option<Vec<u8>> + 'a;
/// FrameForge layout bookkeeping on layers. None of it changes how FrameForge renders the layer,
/// so it imports without a warning; unsupported keys that do change rendering still warn. Sources
/// in FrameForge (youtube-thumbnail-app):
/// - `src/generated-assets.js` `freshGeneratedLayer`, which builds every image layer the native
///   materialize endpoint returns: `sourceWidth`/`sourceHeight` (fallbacks for the decoded image
///   size), `hasAlpha`, `backgroundRemoved`, `generationModel`, `busyZones` (layout audits), and
///   `maskBounds` (placement input only together with `maskAppliedToSource`, which still warns);
/// - `src/learning/channel-memory.js` `applyGuidanceToLayers`: `evidence` (channel-rule provenance);
/// - `src/archetypes/compiler.js` `imageLayer`: `archetypeRole`, `objectBounds` (as `maskBounds`),
///   `primaryFocal`, `gestureDirection`, `gazeDirection`, `highlightRegion`, `modificationPolicy`,
///   `allowRectangularSourcePatch`;
/// - `src/composition/compose-thumbnail.js`: `groupId`.
pub const METADATA_KEYS: [&str; 17] = [
    "sourceWidth",
    "sourceHeight",
    "hasAlpha",
    "backgroundRemoved",
    "maskBounds",
    "busyZones",
    "generationModel",
    "evidence",
    "archetypeRole",
    "objectBounds",
    "primaryFocal",
    "gestureDirection",
    "gazeDirection",
    "highlightRegion",
    "modificationPolicy",
    "allowRectangularSourcePatch",
    "groupId",
];
#[derive(Default)]
pub struct ImportOptions<'a> {
    /// Supply font bytes before type creation. None uses installed/bundled faces.
    pub font_resolver: Option<&'a FontResolver<'a>>,
}
#[derive(Debug, Default)]
pub struct ImportReport {
    pub document: usize,
    pub layers: BTreeMap<String, u64>,
    pub warnings: Vec<String>,
    pub missing_fonts: Vec<String>,
}
fn exec(s: &mut Session, id: &str, p: Value) -> Result<Value> {
    s.execute(id, p).map_err(|e| e.to_string())
}
fn layer_id(v: &Value) -> Result<u64> {
    v.get("layer").and_then(Value::as_u64).ok_or_else(|| "command returned no layer".into())
}
fn quad([x, y, w, h]: [f64; 4], degrees: f64) -> [[f64; 2]; 4] {
    let (sn, cs) = degrees.to_radians().sin_cos();
    [[0., 0.], [w, 0.], [w, h], [0., h]].map(|[a, b]| [x + a * cs - b * sn, y + a * sn + b * cs])
}
/// Failed imports remove their partially created document.
pub fn import_into(s: &mut Session, a: &Archive, o: &ImportOptions<'_>) -> Result<ImportReport> {
    let original = s.documents().len();
    // Import appearance is independent of the user's saved type-tool defaults.
    let defaults = s.type_defaults.take();
    let r = import(s, a, o);
    s.type_defaults = defaults;
    if r.is_err() && s.documents().len() > original {
        s.close(original);
    }
    r
}
fn import(s: &mut Session, a: &Archive, o: &ImportOptions<'_>) -> Result<ImportReport> {
    let layers = a.project.get("layers").and_then(Value::as_array).ok_or("missing layers")?;
    if layers.len() > 500 {
        return Err("layer count exceeds 500".into());
    }
    // Current FrameForge v1 fixes its rendering canvas at 1280 × 720.
    let canvas = a.project.get("canvas").unwrap_or(&Value::Null);
    let width = number(canvas, "width", 1280.);
    let height = number(canvas, "height", 720.);
    if !(1. ..=16384.).contains(&width) || !(1. ..=16384.).contains(&height) || width * height > 64_000_000. {
        return Err("invalid canvas size".into());
    }
    let d = exec(
        s,
        "file.new",
        json!({"width":width,"height":height,"mode":"rgb","depth":8,"resolution":72,"background":"#111111","name":string(&a.project,"name","FrameForge")}),
    )?;
    let mut report = ImportReport { document: d.get("document").and_then(Value::as_u64).ok_or("no document index")? as usize, ..Default::default() };
    let mut resolved = BTreeMap::new();
    for l in layers.iter().rev() {
        let name = string(l, "name", string(l, "id", "unnamed"));
        let kind = string(l, "type", "");
        if !matches!(kind, "text" | "image") {
            report.warnings.push(format!("Layer '{name}': unsupported {kind} layer skipped"));
            continue;
        }
        let common = ["id", "name", "type", "x", "y", "w", "h", "rotation", "visible", "locked", "opacity", "role", "semanticRole", "generated", "assetKind"];
        let text_keys = [
            "text",
            "font",
            "fontWeight",
            "bold",
            "italic",
            "size",
            "color",
            "textAlign",
            "stroke",
            "strokeColor",
            "strokeWidth",
            "shadow",
            "scaleX",
            "scaleY",
            "lineHeight",
            "charSpacing",
        ];
        let image_keys = [
            "assetRef",
            "fit",
            "preserveAspectRatio",
            "contentScale",
            "contentAnchorX",
            "contentAnchorY",
            "contentOffsetX",
            "contentOffsetY",
            "position",
            "clipToSlot",
            "allowBleed",
        ];
        if let Some(keys) = l.as_object() {
            for key in keys.keys() {
                if !common.contains(&key.as_str())
                    && !METADATA_KEYS.contains(&key.as_str())
                    && !(if kind == "text" { text_keys.as_slice() } else { image_keys.as_slice() }).contains(&key.as_str())
                {
                    report.warnings.push(format!("Layer '{name}': unsupported feature '{key}' approximated or omitted"));
                }
            }
        }
        let slot = convert::pixel_rect(l, [width, height])?;
        let id = if kind == "image" {
            let reference = string(l, "assetRef", "");
            let bytes = a.assets.get(reference).ok_or_else(|| format!("Layer '{name}': missing asset '{reference}'"))?;
            let placed = photocraft_engine::file_cmds::place_bytes(s, name, bytes.clone(), None, &json!({"scale":100,"fit":false,"center":[0,0]}))
                .map_err(|e| e.to_string())?;
            let id = layer_id(&placed)?;
            let b = placed.get("bounds").and_then(Value::as_array).ok_or("no placed bounds")?;
            let n = |i: usize| b.get(i).and_then(Value::as_f64).ok_or("invalid placed bounds");
            let source = [n(2)? - n(0)?, n(3)? - n(1)?];
            let r = convert::image_rect(l, slot, source)?;
            exec(s, "edit.transform", json!({"layer":id,"rect":b,"quad":quad(r,number(l,"rotation",0.))}))?;
            if l.get("clipToSlot") == Some(&Value::Bool(true))
                || (matches!(string(l, "fit", "contain"), "cover" | "full-bleed") && l.get("allowBleed") != Some(&Value::Bool(true)))
            {
                let [x, y, w, h] = slot;
                exec(s, "layer.vectorMask.add", json!({"layer":id,"path":{"subpaths":[{"closed":true,"knots":[[x,y],[x+w,y],[x+w,y+h],[x,y+h]]}]}}))?;
            }
            if l.get("shadow").is_some() {
                report.warnings.push(format!("Layer '{name}': image shadow omitted"));
            }
            id
        } else {
            import_text(s, l, slot, o, &mut report, &mut resolved)?
        };
        exec(
            s,
            "layer.setProps",
            json!({"layer":id,"name":name,"visible":l.get("visible").and_then(Value::as_bool).unwrap_or(true),"locked":l.get("locked").and_then(Value::as_bool).unwrap_or(false),"opacity":number(l,"opacity",1.)}),
        )?;
        let ff_id = string(l, "id", "");
        if report.layers.insert(ff_id.to_string(), id).is_some() {
            return Err(format!("duplicate layer id '{ff_id}'"));
        }
    }
    Ok(report)
}
fn import_text(
    s: &mut Session,
    l: &Value,
    [x, y, w, h]: [f64; 4],
    o: &ImportOptions<'_>,
    report: &mut ImportReport,
    resolved: &mut BTreeMap<(String, u16), Option<String>>,
) -> Result<u64> {
    let name = string(l, "name", "text");
    let text = string(l, "text", "").replace("\r\n", "\n").replace('\r', "\n");
    let mut style = convert::text_style(l)?;
    let family = string(&style, "font", "Inter").to_string();
    let weight = number(&style, "weight", 400.) as u16;
    let key = (family.clone(), weight);
    let actual = if let Some(actual) = resolved.get(&key) {
        actual.clone()
    } else {
        let bytes = o.font_resolver.and_then(|f| f(&family, weight));
        let mut engine = photocraft_text::shared().lock().unwrap_or_else(|e| e.into_inner());
        let added = bytes.map(|bytes| engine.fonts.register_font_data(bytes)).unwrap_or_default();
        // Converted variable-font instances may have a family such as "Montserrat
        // Thin Black". The resolver's bytes define the requested face; use the
        // family reported by registration rather than silently falling back.
        let actual = added
            .iter()
            .find(|f| f.eq_ignore_ascii_case(&family))
            .or_else(|| added.first())
            .cloned()
            .or_else(|| engine.fonts.families().into_iter().find(|f| f.eq_ignore_ascii_case(&family)));
        resolved.insert(key, actual.clone());
        actual
    };
    let actual = match actual {
        Some(actual) => actual,
        None => {
            if !report.missing_fonts.contains(&family) {
                report.missing_fonts.push(family.clone());
            }
            report.warnings.push(format!("Layer '{name}': missing font '{family}' weight {weight}; using PhotoCraft fallback"));
            "Inter".to_string()
        }
    };
    style["font"] = json!(actual);
    let authored = number(&style, "size", 60.);
    let line_height = number(l, "lineHeight", 1.);
    if !(0.1..=10.).contains(&line_height) {
        return Err("invalid line height".into());
    }
    style["box"] = json!([0, 0, 32768, 32768]);
    style["name"] = json!(name);
    style["text"] = json!(text);
    // Measure explicit lines without wrapping before fitting. FrameForge IText never wraps.
    style["align"] = json!("left");
    let id = layer_id(&exec(s, "type.create", style)?)?;
    let info = exec(s, "type.info", json!({"layer":id}))?;
    let lines = info.get("lines").and_then(Value::as_array).ok_or("missing type lines")?;
    let max_width = lines.iter().map(|ln| number(ln, "x1", 0.) - number(ln, "x0", 0.)).fold(1., f64::max);
    let natural_height = authored * 1.13 * (1. + text.split('\n').count().saturating_sub(1) as f64 * line_height);
    let fit = 1f64.min(w / max_width).min(h / natural_height);
    let mut size = (authored * fit).max(0.1);
    // Use a paragraph box, but retain authored newlines and avoid silent truncation.
    exec(s, "type.edit", json!({"layer":id,"box":[0,0,(max_width*fit+0.01).min(w).max(1.),h]}))?;
    for _ in 0..120 {
        let final_info = exec(s, "type.setStyle", json!({"layer":id,"size":size,"leading":size*1.13*line_height,"align":string(l,"textAlign","left")}))?;
        let ls = final_info.get("lines").and_then(Value::as_array).ok_or("missing fitted lines")?;
        let full = ls.last().is_some_and(|ln| ln.get("end").and_then(Value::as_u64) == Some(text.chars().count() as u64));
        if text.is_empty()
            || (full
                && ls.len() == text.split('\n').count()
                && ls
                    .iter()
                    .all(|ln| number(ln, "x1", 0.) - number(ln, "x0", 0.) <= w + 0.01 && number(ln, "baseline", 0.) + number(ln, "descent", 0.) <= h + 0.5))
        {
            break;
        }
        size *= 0.98;
        if size < 0.1 {
            return Err(format!("Layer '{name}': text cannot fit"));
        }
    }
    // IText aligns shorter lines inside the longest authored line, not inside
    // the target slot. Keep that natural width in the editable paragraph box.
    let final_info = exec(s, "type.edit", json!({"layer":id,"box":[0,0,(max_width*size/authored+0.01).min(w).max(1.),h]}))?;
    let lines = final_info.get("lines").and_then(Value::as_array).ok_or("missing fitted lines")?;
    if !text.is_empty() && lines.last().and_then(|ln| ln.get("end")).and_then(Value::as_u64) != Some(text.chars().count() as u64) {
        return Err(format!("Layer '{name}': text fit did not converge"));
    }
    let baseline = lines.first().map_or(0., |ln| number(ln, "baseline", 0.));
    let fitted_height = size * 1.13 * (1. + text.split('\n').count().saturating_sub(1) as f64 * line_height);
    let top = y + (h - fitted_height).max(0.) / 2.;
    // Fabric baseline: 1.13 em line height minus 0.25 em descent.
    let baseline_offset = size * 0.88 - baseline;
    // Fabric's left/top refer to the stroke-inclusive object bounds. Its fill
    // origin is inset by half the centred stroke (the outside width).
    let stroke_effect = convert::stroke(l, size)?;
    let stroke_inset = stroke_effect.as_ref().and_then(|st| st.get("params")).map_or(0., |p| number(p, "size", 0.));
    let sx = number(l, "scaleX", 1.);
    let sy = number(l, "scaleY", 1.);
    if sx.abs() < 0.001 || sy.abs() < 0.001 || sx.abs() > 100. || sy.abs() > 100. {
        return Err("invalid type scale".into());
    }
    let (sn, cs) = number(l, "rotation", 0.).to_radians().sin_cos();
    exec(
        s,
        "edit.transform",
        json!({"layer":id,"matrix":[cs*sx,sn*sx,-sn*sy,cs*sy,x+cs*sx*stroke_inset-sn*sy*(baseline_offset+stroke_inset),top+sn*sx*stroke_inset+cs*sy*(baseline_offset+stroke_inset)]}),
    )?;
    let mut effects = Vec::new();
    if let Some(stroke) = stroke_effect {
        effects.push(stroke);
    }
    if let Some(shadow) = convert::shadow(l.get("shadow").unwrap_or(&Value::Null))? {
        effects.push(shadow);
    }
    if !effects.is_empty() {
        exec(s, "layer.layerStyle.replace", json!({"layer":id,"effects":effects}))?;
    }
    Ok(id)
}
/// CLI/test resolver for the externally supplied FrameForge font conversions.
#[cfg(not(target_arch = "wasm32"))]
pub fn font_from_dir(dir: &std::path::Path, family: &str, weight: u16) -> Option<Vec<u8>> {
    let slug = family.to_ascii_lowercase().replace(' ', "-");
    if !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    std::fs::read(dir.join(format!("{slug}-{weight}.ttf"))).ok()
}
#[doc(hidden)]
pub mod testing;
#[cfg(test)]
mod tests;
