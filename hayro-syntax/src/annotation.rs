//! Appearances for annotations that carry none (no `AP` entry).
//!
//! Shared by hayro-interpret (which draws them) and hayro-write (which flattens them into a copied page), so a page
//! looks the same rendered and copied.
//!
//! PDF 2.0 requires an appearance stream for every visible annotation, but many writers leave it out for
//! the markup annotations (lines, shapes, ink, text highlights) and let the viewer draw them. Acrobat, pdf.js
//! and Aspose do. This module writes a small content stream for each such annotation, following pdf.js's
//! rules (`src/core/annotation.js`): the same default colours, widths and shapes, so a file draws the same
//! here as in the viewers people check it with. Text-bearing annotations (`FreeText`, the `Text` note icon)
//! are not drawn: they need fonts and icons of the viewer's own.

use crate::object::dict::keys::{
    BORDER, BS, C, CA, IC, INKLIST, L, QUADPOINTS, RECT, SUBTYPE, VERTICES, W,
};
use crate::object::{Array, Dict, Name, Rect};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

/// A synthesised appearance: content stream operators in default user space, plus how to composite them.
#[derive(Debug, Clone)]
pub struct Synthesised {
    /// The operators, in default user space (no resources needed).
    pub ops: String,
    /// The annotation's constant opacity (`CA`), applied to the appearance as a whole (a transparency group).
    pub opacity: f32,
    /// Text highlights multiply with what is under them.
    pub multiply: bool,
}

enum Colour {
    /// The key is not there: the subtype's default applies.
    Absent,
    /// An empty array: transparent, nothing is painted.
    Transparent,
    Components(Vec<f32>),
}

fn colour(annot: &Dict<'_>, key: &[u8]) -> Colour {
    match annot.get::<Array<'_>>(key) {
        None => Colour::Absent,
        Some(array) => {
            let components: Vec<f32> = array.iter::<f32>().collect();
            match components.len() {
                1 | 3 | 4 => Colour::Components(components),
                _ => Colour::Transparent,
            }
        }
    }
}

/// The colour operator for components, `stroke` picking `RG` over `rg`.
fn colour_op(components: &[f32], stroke: bool) -> String {
    let mut out = String::new();
    for c in components {
        let _ = write!(out, "{} ", num(c.clamp(0.0, 1.0)));
    }
    out.push_str(match (components.len(), stroke) {
        (1, true) => "G",
        (1, false) => "g",
        (3, true) => "RG",
        (3, false) => "rg",
        (_, true) => "K",
        (_, false) => "k",
    });
    out
}

fn num(v: f32) -> String {
    let mut s = format!("{v:.4}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" { "0".to_string() } else { s }
}

/// The border width: `BS` `W`, else the third number of `Border`, else 1 (the specification's default).
fn border_width(annot: &Dict<'_>) -> f32 {
    if let Some(w) = annot.get::<Dict<'_>>(BS).and_then(|bs| bs.get::<f32>(W)) {
        return w.max(0.0);
    }
    if let Some(border) = annot.get::<Array<'_>>(BORDER) {
        let values: Vec<f32> = border.iter::<f32>().collect();
        if values.len() >= 3 {
            return values[2].max(0.0);
        }
    }
    1.0
}

fn numbers(array: &Array<'_>) -> Vec<f32> {
    array.iter::<f32>().filter(|v| v.is_finite()).collect()
}

fn polyline(out: &mut String, points: &[f32]) -> bool {
    if points.len() < 4 {
        return false;
    }
    for (i, p) in points.as_chunks::<2>().0.iter().enumerate() {
        let _ = write!(
            out,
            "{} {} {} ",
            num(p[0]),
            num(p[1]),
            if i == 0 { "m" } else { "l" }
        );
    }
    true
}

