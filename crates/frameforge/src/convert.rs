//! Renderer conversions, independent of a session.
use crate::Result;
use serde_json::{Value, json};
pub fn number(v: &Value, key: &str, default: f64) -> f64 {
    v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(default)
}
pub fn string<'a>(v: &'a Value, key: &str, default: &'a str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or(default)
}
pub fn pixel_rect(layer: &Value, canvas: [f64; 2]) -> Result<[f64; 4]> {
    let r = [
        number(layer, "x", 0.) * canvas[0] / 100.,
        number(layer, "y", 0.) * canvas[1] / 100.,
        number(layer, "w", 0.) * canvas[0] / 100.,
        number(layer, "h", 0.) * canvas[1] / 100.,
    ];
    if r.iter().any(|v| !v.is_finite() || v.abs() > 32768.) || r[2] <= 0. || r[3] <= 0. {
        return Err("invalid or excessive layer geometry".into());
    }
    Ok(r)
}
pub fn image_rect(layer: &Value, slot: [f64; 4], source: [f64; 2]) -> Result<[f64; 4]> {
    if source.iter().any(|n| *n <= 0. || !n.is_finite()) {
        return Err("invalid image dimensions".into());
    }
    let [x, y, w, h] = slot;
    let scale =
        if matches!(string(layer, "fit", "contain"), "cover" | "full-bleed") { (w / source[0]).max(h / source[1]) } else { (w / source[0]).min(h / source[1]) };
    let scale = (scale * number(layer, "contentScale", 1.).max(0.01)).max(0.000001);
    let iw = source[0] * scale;
    let ih = source[1] * scale;
    let position = string(layer, "position", "");
    let ax = string(
        layer,
        "contentAnchorX",
        if position.contains("left") {
            "left"
        } else if position.contains("right") {
            "right"
        } else {
            "center"
        },
    );
    let ay = string(
        layer,
        "contentAnchorY",
        if position.contains("top") {
            "top"
        } else if position.contains("bottom") {
            "bottom"
        } else {
            "center"
        },
    );
    let left =
        x + match ax {
            "left" => 0.,
            "right" => w - iw,
            _ => (w - iw) / 2.,
        } + number(layer, "contentOffsetX", 0.) * w;
    let top =
        y + match ay {
            "top" => 0.,
            "bottom" => h - ih,
            _ => (h - ih) / 2.,
        } + number(layer, "contentOffsetY", 0.) * h;
    let r = [left, top, iw, ih];
    if r.iter().any(|n| !n.is_finite() || n.abs() > 32768.) {
        return Err("excessive image transform".into());
    }
    Ok(r)
}
pub fn color(s: &str) -> Result<[f64; 4]> {
    if let Some(h) = s.strip_prefix('#') {
        if !h.is_ascii() {
            return Err("invalid color".into());
        }
        let channels: Vec<u8> = match h.len() {
            3 | 4 => h.bytes().map(|c| (c as char).to_digit(16).map(|n| n as u8 * 17).ok_or("invalid hex color")).collect::<std::result::Result<_, _>>()?,
            6 | 8 => h
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| std::str::from_utf8(p).ok().and_then(|s| u8::from_str_radix(s, 16).ok()).ok_or("invalid hex color"))
                .collect::<std::result::Result<_, _>>()?,
            _ => return Err("invalid hex color".into()),
        };
        return Ok([
            f64::from(*channels.first().ok_or("invalid color")?) / 255.,
            f64::from(*channels.get(1).ok_or("invalid color")?) / 255.,
            f64::from(*channels.get(2).ok_or("invalid color")?) / 255.,
            channels.get(3).map_or(1., |n| f64::from(*n) / 255.),
        ]);
    }
    let inner = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")).and_then(|s| s.strip_suffix(')')).ok_or("unsupported color syntax")?;
    let v: Vec<f64> = inner.split(',').map(|s| s.trim().parse::<f64>().map_err(|_| "invalid RGB color")).collect::<std::result::Result<_, _>>()?;
    if !(3..=4).contains(&v.len()) || v.iter().any(|n| !n.is_finite()) {
        return Err("invalid RGB color".into());
    }
    Ok([
        v.first().copied().unwrap_or(0.).clamp(0., 255.) / 255.,
        v.get(1).copied().unwrap_or(0.).clamp(0., 255.) / 255.,
        v.get(2).copied().unwrap_or(0.).clamp(0., 255.) / 255.,
        v.get(3).copied().unwrap_or(1.).clamp(0., 1.),
    ])
}
pub fn stroke(layer: &Value, size: f64) -> Result<Option<Value>> {
    if layer.get("stroke").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let c = color(string(layer, "strokeColor", "#111111"))?;
    let em = number(layer, "strokeWidth", 0.035);
    let width = (if em == 0. { 0.035 } else { em }) * size;
    if !(0. ..=250.).contains(&width) {
        return Err("invalid stroke width".into());
    }
    Ok(Some(json!({"kind":"stroke","params":{"size":width,"position":"outside","color":c,"opacity":100}})))
}
pub fn shadow(value: &Value) -> Result<Option<Value>> {
    if value.is_null() || value == &Value::Bool(false) {
        return Ok(None);
    }
    if value != &Value::Bool(true) && !value.is_object() {
        return Err("invalid shadow".into());
    }
    let c = color(string(value, "color", "rgba(0,0,0,.85)"))?;
    let x = number(value, "offsetX", 4.);
    let y = number(value, "offsetY", 5.);
    let blur = number(value, "blur", 7.);
    if x.abs() > 30000. || y.abs() > 30000. || !(0. ..=250.).contains(&blur) {
        return Err("invalid shadow geometry".into());
    }
    Ok(Some(
        json!({"kind":"dropShadow","params":{"color":[c[0],c[1],c[2],1.],"opacity":c[3]*100.,"angle":y.atan2(-x).to_degrees(),"useGlobalLight":false,"distance":x.hypot(y),"spread":0,"size":blur,"blend":"Normal"}}),
    ))
}
pub fn text_style(layer: &Value) -> Result<Value> {
    let requested = string(layer, "font", "Arial");
    let family = match requested {
        "Impact" | "Haettenschweiler" | "Franchise" => "Anton",
        "Nimbus Sans Narrow" => "Archivo Narrow",
        "Quicksand" => "Poppins",
        _ => requested,
    };
    let weight = match layer.get("fontWeight").and_then(Value::as_str) {
        Some("bold") => 700.,
        Some("normal") => 400.,
        Some(s) => s.parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(400.),
        None => number(layer, "fontWeight", if layer.get("bold").and_then(Value::as_bool) == Some(true) { 900. } else { 400. }),
    };
    let two_weights =
        ["Montserrat", "Passion One", "Poppins", "Rubik", "Nunito Sans", "Barlow", "Inter", "Work Sans", "Roboto Condensed", "Roboto Slab", "Playfair Display"];
    let single_weights = [
        "Anton",
        "Bebas Neue",
        "Oswald",
        "Fjalla One",
        "Teko",
        "Archivo Narrow",
        "Archivo Black",
        "Alfa Slab One",
        "Titan One",
        "Sigmar One",
        "Luckiest Guy",
        "Bangers",
        "Zilla Slab",
        "Bevan",
        "Pacifico",
        "Permanent Marker",
        "Caveat",
        "Abril Fatface",
    ];
    let weight = if two_weights.contains(&family) {
        if weight <= 650. { 400. } else { 900. }
    } else if single_weights.contains(&family) {
        400.
    } else {
        weight.clamp(100., 900.)
    };
    let size = number(layer, "size", 60.);
    if !(0.1..=1296.).contains(&size) {
        return Err("invalid type size".into());
    }
    Ok(
        json!({"font":family,"weight":weight,"italic":layer.get("italic").and_then(Value::as_bool).unwrap_or(false),"size":size,"color":color(string(layer,"color","#ffffff"))?,"align":string(layer,"textAlign","left"),"tracking":number(layer,"charSpacing",0.),"leading":size*1.13*number(layer,"lineHeight",1.),"kerning":"metrics","ligatures":false}),
    )
}