/// Synthesise the appearance of an annotation without `AP`, or `None` when it draws nothing.
pub fn synthesise(annot: &Dict<'_>) -> Option<Synthesised> {
    let subtype = annot.get::<Name<'_>>(SUBTYPE)?;
    let opacity = annot.get::<f32>(CA).unwrap_or(1.0).clamp(0.0, 1.0);
    if opacity == 0.0 {
        return None;
    }
    let stroke = |default: &[f32]| -> Option<String> {
        match colour(annot, C) {
            Colour::Absent => Some(colour_op(default, true)),
            Colour::Transparent => None,
            Colour::Components(c) => Some(colour_op(&c, true)),
        }
    };
    let black = [0.0_f32, 0.0, 0.0];
    let mut ops = String::new();
    let mut multiply = false;
    match subtype.as_ref() {
        b"Line" => {
            let l = numbers(&annot.get::<Array<'_>>(L)?);
            if l.len() < 4 {
                return None;
            }
            let colour = stroke(&black)?;
            let width = match border_width(annot) {
                0.0 => 1.0,
                w => w,
            };
            let _ = write!(ops, "{colour} {} w ", num(width));
            polyline(&mut ops, &l[..4]);
            ops.push('S');
        }
        b"PolyLine" | b"Polygon" => {
            let vertices = numbers(&annot.get::<Array<'_>>(VERTICES)?);
            let closed = subtype.as_ref() == b"Polygon";
            let colour = stroke(&black)?;
            let width = match border_width(annot) {
                0.0 => 1.0,
                w => w,
            };
            let fill = match (closed, colour_or_none(annot, IC)) {
                (true, Some(fill)) => Some(fill),
                _ => None,
            };
            let _ = write!(ops, "{colour} {} w ", num(width));
            if let Some(fill) = &fill {
                let _ = write!(ops, "{fill} ");
            }
            if !polyline(&mut ops, &vertices) {
                return None;
            }
            ops.push_str(match (closed, fill.is_some()) {
                (true, true) => "b",
                (true, false) => "s",
                _ => "S",
            });
        }
        b"Ink" => {
            let ink = annot.get::<Array<'_>>(INKLIST)?;
            let colour = stroke(&black)?;
            let width = match border_width(annot) {
                0.0 => 1.0,
                w => w,
            };
            let _ = write!(ops, "{colour} {} w 1 J 1 j ", num(width));
            let mut any = false;
            for stroke_points in ink.iter::<Array<'_>>() {
                let points = numbers(&stroke_points);
                if polyline(&mut ops, &points) {
                    any = true;
                } else if points.len() == 2 {
                    // A single point: a dot, as a zero-length round-capped line.
                    let _ = write!(ops, "{0} {1} m {0} {1} l ", num(points[0]), num(points[1]));
                    any = true;
                }
            }
            if !any {
                return None;
            }
            ops.push('S');
        }
        b"Square" | b"Circle" => {
            let rect = annot.get::<Rect>(RECT)?;
            let width = border_width(annot);
            let fill = colour_or_none(annot, IC);
            // pdf.js: a zero-width border with no fill would be a hairline nobody asked for.
            if width == 0.0 && fill.is_none() {
                return None;
            }
            let colour = if width > 0.0 { stroke(&black) } else { None };
            if colour.is_none() && fill.is_none() {
                return None;
            }
            let half = (width / 2.0) as f64;
            let (x0, y0) = (rect.x0.min(rect.x1) + half, rect.y0.min(rect.y1) + half);
            let (rw, rh) = (
                rect.x0.max(rect.x1) - rect.x0.min(rect.x1),
                rect.y0.max(rect.y1) - rect.y0.min(rect.y1),
            );
            let (w, h) = ((rw - 2.0 * half).max(0.0), (rh - 2.0 * half).max(0.0));
            if let Some(colour) = &colour {
                let _ = write!(ops, "{colour} {} w ", num(width));
            }
            if let Some(fill) = &fill {
                let _ = write!(ops, "{fill} ");
            }
            if subtype.as_ref() == b"Square" {
                let _ = write!(
                    ops,
                    "{} {} {} {} re ",
                    num(x0 as f32),
                    num(y0 as f32),
                    num(w as f32),
                    num(h as f32)
                );
            } else {
                // Four Bézier quarters, as pdf.js writes them.
                const K: f64 = 0.552_284_749_8;
                let (rx, ry) = (w / 2.0, h / 2.0);
                let (cx, cy) = (x0 + rx, y0 + ry);
                let (ox, oy) = (rx * K, ry * K);
                let p = |v: f64| num(v as f32);
                let _ = write!(
                    ops,
                    "{} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c h ",
                    p(cx),
                    p(cy + ry),
                    p(cx + ox),
                    p(cy + ry),
                    p(cx + rx),
                    p(cy + oy),
                    p(cx + rx),
                    p(cy),
                    p(cx + rx),
                    p(cy - oy),
                    p(cx + ox),
                    p(cy - ry),
                    p(cx),
                    p(cy - ry),
                    p(cx - ox),
                    p(cy - ry),
                    p(cx - rx),
                    p(cy - oy),
                    p(cx - rx),
                    p(cy),
                    p(cx - rx),
                    p(cy + oy),
                    p(cx - ox),
                    p(cy + ry),
                    p(cx),
                    p(cy + ry),
                );
            }
            ops.push_str(match (colour.is_some(), fill.is_some()) {
                (true, true) => "B",
                (true, false) => "S",
                _ => "f",
            });
        }
        b"Highlight" | b"Underline" | b"StrikeOut" | b"Squiggly" => {
            let quads = numbers(&annot.get::<Array<'_>>(QUADPOINTS)?);
            if quads.len() < 8 {
                return None;
            }
            let highlight = subtype.as_ref() == b"Highlight";
            let colour = match (colour(annot, C), highlight) {
                (Colour::Transparent, _) => return None,
                (Colour::Components(c), h) => colour_op(&c, !h),
                (Colour::Absent, true) => colour_op(&[1.0, 1.0, 0.0], false),
                (Colour::Absent, false) => colour_op(&black, true),
            };
            let _ = write!(ops, "{colour} ");
            match subtype.as_ref() {
                b"Underline" => ops.push_str("[] 0 d 0.571 w "),
                b"StrikeOut" | b"Squiggly" => ops.push_str("[] 0 d 1 w "),
                _ => {}
            }
            for q in quads.as_chunks::<8>().0 {
                // Points in the order Acrobat writes them: upper left, upper right, lower left, lower right.
                let (p0, p1, p2, p3) = ((q[0], q[1]), (q[2], q[3]), (q[4], q[5]), (q[6], q[7]));
                match subtype.as_ref() {
                    b"Highlight" => {
                        let _ = write!(
                            ops,
                            "{} {} m {} {} l {} {} l {} {} l f ",
                            num(p0.0),
                            num(p0.1),
                            num(p1.0),
                            num(p1.1),
                            num(p3.0),
                            num(p3.1),
                            num(p2.0),
                            num(p2.1)
                        );
                    }
                    b"Underline" => {
                        let _ = write!(
                            ops,
                            "{} {} m {} {} l S ",
                            num(p2.0),
                            num(p2.1 + 1.3),
                            num(p3.0),
                            num(p3.1 + 1.3)
                        );
                    }
                    b"StrikeOut" => {
                        let _ = write!(
                            ops,
                            "{} {} m {} {} l S ",
                            num((p0.0 + p2.0) / 2.0),
                            num((p0.1 + p2.1) / 2.0),
                            num((p1.0 + p3.0) / 2.0),
                            num((p1.1 + p3.1) / 2.0)
                        );
                    }
                    _ => {
                        let h = (p0.1 - p2.1) / 6.0;
                        let (mut x, y, end) = (p2.0, p2.1, p3.0);
                        if !(h.is_finite() && x.is_finite() && end.is_finite())
                            || end - x > 100_000.0
                        {
                            continue;
                        }
                        let mut shift = h;
                        let _ = write!(ops, "{} {} m ", num(x), num(y + shift));
                        while x < end {
                            x += 2.0;
                            shift = if shift == 0.0 { h } else { 0.0 };
                            let _ = write!(ops, "{} {} l ", num(x), num(y + shift));
                        }
                        ops.push_str("S ");
                    }
                }
            }
            multiply = highlight;
        }
        _ => return None,
    }
    Some(Synthesised {
        ops,
        opacity,
        multiply,
    })
}

/// A fill colour operator, or `None` when the key is absent or transparent.
fn colour_or_none(annot: &Dict<'_>, key: &[u8]) -> Option<String> {
    match colour(annot, key) {
        Colour::Components(c) => Some(colour_op(&c, false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::num;

    #[test]
    fn numbers_are_short() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(0.5), "0.5");
        assert_eq!(num(-0.00001), "0");
        assert_eq!(num(52.91), "52.91");
    }
}
