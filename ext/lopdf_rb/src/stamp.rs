use lopdf::{Document, Object, ObjectId, Dictionary, Stream};
use serde::{Deserialize, Deserializer};

use crate::geometry::{get_page_dimensions_or_fallback, resolve_x, resolve_y};
use crate::metrics::text_width_pt;

/// Stamp configuration read from the JSON file produced by Ruby.
#[derive(Deserialize)]
pub(crate) struct StampConfig {
    #[serde(default)]
    stamps: Vec<StampItem>,
    #[serde(default)]
    text_blocks: Vec<TextBlockItem>,
    #[serde(default)]
    lines: Vec<LineItem>,
    #[serde(default)]
    rectangles: Vec<RectangleItem>,
}

/// The 14 standard PDF Type1 base fonts. These are guaranteed by the spec to
/// be available in every conforming viewer without embedding.
const STANDARD_BASE_FONTS: &[&str] = &[
    "Helvetica", "Helvetica-Bold", "Helvetica-Oblique", "Helvetica-BoldOblique",
    "Times-Roman", "Times-Bold", "Times-Italic", "Times-BoldItalic",
    "Courier", "Courier-Bold", "Courier-Oblique", "Courier-BoldOblique",
    "Symbol", "ZapfDingbats",
];

/// A single visible text stamp to render on every page.
///
/// TODO:: [DEFERRED] Support <barcode> element type
/// Reason: requires Rust barcode encoding crate; only used by identity certificates (not yet built)
/// Scope: 3
/// Java ref: common/src/main/java/com/amphora/common/drawing/xml/DrawingXmlService.java
/// See: AMPHTT-826
#[derive(Deserialize)]
struct StampItem {
    text: String,
    x: f64,
    y: f64,
    origin_x: String,       // "left" | "right" | "centre"
    origin_y: String,       // "top" | "bottom" | "middle"
    size: f64,
    #[serde(deserialize_with = "deserialize_rgb")]
    color: [f64; 3],        // RGB 0.0–1.0, clamped at deserialization
    font: Option<String>,   // PDF base font name; defaults to Helvetica
    align: Option<String>,          // "left" | "centre" | "right"
    vertical_align: Option<String>, // "bottom" | "top" | "middle"
    rotation: Option<f64>,          // degrees counter-clockwise
}

/// A text block that word-wraps within a bounding width.
#[derive(Deserialize)]
struct TextBlockItem {
    text: String,
    x: f64,
    y: f64,
    #[serde(default = "default_left")]
    origin_x: String,
    #[serde(default = "default_bottom")]
    origin_y: String,
    #[serde(default = "default_font_size")]
    size: f64,
    #[serde(default, deserialize_with = "deserialize_rgb")]
    color: [f64; 3],
    font: Option<String>,
    width: f64,
    line_spacing: f64,
}

/// A line element — two endpoints with independent origins.
#[derive(Deserialize)]
struct LineItem {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    #[serde(default = "default_left")]
    x1_origin: String,
    #[serde(default = "default_bottom")]
    y1_origin: String,
    #[serde(default = "default_left")]
    x2_origin: String,
    #[serde(default = "default_bottom")]
    y2_origin: String,
    #[serde(default, deserialize_with = "deserialize_rgb")]
    color: [f64; 3],
    #[serde(default = "default_thickness")]
    thickness: f64,
}

/// A rectangle element — two corners with independent origins.
#[derive(Deserialize)]
struct RectangleItem {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    #[serde(default = "default_left")]
    x1_origin: String,
    #[serde(default = "default_bottom")]
    y1_origin: String,
    #[serde(default = "default_left")]
    x2_origin: String,
    #[serde(default = "default_bottom")]
    y2_origin: String,
    #[serde(default, deserialize_with = "deserialize_rgb")]
    color: [f64; 3],
    #[serde(default = "default_thickness")]
    thickness: f64,
}

fn default_left() -> String { "left".to_string() }
fn default_bottom() -> String { "bottom".to_string() }
fn default_font_size() -> f64 { 12.0 }
fn default_thickness() -> f64 { 0.5 }

/// Clamp a colour channel into the [0.0, 1.0] range DeviceRGB operands
/// require (ISO 32000 §8.6.4.2). Out-of-range channels are corrected
/// silently, consistent with the crate's silent-correction convention
/// (unknown fonts fall back to Helvetica, unknown origins fall through to
/// defaults). The `+ 0.0` normalizes IEEE-754 negative zero to positive
/// zero — `-0.0` is inside the clamp range but would Display as "-0" in
/// the content stream. Channels are always finite here: the config arrives
/// as JSON, which cannot encode NaN/Infinity.
fn clamp_unit_channel(v: f64) -> f64 {
    v.clamp(0.0, 1.0) + 0.0
}

/// Deserialize an RGB triple, normalizing every channel via
/// `clamp_unit_channel`. Applied to each colour field on the stamp item
/// structs so the stored values are safe to interpolate into content
/// streams by construction — a future write site cannot reintroduce
/// out-of-range operands by formatting the raw field.
fn deserialize_rgb<'de, D>(deserializer: D) -> Result<[f64; 3], D::Error>
where
    D: Deserializer<'de>,
{
    let channels = <[f64; 3]>::deserialize(deserializer)?;
    Ok(channels.map(clamp_unit_channel))
}

/// Apply stamp config to every page in the document.
///
/// Iterates all pages, reads each page's dimensions from MediaBox,
/// resolves origin-relative coordinates, and appends a content stream
/// with the stamp text.
///
/// Returns `Err` when a page's font registration or content-stream append
/// fails. The first failure aborts the loop: earlier pages remain stamped,
/// and the failing page itself may already be partially mutated (fonts
/// registered in its /Resources by earlier items, orphaned font/stream
/// objects added) — callers must discard the document on error rather than
/// save it.
pub(crate) fn apply_stamp_config(doc: &mut Document, config: &StampConfig) -> Result<(), String> {
    // Collect (page number, page ID) pairs so we can mutate doc afterwards —
    // get_pages() returns a BTreeMap, so iteration is already sorted by the
    // 1-based physical page number.
    let page_ids: Vec<(u32, ObjectId)> = doc.get_pages()
        .iter()
        .map(|(&number, &id)| (number, id))
        .collect();

    // If no pages, nothing to do
    if page_ids.is_empty() {
        return Ok(());
    }

    // Read dimensions for each page before mutating
    let dimensions: Vec<(f64, f64)> = page_ids.iter()
        .map(|&(page_number, pid)| get_page_dimensions_or_fallback(doc, pid, page_number))
        .collect();

    for (&(page_number, page_id), &(page_width, page_height)) in
        page_ids.iter().zip(dimensions.iter())
    {

        // Build content stream for all stamp items on this page.
        // Render order: lines/rectangles first (background), then text on top.
        let mut content_parts: Vec<String> = Vec::new();

        for line in &config.lines {
            let abs_x1 = resolve_x(line.x1, &line.x1_origin, page_width);
            let abs_y1 = resolve_y(line.y1, &line.y1_origin, page_height);
            let abs_x2 = resolve_x(line.x2, &line.x2_origin, page_width);
            let abs_y2 = resolve_y(line.y2, &line.y2_origin, page_height);

            content_parts.push(format!(
                "q {} {} {} RG {} w {} {} m {} {} l S Q",
                line.color[0], line.color[1], line.color[2],
                line.thickness,
                abs_x1, abs_y1,
                abs_x2, abs_y2
            ));
        }

        for rect in &config.rectangles {
            let abs_x1 = resolve_x(rect.x1, &rect.x1_origin, page_width);
            let abs_y1 = resolve_y(rect.y1, &rect.y1_origin, page_height);
            let abs_x2 = resolve_x(rect.x2, &rect.x2_origin, page_width);
            let abs_y2 = resolve_y(rect.y2, &rect.y2_origin, page_height);

            let rx = abs_x1.min(abs_x2);
            let ry = abs_y1.min(abs_y2);
            let rw = (abs_x2 - abs_x1).abs();
            let rh = (abs_y2 - abs_y1).abs();

            content_parts.push(format!(
                "q {} {} {} RG {} w {} {} {} {} re S Q",
                rect.color[0], rect.color[1], rect.color[2],
                rect.thickness,
                rx, ry, rw, rh
            ));
        }

        for item in &config.stamps {
            let base_font = resolve_base_font(&item.font);
            let font_key = ensure_font(doc, page_id, base_font)
                .map_err(|e| format!("page {page_number}: {e}"))?;
            let font_name = encode_pdf_name(&font_key);

            let mut abs_x = resolve_x(item.x, &item.origin_x, page_width);
            let mut abs_y = resolve_y(item.y, &item.origin_y, page_height);

            // Horizontal alignment: shift position based on measured text width
            if let Some(ref align) = item.align {
                let tw = text_width_pt(base_font, &item.text, item.size);
                match align.as_str() {
                    "centre" | "center" => abs_x -= tw / 2.0,
                    "right" => abs_x -= tw,
                    _ => {} // "left" or default — no adjustment
                }
            }

            // Vertical alignment: shift position based on font size
            if let Some(ref va) = item.vertical_align {
                match va.as_str() {
                    "top" => abs_y -= item.size,
                    "middle" => abs_y -= item.size / 2.0,
                    _ => {} // "bottom" or default — no adjustment
                }
            }

            // Use Tm (text matrix) when rotated, Td (translate) otherwise
            let position_op = if let Some(degrees) = item.rotation {
                let radians = degrees.to_radians();
                let cos = radians.cos();
                let sin = radians.sin();
                format!("{} {} {} {} {} {} Tm", cos, sin, -sin, cos, abs_x, abs_y)
            } else {
                format!("{} {} Td", abs_x, abs_y)
            };

            content_parts.push(format!(
                "q {} {} {} rg BT /{} {} Tf {} ({}) Tj ET Q",
                item.color[0], item.color[1], item.color[2],
                font_name, item.size,
                position_op,
                escape_pdf_text(&item.text)
            ));
        }

        // Render text blocks (word-wrapped)
        for block in &config.text_blocks {
            let base_font = resolve_base_font(&block.font);
            let font_key = ensure_font(doc, page_id, base_font)
                .map_err(|e| format!("page {page_number}: {e}"))?;
            let font_name = encode_pdf_name(&font_key);

            let abs_x = resolve_x(block.x, &block.origin_x, page_width);
            let abs_y = resolve_y(block.y, &block.origin_y, page_height);

            let lines = wrap_text(&block.text, base_font, block.size, block.width);

            for (line_idx, line) in lines.iter().enumerate() {
                let line_y = abs_y - (line_idx as f64 * block.line_spacing);
                content_parts.push(format!(
                    "q {} {} {} rg BT /{} {} Tf {} {} Td ({}) Tj ET Q",
                    block.color[0], block.color[1], block.color[2],
                    font_name, block.size,
                    abs_x, line_y,
                    escape_pdf_text(line)
                ));
            }
        }

        if content_parts.is_empty() {
            continue;
        }

        let stamp_content = content_parts.join("\n");
        let stamp_stream = Stream::new(Dictionary::new(), stamp_content.into_bytes());
        let stamp_id = doc.add_object(stamp_stream);

        // Append content stream to page.
        // /Contents can be a Reference, an Array of References, or a direct
        // Stream object — all must be preserved when appending the stamp.
        //
        // Snapshot existing Contents (immutable borrow) before mutating,
        // because direct Stream objects need to be cloned and added as
        // indirect objects to avoid borrow conflicts.
        append_content_stream(doc, page_id, stamp_id)
            .map_err(|e| format!("page {page_number}: {e}"))?;
    }

    Ok(())
}

/// Word-wrap text to fit within `max_width` points.
///
/// Splits on whitespace and accumulates words per line. A word wider than
/// max_width goes on its own line (no mid-word breaking — matches Java).
fn wrap_text(text: &str, base_font: &str, font_size: f64, max_width: f64) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return vec![];
    }

    let space_width = text_width_pt(base_font, " ", font_size);
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_width: f64 = 0.0;

    for word in words {
        let word_width = text_width_pt(base_font, word, font_size);

        if current_line.is_empty() {
            // First word on the line — always accept it
            current_line.push_str(word);
            current_width = word_width;
        } else if current_width + space_width + word_width <= max_width {
            // Fits on current line
            current_line.push(' ');
            current_line.push_str(word);
            current_width += space_width + word_width;
        } else {
            // Doesn't fit — start new line
            lines.push(current_line);
            current_line = word.to_string();
            current_width = word_width;
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

/// Resolve an object id to its mutable dictionary for a content-stream
/// write, failing with `"<action>: <lopdf error>"`. Covers both failure
/// shapes the old silent `if let Ok(Object::Dictionary(...))` guards
/// swallowed: `get_object_mut` errors and ok-but-not-a-Dictionary objects.
/// The action closure is only evaluated on the error path.
fn require_dict_mut(
    doc: &mut Document,
    id: ObjectId,
    action: impl FnOnce() -> String,
) -> Result<&mut Dictionary, String> {
    doc.get_object_mut(id)
        .and_then(Object::as_dict_mut)
        .map_err(|e| format!("{}: {}", action(), e))
}

/// Immutable sibling of [`require_dict_mut`]: resolve an object id to its
/// dictionary for a read the caller's contract requires to succeed,
/// failing with `"<action>: <lopdf error>"`. The action closure is only
/// evaluated on the error path.
fn require_dict(
    doc: &Document,
    id: ObjectId,
    action: impl FnOnce() -> String,
) -> Result<&Dictionary, String> {
    doc.get_object(id)
        .and_then(Object::as_dict)
        .map_err(|e| format!("{}: {}", action(), e))
}

/// Escape special characters for a PDF text string (parenthesised literal).
fn escape_pdf_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Encode raw PDF-name bytes for emission into a generated content stream
/// (the `/Name` after the solidus), applying the `#XX` escaping of
/// ISO 32000 §7.3.5. Mirrors lopdf 0.34's `Writer::write_name` (writer.rs)
/// byte-for-byte — same escape set, same uppercase hex — so the name
/// emitted here and the /Font dictionary key lopdf serializes at save time
/// are textually identical and decode to the same byte sequence. Regular
/// bytes (printable ASCII minus delimiters and `#`) pass through, so the
/// generated `F_PS_*` names are unchanged by construction. Re-check the
/// mirrored rule if the lopdf pin is ever bumped.
fn encode_pdf_name(name: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(name.len());
    for &byte in name {
        if b" \t\n\r\x0C()<>[]{}/%#".contains(&byte) || !(0x21..=0x7E).contains(&byte) {
            // Infallible for String; matches lopdf's `write!(file, "#{:02X}", byte)`.
            let _ = write!(out, "#{byte:02X}");
        } else {
            out.push(byte as char);
        }
    }
    out
}

/// Append a content stream to a page's /Contents, preserving existing content.
///
/// /Contents can be a Reference (to a Stream or Array), a direct Array,
/// a direct Stream, or absent. References are dereferenced to determine
/// the target type (ISO 32000 §7.7.3.3).
///
/// Returns `Err` when the page dictionary cannot be accessed for mutation —
/// the stamp stream (and, when the original /Contents was a direct stream,
/// its promoted indirect copy) has already been added as an object at that
/// point but is wired into no /Contents, so the caller must treat the
/// document as broken rather than saved-with-a-missing-stamp.
fn append_content_stream(doc: &mut Document, page_id: ObjectId, stamp_id: ObjectId) -> Result<(), String> {
    // Snapshot the existing Contents with an immutable borrow.
    // References are dereferenced to distinguish stream vs array targets.
    enum ContentsKind {
        RefStream(ObjectId),
        RefArray(Vec<Object>),
        Array(Vec<Object>),
        Stream(Stream),
        Empty,
    }

    let kind = if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
        match page_dict.get(b"Contents") {
            Ok(Object::Reference(r)) => {
                match doc.get_object(*r) {
                    Ok(Object::Array(a)) => ContentsKind::RefArray(a.clone()),
                    _ => ContentsKind::RefStream(*r),
                }
            }
            Ok(Object::Array(a)) => ContentsKind::Array(a.clone()),
            Ok(Object::Stream(s)) => ContentsKind::Stream(s.clone()),
            _ => ContentsKind::Empty,
        }
    } else {
        ContentsKind::Empty
    };

    // For direct Stream, promote to indirect object first (no borrow conflict)
    let promoted_id = if let ContentsKind::Stream(s) = &kind {
        Some(doc.add_object(Object::Stream(s.clone())))
    } else {
        None
    };

    // Now mutate the page dict
    let page_dict = require_dict_mut(doc, page_id, || {
        "cannot append stamp content stream to the page".to_string()
    })?;
    match kind {
        ContentsKind::RefStream(existing_ref) => {
            page_dict.set("Contents", Object::Array(vec![
                Object::Reference(existing_ref),
                Object::Reference(stamp_id),
            ]));
        }
        ContentsKind::RefArray(mut existing_arr) | ContentsKind::Array(mut existing_arr) => {
            existing_arr.push(Object::Reference(stamp_id));
            page_dict.set("Contents", Object::Array(existing_arr));
        }
        ContentsKind::Stream(_) => {
            if let Some(existing_id) = promoted_id {
                page_dict.set("Contents", Object::Array(vec![
                    Object::Reference(existing_id),
                    Object::Reference(stamp_id),
                ]));
            }
        }
        ContentsKind::Empty => {
            page_dict.set("Contents", Object::Array(vec![Object::Reference(stamp_id)]));
        }
    }

    Ok(())
}

/// Validate and resolve the font name. Returns a standard PDF base font name,
/// falling back to Helvetica for unrecognized fonts (silent fallback).
fn resolve_base_font(font: &Option<String>) -> &str {
    match font {
        Some(name) if STANDARD_BASE_FONTS.contains(&name.as_str()) => name.as_str(),
        Some(_) => "Helvetica",
        None => "Helvetica",
    }
}

/// Ensure the requested base font exists in the page's /Resources, walking
/// the /Parent chain for inherited Resources. Returns the font's resource
/// key (e.g. `F_PS_Helvetica`) as raw bytes — an existing key found by
/// lookup may contain non-UTF8 bytes or delimiters (see
/// `find_base_font_in`), so callers must encode it via `encode_pdf_name`
/// before interpolating it into a content stream Tf operator.
///
/// /Font inside Resources may be a direct Dictionary or an indirect
/// Reference (ISO 32000 §7.8.3) — lookups dereference it, and registration
/// writes through the reference on leaf pages. When registration is needed
/// against inherited resources, they are cloned onto the leaf page with any
/// indirect /Font promoted to a direct dict, so a parent font dictionary
/// shared with sibling pages is never mutated. A lookup hit returns early
/// and leaves the page untouched — no clone, no page-local /Resources.
///
/// Returns `Err` when the page or its /Resources cannot be accessed for
/// mutation, when a /Font reference does not resolve to a dictionary, or
/// when a /Resources entry — on the page or any /Parent ancestor consulted
/// during the walk — is a Reference that does not resolve to a dictionary.
/// A swallowed failure here would leave content streams referencing a font
/// that was never registered, which renders blank.
/// On `Err` from the /Parent walk the page is untouched; on later `Err`s
/// the freshly created font object has already been added to the document
/// but is wired into no /Resources — an orphan, harmless because callers
/// discard the document on error (see `apply_stamp_config`).
fn ensure_font(doc: &mut Document, page_id: ObjectId, base_font: &str) -> Result<Vec<u8>, String> {
    // Phase 1: Walk /Parent chain to find /Resources (inheritable per ISO 32000).
    // Snapshot the location so we can mutate afterwards without borrow conflicts.
    enum ResourcesLocation {
        DirectOnLeaf,
        IndirectOnLeaf(ObjectId),
        Inherited(Dictionary),
        Missing,
    }

    let mut location = ResourcesLocation::Missing;
    let mut existing_name: Option<Vec<u8>> = None;

    let mut current_id = page_id;
    let mut is_leaf = true;
    for _ in 0..32 {
        if let Ok(Object::Dictionary(ref dict)) = doc.get_object(current_id) {
            // Resolve this node's /Resources to (dict, leaf location);
            // `None` walks on to /Parent. Only the Reference arm is
            // fallible: the nearest /Resources governs this page (ISO 32000
            // §7.7.3.4) and becomes Phase 2's write target, so an
            // unresolvable reference cannot be a lookup miss — walking on
            // would substitute an ancestor no conforming reader consults,
            // and falling to `Missing` would silently replace the corrupt
            // entry with a fresh dict.
            let found = match dict.get(b"Resources") {
                Ok(Object::Reference(ref_id)) => {
                    let resources = require_dict(doc, *ref_id, || {
                        format!("cannot resolve /Resources reference {ref_id:?} for font {base_font}")
                    })?;
                    Some((resources, ResourcesLocation::IndirectOnLeaf(*ref_id)))
                }
                Ok(Object::Dictionary(ref resources)) => {
                    Some((resources, ResourcesLocation::DirectOnLeaf))
                }
                _ => None,
            };
            if let Some((resources, leaf_location)) = found {
                existing_name = find_font_in_resources(doc, resources, base_font);
                location = if is_leaf {
                    leaf_location
                } else {
                    ResourcesLocation::Inherited(resources.clone())
                };
                break;
            }
            if let Ok(Object::Reference(parent_id)) = dict.get(b"Parent") {
                current_id = *parent_id;
                is_leaf = false;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    if let Some(name) = existing_name {
        return Ok(name);
    }

    // Phase 2: Font not found — add it
    let font_name = format!("F_PS_{}", base_font.replace('-', ""));

    let mut font_dict = Dictionary::new();
    font_dict.set("Type", Object::Name(b"Font".to_vec()));
    font_dict.set("Subtype", Object::Name(b"Type1".to_vec()));
    font_dict.set("BaseFont", Object::Name(base_font.as_bytes().to_vec()));
    let font_id = doc.add_object(font_dict);

    // The page-identity ("page 3") context is supplied by apply_stamp_config's
    // wrap — these messages name only the font and the object being written,
    // so a composed error carries a single page reference.
    let page_ctx = || format!("cannot register font {base_font} on the page");

    // In every branch, `add_font_to_resources` returning `Some(id)` means
    // /Font is an indirect Reference the insert could not go through — the
    // write completes via the referenced dict (leaf branches) or a page-local
    // promotion (Inherited), and an unresolvable reference is an `Err`, never
    // a silent direct-/Font overwrite. The error path is reachable only from
    // the `Some` arm: a successful direct insert never touches /Font again,
    // so pre-existing direct entries cannot be clobbered.
    match location {
        ResourcesLocation::IndirectOnLeaf(res_id) => {
            let resources = require_dict_mut(doc, res_id, || {
                format!("cannot register font {base_font} in /Resources {res_id:?}")
            })?;
            if let Some(fonts_id) = add_font_to_resources(resources, &font_name, font_id) {
                write_font_through_ref(doc, fonts_id, &font_name, font_id, base_font)?;
            }
        }
        ResourcesLocation::DirectOnLeaf => {
            let page_dict = require_dict_mut(doc, page_id, page_ctx)?;
            let resources = page_dict
                .get_mut(b"Resources")
                .and_then(Object::as_dict_mut)
                .map_err(|e| {
                    format!("cannot register font {base_font} in the page's direct /Resources: {e}")
                })?;
            if let Some(fonts_id) = add_font_to_resources(resources, &font_name, font_id) {
                write_font_through_ref(doc, fonts_id, &font_name, font_id, base_font)?;
            }
        }
        ResourcesLocation::Inherited(mut inherited) => {
            if let Some(fonts_id) = add_font_to_resources(&mut inherited, &font_name, font_id) {
                // The inherited /Font is an indirect reference to a dict the
                // parent shares with sibling pages — promote it to a direct
                // dict in the page-local resources clone (copy + add) rather
                // than writing through the reference, so the shared font
                // dictionary is never mutated.
                let fonts = require_dict(doc, fonts_id, || {
                    format!("cannot register font {base_font} through the inherited /Font reference {fonts_id:?}")
                })?;
                let mut local_fonts = fonts.clone();
                local_fonts.set(font_name.as_str(), Object::Reference(font_id));
                inherited.set("Font", Object::Dictionary(local_fonts));
            }
            let page_dict = require_dict_mut(doc, page_id, page_ctx)?;
            page_dict.set("Resources", Object::Dictionary(inherited));
        }
        ResourcesLocation::Missing => {
            let page_dict = require_dict_mut(doc, page_id, page_ctx)?;
            let mut resources = Dictionary::new();
            let created = add_font_to_resources(&mut resources, &font_name, font_id);
            debug_assert!(created.is_none(), "a fresh dict cannot hold an indirect /Font");
            page_dict.set("Resources", Object::Dictionary(resources));
        }
    }

    Ok(font_name.into_bytes())
}

/// Complete a font registration whose /Font entry is an indirect reference:
/// write the entry into the referenced dictionary. Errs when the reference
/// does not resolve to a dictionary — falling back to a direct /Font write
/// there would mask document corruption (the d36e4b0f lesson, adapted to
/// the post-AMPHTT-1159 error contract).
fn write_font_through_ref(
    doc: &mut Document,
    fonts_id: ObjectId,
    font_name: &str,
    font_id: ObjectId,
    base_font: &str,
) -> Result<(), String> {
    let fonts = require_dict_mut(doc, fonts_id, || {
        format!("cannot register font {base_font} through the indirect /Font reference {fonts_id:?}")
    })?;
    fonts.set(font_name, Object::Reference(font_id));
    Ok(())
}

/// Search for an existing font with the given BaseFont name in a Resources dictionary.
/// Returns the font's resource key name if found. Handles /Font as either a
/// direct Dictionary or an indirect Reference (ISO 32000 §7.8.3 permits both).
/// An unresolvable Reference yields None — "not found" is the right answer
/// for a lookup; the registration path reports the broken reference as `Err`.
fn find_font_in_resources(doc: &Document, resources: &Dictionary, base_font: &str) -> Option<Vec<u8>> {
    match resources.get(b"Font") {
        Ok(Object::Dictionary(ref fonts)) => find_base_font_in(doc, fonts, base_font),
        Ok(Object::Reference(ref_id)) => {
            if let Ok(Object::Dictionary(ref fonts)) = doc.get_object(*ref_id) {
                find_base_font_in(doc, fonts, base_font)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Search a /Font dictionary for an entry with the given BaseFont name.
/// Entries may be indirect References or direct dictionaries (ISO 32000
/// §7.8.3 permits both). An entry Reference that does not resolve to a
/// dictionary is skipped — "not found" is the right answer for a lookup,
/// and this function never becomes a write target (unlike the /Resources
/// and /Font container references, whose registration paths error).
///
/// The matched key is returned as its raw bytes: a legal name key may
/// decode (via `#XX` escapes) to bytes that are not UTF-8 or that contain
/// delimiters, and a lossy conversion would destroy exactly the bytes an
/// emission site needs to reference the entry — encoding happens only at
/// emission, via `encode_pdf_name`.
fn find_base_font_in(doc: &Document, fonts: &Dictionary, base_font: &str) -> Option<Vec<u8>> {
    let target = base_font.as_bytes();
    for (name, entry) in fonts.iter() {
        let font_dict = match entry {
            Object::Reference(fref) => match doc.get_object(*fref) {
                Ok(Object::Dictionary(ref d)) => d,
                _ => continue,
            },
            Object::Dictionary(ref d) => d,
            _ => continue,
        };
        if let Ok(Object::Name(ref bf)) = font_dict.get(b"BaseFont") {
            if bf == target {
                return Some(name.clone());
            }
        }
    }
    None
}

/// Add a font to a Resources /Font dictionary.
/// Returns `Some(object_id)` when /Font is an indirect Reference the caller
/// must write through instead (this function holds only the resources dict,
/// not the document, so it cannot dereference). Discarding the `Some` silently
/// drops the registration — the caller must complete or fail the write.
#[must_use]
fn add_font_to_resources(resources: &mut Dictionary, font_name: &str, font_id: ObjectId) -> Option<ObjectId> {
    match resources.get(b"Font") {
        Ok(Object::Reference(ref_id)) => return Some(*ref_id),
        Ok(Object::Dictionary(_)) => {}
        // A Reference was diverted to the caller above. Any other value
        // cannot hold font entries, so replacing it (or creating a missing
        // /Font) repairs the malformed entry without discarding fonts —
        // unlike the indirect case, where an unresolvable reference is an
        // error upstream (see write_font_through_ref / ensure_font).
        _ => resources.set("Font", Object::Dictionary(Dictionary::new())),
    }
    if let Ok(Object::Dictionary(ref mut fonts)) = resources.get_mut(b"Font") {
        fonts.set(font_name, Object::Reference(font_id));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us_letter_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(612),
            Object::Integer(792),
        ]
    }

    /// Build an `n`-page PDF for content-stream tests, mirroring the fixture
    /// helpers in the sibling test modules (document.rs, annotation.rs,
    /// manipulation.rs). `media_box: None` omits the MediaBox entirely so
    /// the US Letter fallback path can be exercised. Returns the document
    /// and the page ids in physical order.
    fn build_test_pdf(page_count: usize, media_box: Option<Vec<Object>>) -> (Document, Vec<ObjectId>) {
        let mut doc = Document::new();
        let pages_id = doc.new_object_id();

        let mut page_ids = Vec::new();
        let mut kids = Vec::new();
        for _ in 0..page_count {
            let content_id = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));

            let mut page_dict = Dictionary::new();
            page_dict.set("Type", Object::Name(b"Page".to_vec()));
            page_dict.set("Parent", Object::Reference(pages_id));
            if let Some(ref mb) = media_box {
                page_dict.set("MediaBox", Object::Array(mb.clone()));
            }
            page_dict.set("Contents", Object::Reference(content_id));
            let page_id = doc.add_object(page_dict);
            page_ids.push(page_id);
            kids.push(Object::Reference(page_id));
        }

        let mut pages_dict = Dictionary::new();
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set("Kids", Object::Array(kids));
        pages_dict.set("Count", Object::Integer(page_count as i64));
        doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));

        (doc, page_ids)
    }

    /// Add a Type1 font object for `base_font`; returns its object id.
    fn add_type1_font(doc: &mut Document, base_font: &str) -> ObjectId {
        let mut font = Dictionary::new();
        font.set("Type", Object::Name(b"Font".to_vec()));
        font.set("Subtype", Object::Name(b"Type1".to_vec()));
        font.set("BaseFont", Object::Name(base_font.as_bytes().to_vec()));
        doc.add_object(font)
    }

    /// Add an indirect /Font dictionary object holding `key` → a Type1
    /// `base_font`; returns the /Font dictionary's object id, for pages to
    /// reference from /Resources in the indirect-/Font tests. The key is
    /// generic over `Into<Vec<u8>>` so the name-encoding tests can register
    /// non-UTF8 or delimiter-carrying keys; `&str` call sites are unchanged.
    fn add_indirect_font_dict(doc: &mut Document, key: impl Into<Vec<u8>>, base_font: &str) -> ObjectId {
        let font_obj_id = add_type1_font(doc, base_font);
        let mut fonts = Dictionary::new();
        fonts.set(key, Object::Reference(font_obj_id));
        doc.add_object(Object::Dictionary(fonts))
    }

    /// Give `page_id` its own direct /Resources whose /Font is `font_value`
    /// (a direct dict or an indirect reference, depending on the test).
    fn set_page_font(doc: &mut Document, page_id: ObjectId, font_value: Object) {
        let mut resources = Dictionary::new();
        resources.set("Font", font_value);
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Resources", Object::Dictionary(resources));
        }
    }

    /// The page's /Parent (Pages) node id — where inherited resources live.
    fn parent_pages_id(doc: &Document, page_id: ObjectId) -> ObjectId {
        match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref page_dict)) => match page_dict.get(b"Parent") {
                Ok(Object::Reference(r)) => *r,
                other => panic!("expected /Parent reference, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        }
    }

    /// Navigate page → direct /Resources → direct /Font, panicking with a
    /// which-level message on any mismatch; callers assert on the returned
    /// dictionary's contents. #[track_caller] keeps panic locations at the
    /// calling test rather than in here.
    #[track_caller]
    fn page_font_dict(doc: &Document, page_id: ObjectId) -> &Dictionary {
        match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref page_dict)) => match page_dict.get(b"Resources") {
                Ok(Object::Dictionary(ref resources)) => match resources.get(b"Font") {
                    Ok(Object::Dictionary(ref fonts)) => fonts,
                    other => panic!("expected direct /Font dict, got {other:?}"),
                },
                other => panic!("expected direct /Resources dict, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        }
    }

    #[test]
    fn escape_pdf_text_special_chars() {
        assert_eq!(escape_pdf_text(r"a\b"), r"a\\b");
        assert_eq!(escape_pdf_text("a(b)c"), r"a\(b\)c");
        assert_eq!(escape_pdf_text("line1\nline2"), r"line1\nline2");
    }

    #[test]
    fn encode_pdf_name_passes_regular_names_unchanged() {
        // The generated F_PS_* names are all regular bytes, so uniform
        // encoding at emission is a no-op for them by construction.
        assert_eq!(encode_pdf_name(b"F_PS_Helvetica"), "F_PS_Helvetica");
        assert_eq!(encode_pdf_name(b"TR7"), "TR7");
    }

    #[test]
    fn encode_pdf_name_escapes_delimiters_whitespace_hash_and_non_regular_bytes() {
        assert_eq!(encode_pdf_name(b"F 1"), "F#201");
        assert_eq!(encode_pdf_name(b"F/1"), "F#2F1");
        assert_eq!(encode_pdf_name(b"F#1"), "F#231");
        assert_eq!(encode_pdf_name(b"a(b)"), "a#28b#29");
        // The non-UTF8 bytes lopdf's own parser test decodes from /#cb#ce#cc#e5.
        assert_eq!(encode_pdf_name(b"\xCB\xCE\xCC\xE5"), "#CB#CE#CC#E5");
        // ISO 32000 §7.3.5 forbids #00 in a conforming name, but lopdf's
        // writer emits it for a nonconforming key — matching that keeps the
        // dictionary key and the content-stream name agreed either way.
        assert_eq!(encode_pdf_name(b"\x00"), "#00");
        assert_eq!(encode_pdf_name(b""), "");
    }

    #[test]
    fn wrap_text_single_line() {
        // "Hello" in Courier at 10pt = 30pt, fits easily in 200pt
        let lines = wrap_text("Hello", "Courier", 10.0, 200.0);
        assert_eq!(lines, vec!["Hello"]);
    }

    #[test]
    fn wrap_text_basic_wrapping() {
        // Courier 10pt: each char = 6pt, space = 6pt
        // "aa bb cc" = 3 words, each 12pt wide, space 6pt
        // max_width = 25pt → "aa bb" (12+6+12=30 > 25) → "aa" then "bb cc" (12+6+12=30 > 25) → "aa", "bb", "cc"
        // Actually: "aa" = 12pt, fits. "bb" = 12+6+12=30 > 25, new line. "cc" = 12+6+12=30 > 25, new line.
        let lines = wrap_text("aa bb cc", "Courier", 10.0, 25.0);
        assert_eq!(lines, vec!["aa", "bb", "cc"]);
    }

    #[test]
    fn wrap_text_fits_on_one_line() {
        // "aa bb" at Courier 10pt = 12+6+12 = 30pt, max 100pt → one line
        let lines = wrap_text("aa bb", "Courier", 10.0, 100.0);
        assert_eq!(lines, vec!["aa bb"]);
    }

    #[test]
    fn wrap_text_empty_string() {
        let lines = wrap_text("", "Courier", 10.0, 100.0);
        assert!(lines.is_empty());
    }

    #[test]
    fn wrap_text_word_wider_than_max() {
        // "Supercalifragilistic" at Courier 10pt = 20*6 = 120pt, max 50pt
        // Single word goes on its own line without breaking
        let lines = wrap_text("Supercalifragilistic", "Courier", 10.0, 50.0);
        assert_eq!(lines, vec!["Supercalifragilistic"]);
    }

    #[test]
    fn wrap_text_whitespace_only() {
        let lines = wrap_text("   ", "Courier", 10.0, 100.0);
        assert!(lines.is_empty());
    }

    #[test]
    fn deserialize_rgb_clamps_and_normalizes_channels() {
        let json = r#"{"x1": 0, "y1": 0, "x2": 1, "y2": 1, "color": [1.5, -0.25, 0.5]}"#;
        let line: LineItem = serde_json::from_str(json).unwrap();
        assert_eq!(line.color, [1.0, 0.0, 0.5]);

        // In-range channels pass through unchanged; negative zero is
        // normalized to +0.0 (it compares equal, so assert the sign bit).
        let json = r#"{"x1": 0, "y1": 0, "x2": 1, "y2": 1, "color": [-0.0, 0.42, 1.0]}"#;
        let line: LineItem = serde_json::from_str(json).unwrap();
        assert_eq!(line.color, [0.0, 0.42, 1.0]);
        assert!(
            line.color[0].is_sign_positive(),
            "negative zero should be normalized to +0.0"
        );
    }

    #[test]
    fn deserialize_line_defaults() {
        let json = r#"{"x1": 10, "y1": 20, "x2": 300, "y2": 400}"#;
        let line: LineItem = serde_json::from_str(json).unwrap();
        assert_eq!(line.x1, 10.0);
        assert_eq!(line.y1, 20.0);
        assert_eq!(line.x2, 300.0);
        assert_eq!(line.y2, 400.0);
        assert_eq!(line.x1_origin, "left");
        assert_eq!(line.y1_origin, "bottom");
        assert_eq!(line.x2_origin, "left");
        assert_eq!(line.y2_origin, "bottom");
        assert_eq!(line.color, [0.0, 0.0, 0.0]);
        assert_eq!(line.thickness, 0.5);
    }

    #[test]
    fn deserialize_rectangle_defaults() {
        let json = r#"{"x1": 0, "y1": 0, "x2": 100, "y2": 50}"#;
        let rect: RectangleItem = serde_json::from_str(json).unwrap();
        assert_eq!(rect.x1, 0.0);
        assert_eq!(rect.y1, 0.0);
        assert_eq!(rect.x2, 100.0);
        assert_eq!(rect.y2, 50.0);
        assert_eq!(rect.x1_origin, "left");
        assert_eq!(rect.y1_origin, "bottom");
        assert_eq!(rect.x2_origin, "left");
        assert_eq!(rect.y2_origin, "bottom");
        assert_eq!(rect.color, [0.0, 0.0, 0.0]);
        assert_eq!(rect.thickness, 0.5);
    }

    #[test]
    fn deserialize_config_with_all_element_types() {
        let json = r#"{
            "stamps": [{"text": "hello", "x": 0, "y": 0, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [0, 0, 0]}],
            "text_blocks": [{"text": "block", "x": 0, "y": 0, "width": 100, "line_spacing": 14}],
            "lines": [{"x1": 0, "y1": 0, "x2": 100, "y2": 0}],
            "rectangles": [{"x1": 0, "y1": 0, "x2": 50, "y2": 50}]
        }"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.stamps.len(), 1);
        assert_eq!(config.text_blocks.len(), 1);
        assert_eq!(config.lines.len(), 1);
        assert_eq!(config.rectangles.len(), 1);
    }

    #[test]
    fn test_apply_stamp_config_renders_on_all_pages() {
        let (mut doc, _page_ids) = build_test_pdf(2, Some(us_letter_box()));

        let json = r#"{"stamps": [{"text": "VIEWED", "x": 10, "y": 10, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [0.5, 0.5, 0.5]}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        // Each page should now have a /Contents array with 2 elements (original + stamp)
        let pages = doc.get_pages();
        for &page_id in pages.values() {
            if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
                if let Ok(Object::Array(ref contents)) = page_dict.get(b"Contents") {
                    assert_eq!(contents.len(), 2, "Page should have original + stamp content streams");
                } else {
                    panic!("Page /Contents should be an array after stamping");
                }
            }
        }
    }

    #[test]
    fn test_apply_stamp_config_falls_back_to_us_letter_without_bbox() {
        // 1-page PDF with no MediaBox/CropBox anywhere: stamping proceeds
        // against the US Letter fallback (612x792) rather than skipping the
        // page or using zero-collapsed dimensions. Pins the value of
        // geometry::US_LETTER_FALLBACK end-to-end through a real call site.
        let (mut doc, page_ids) = build_test_pdf(1, None);
        let page_id = page_ids[0];

        // A line starting at (0 from right, 0 from top): with the fallback
        // dimensions the start point resolves to exactly (612, 792).
        let json = r#"{"lines": [{"x1": 0, "y1": 0, "x2": 0, "y2": 0, "x1_origin": "right", "y1_origin": "top"}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let page = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => d.clone(),
            other => panic!("expected page dictionary, got {:?}", other),
        };
        let contents = match page.get(b"Contents") {
            Ok(Object::Array(ref a)) => a.clone(),
            other => panic!("expected /Contents array after stamping, got {:?}", other),
        };
        assert_eq!(contents.len(), 2, "Page should have original + stamp content streams");

        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {:?}", other),
        };
        assert!(
            content.contains("612 792 m"),
            "stamp coordinates should resolve against the US Letter fallback, got: {content}"
        );
    }

    #[test]
    fn test_apply_stamp_config_renders_text_block_with_line_spacing() {
        // Courier is fixed-pitch: every glyph is 600 units = 6 pt/char at
        // size 10. "aaaa bbbb" = 24 + 6 + 24 = 54 pt <= 70; adding " cccc"
        // -> 84 pt > 70, so the block wraps to exactly two lines.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];

        let json = r#"{"text_blocks": [{"text": "aaaa bbbb cccc", "x": 50, "y": 700, "width": 70, "size": 10, "font": "Courier", "line_spacing": 14}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let contents = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array after stamping, got {:?}", other),
            },
            other => panic!("expected page dictionary, got {:?}", other),
        };
        assert_eq!(contents.len(), 2, "Page should have original + stamp content streams");

        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {:?}", other),
        };

        // Text-showing structure and font selection
        assert!(content.contains("BT"), "should open a text object, got: {content}");
        assert!(content.contains("ET"), "should close the text object, got: {content}");
        assert!(
            content.contains("/F_PS_Courier 10 Tf"),
            "should select the registered font, got: {content}"
        );

        // The /Tf reference is registered, not dangling:
        // /Resources -> /Font -> F_PS_Courier on the page
        let fonts = page_font_dict(&doc, page_id);
        assert!(
            fonts.get(b"F_PS_Courier").is_ok(),
            "F_PS_Courier should be registered in /Resources"
        );

        // Wrapped lines step down by exactly line_spacing: 700, then
        // 700 - 14 = 686. If line_spacing were ignored (the 0-spacing
        // overlap failure) both lines would land at y=700 and the second
        // assertion would fail — the test cannot pass vacuously.
        assert!(
            content.contains("50 700 Td (aaaa bbbb) Tj"),
            "first wrapped line should sit at the block origin, got: {content}"
        );
        assert!(
            content.contains("50 686 Td (cccc) Tj"),
            "second wrapped line should sit one line_spacing below, got: {content}"
        );
    }

    #[test]
    fn test_apply_stamp_config_clamps_rgb_in_content_stream() {
        // Out-of-range channels must be clamped into DeviceRGB's [0.0, 1.0]
        // before reaching the content stream, covering both a stroke
        // (RG, line) and a fill (rg, stamp text) operator, plus negative
        // zero (in-range for clamp, but would emit "-0" unnormalized).
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];

        let json = r#"{
            "stamps": [{"text": "X", "x": 10, "y": 10, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [1.5, -0.25, 0.5]}],
            "lines": [{"x1": 0, "y1": 0, "x2": 100, "y2": 0, "color": [-0.0, -0.25, 0.5]}]
        }"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let contents = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array after stamping, got {:?}", other),
            },
            other => panic!("expected page dictionary, got {:?}", other),
        };
        assert_eq!(contents.len(), 2, "Page should have original + stamp content streams");

        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {:?}", other),
        };

        assert!(
            content.contains("0 0 0.5 RG"),
            "line stroke colour should be clamped and sign-normalized, got: {content}"
        );
        assert!(
            content.contains("1 0 0.5 rg"),
            "stamp fill colour should be clamped, got: {content}"
        );
        assert!(
            !content.contains("1.5") && !content.contains("-0"),
            "unclamped or negative-zero channel values must never reach the output, got: {content}"
        );
    }

    #[test]
    fn test_ensure_font_adds_to_resources() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];

        let font_name = ensure_font(&mut doc, page_id, "Helvetica")
            .expect("font registration should succeed");
        assert_eq!(font_name, b"F_PS_Helvetica");

        // Verify font was added to page resources
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            if let Ok(Object::Dictionary(ref resources)) = page_dict.get(b"Resources") {
                if let Ok(Object::Dictionary(ref fonts)) = resources.get(b"Font") {
                    assert!(fonts.get(b"F_PS_Helvetica").is_ok(), "Font should be in resources");
                    return;
                }
            }
        }
        panic!("Font not found in page resources");
    }

    #[test]
    fn test_ensure_font_errors_when_page_unresolvable() {
        // Unreachable through apply_stamp_config (get_pages only yields
        // dictionary-resolvable ids) — called directly, as metadata.rs's
        // error-path tests do. Pins the Err contract so a regression back
        // to the old silent-skip guards cannot pass unnoticed.
        let mut doc = Document::new();
        let err = ensure_font(&mut doc, (9999, 0), "Helvetica")
            .expect_err("a dangling page id must fail font registration");
        assert!(
            err.contains("cannot register font Helvetica"),
            "unexpected error message: {err}"
        );

        // The ok-but-not-a-Dictionary shape (as_dict_mut failure).
        let int_id = doc.add_object(Object::Integer(7));
        let err = ensure_font(&mut doc, int_id, "Courier")
            .expect_err("a non-dictionary page object must fail font registration");
        assert!(
            err.contains("cannot register font Courier"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_append_content_stream_errors_when_page_unresolvable() {
        let mut doc = Document::new();
        let stamp_id = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
        let err = append_content_stream(&mut doc, (9999, 0), stamp_id)
            .expect_err("a dangling page id must fail the content-stream append");
        assert!(
            err.contains("cannot append stamp content stream"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_ensure_font_finds_existing_font_through_indirect_font_ref() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id = add_indirect_font_dict(&mut doc, "F1", "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));
        let object_count = doc.objects.len();

        let font_name = ensure_font(&mut doc, page_id, "Times-Roman")
            .expect("lookup through an indirect /Font reference should succeed");

        assert_eq!(font_name, b"F1", "existing font should be found through the reference");
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a lookup hit should not add objects or re-register the font"
        );
    }

    #[test]
    fn test_ensure_font_finds_direct_dict_font_entry() {
        // A font stored as a direct dictionary inside /Font (legal per
        // ISO 32000 §7.8.3) was invisible to the Reference-only lookup,
        // so every stamp pass registered a duplicate F_PS_* object.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];

        let mut font = Dictionary::new();
        font.set("Type", Object::Name(b"Font".to_vec()));
        font.set("Subtype", Object::Name(b"Type1".to_vec()));
        font.set("BaseFont", Object::Name(b"Times-Roman".to_vec()));
        let mut fonts = Dictionary::new();
        fonts.set("TR7", Object::Dictionary(font));
        set_page_font(&mut doc, page_id, Object::Dictionary(fonts));
        let object_count = doc.objects.len();

        let font_name = ensure_font(&mut doc, page_id, "Times-Roman")
            .expect("lookup of a direct-dict font entry should succeed");

        assert_eq!(font_name, b"TR7", "existing direct-dict entry should be reused");
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a lookup hit should not add objects or re-register the font"
        );
        let fonts = page_font_dict(&doc, page_id);
        assert_eq!(
            fonts.len(),
            1,
            "no duplicate entry should be registered alongside TR7"
        );
    }

    #[test]
    fn test_ensure_font_returns_raw_bytes_for_non_utf8_key() {
        // A legal /Font key may decode (via #XX escapes) to non-UTF8 bytes —
        // lopdf's parser test pins /#cb#ce#cc#e5 → these bytes. Pre-fix,
        // from_utf8_lossy replaced them with U+FFFD, so the emitted Tf name
        // no longer byte-matched the dictionary key and the stamp rendered
        // blank in a conforming viewer.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let key: &[u8] = b"\xCB\xCE\xCC\xE5";
        let fonts_id = add_indirect_font_dict(&mut doc, key, "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));
        let object_count = doc.objects.len();

        let font_key = ensure_font(&mut doc, page_id, "Times-Roman")
            .expect("lookup of a non-UTF8 key should succeed");

        assert_eq!(font_key, key, "the matched key must be returned byte-faithfully");
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a lookup hit should not add objects or re-register the font"
        );
    }

    #[test]
    fn test_ensure_font_registers_through_indirect_font_ref() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id = add_indirect_font_dict(&mut doc, "F1", "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("registration through an indirect /Font reference should succeed");
        assert_eq!(font_name, b"F_PS_Courier");

        // The write went through the reference: the referenced dict holds the
        // original and the new entry, both resolving to their BaseFonts.
        let fonts = match doc.get_object(fonts_id) {
            Ok(Object::Dictionary(ref fonts)) => fonts.clone(),
            other => panic!("expected the referenced /Font dictionary, got {other:?}"),
        };
        assert!(
            find_base_font_in(&doc, &fonts, "Times-Roman").is_some(),
            "pre-existing font should be preserved"
        );
        assert!(
            find_base_font_in(&doc, &fonts, "Courier").is_some(),
            "new font should be registered through the reference"
        );

        // The page still points at the same indirect /Font — not a direct replacement.
        if let Ok(Object::Dictionary(ref page_dict)) = doc.get_object(page_id) {
            if let Ok(Object::Dictionary(ref resources)) = page_dict.get(b"Resources") {
                assert_eq!(
                    resources.get(b"Font").unwrap(),
                    &Object::Reference(fonts_id),
                    "/Font should remain an indirect reference"
                );
                return;
            }
        }
        panic!("page should keep its direct /Resources");
    }

    #[test]
    fn test_ensure_font_registers_through_indirect_resources_and_font_ref() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id = add_indirect_font_dict(&mut doc, "F1", "Times-Roman");

        // /Resources is itself an indirect object whose /Font is indirect too
        // (the IndirectOnLeaf write-through path).
        let mut resources = Dictionary::new();
        resources.set("Font", Object::Reference(fonts_id));
        let res_id = doc.add_object(Object::Dictionary(resources));
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Resources", Object::Reference(res_id));
        }

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("registration through indirect /Resources and /Font should succeed");
        assert_eq!(font_name, b"F_PS_Courier");

        let fonts = match doc.get_object(fonts_id) {
            Ok(Object::Dictionary(ref fonts)) => fonts.clone(),
            other => panic!("expected the referenced /Font dictionary, got {other:?}"),
        };
        assert!(
            find_base_font_in(&doc, &fonts, "Times-Roman").is_some(),
            "pre-existing font should be preserved"
        );
        assert!(
            find_base_font_in(&doc, &fonts, "Courier").is_some(),
            "new font should be registered through the reference"
        );

        // The resources object still holds /Font as the same reference.
        match doc.get_object(res_id) {
            Ok(Object::Dictionary(ref resources)) => assert_eq!(
                resources.get(b"Font").unwrap(),
                &Object::Reference(fonts_id),
                "/Font should remain an indirect reference"
            ),
            other => panic!("expected the referenced /Resources dictionary, got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_font_inherited_promotion_preserves_shared_font_dict() {
        let (mut doc, page_ids) = build_test_pdf(2, Some(us_letter_box()));
        let page_id = page_ids[0];
        let sibling_id = page_ids[1];
        let fonts_id = add_indirect_font_dict(&mut doc, "F1", "Times-Roman");

        // /Resources lives on the shared Pages parent, with /Font indirect.
        let pages_id = parent_pages_id(&doc, page_id);
        let mut resources = Dictionary::new();
        resources.set("Font", Object::Reference(fonts_id));
        if let Ok(Object::Dictionary(ref mut pages_dict)) = doc.get_object_mut(pages_id) {
            pages_dict.set("Resources", Object::Dictionary(resources));
        }

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("registration against inherited resources should succeed");
        assert_eq!(font_name, b"F_PS_Courier");

        // The page gained its own /Resources whose /Font was promoted to a
        // direct dict holding the inherited font plus the new one.
        let fonts = page_font_dict(&doc, page_id);
        assert!(
            find_base_font_in(&doc, fonts, "Times-Roman").is_some(),
            "inherited font should be carried into the promoted dict"
        );
        assert!(
            find_base_font_in(&doc, fonts, "Courier").is_some(),
            "new font should be in the promoted dict"
        );

        // The shared referenced font dict is unmutated — still exactly its
        // original single entry — and the sibling page still inherits it.
        match doc.get_object(fonts_id) {
            Ok(Object::Dictionary(ref shared)) => {
                assert_eq!(shared.len(), 1, "shared parent font dict must not gain entries");
                assert!(shared.get(b"F1").is_ok(), "original entry should be untouched");
            }
            other => panic!("expected the shared /Font dictionary, got {other:?}"),
        }
        match doc.get_object(sibling_id) {
            Ok(Object::Dictionary(ref sibling)) => assert!(
                sibling.get(b"Resources").is_err(),
                "sibling page should still inherit the parent resources"
            ),
            other => panic!("expected sibling page dictionary, got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_font_errors_when_indirect_font_dangling() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        set_page_font(&mut doc, page_id, Object::Reference((9999, 0)));

        let err = ensure_font(&mut doc, page_id, "Courier")
            .expect_err("a dangling /Font reference must fail font registration");
        assert!(
            err.contains("cannot register font Courier"),
            "unexpected error message: {err}"
        );

        // The ok-but-not-a-Dictionary shape (as_dict_mut failure).
        let int_id = doc.add_object(Object::Integer(7));
        set_page_font(&mut doc, page_id, Object::Reference(int_id));
        let err = ensure_font(&mut doc, page_id, "Helvetica")
            .expect_err("a /Font reference to a non-dictionary must fail font registration");
        assert!(
            err.contains("cannot register font Helvetica"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_ensure_font_errors_when_indirect_resources_font_dangling() {
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];

        let mut resources = Dictionary::new();
        resources.set("Font", Object::Reference((9999, 0)));
        let res_id = doc.add_object(Object::Dictionary(resources));
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Resources", Object::Reference(res_id));
        }

        let err = ensure_font(&mut doc, page_id, "Courier")
            .expect_err("a dangling /Font reference under indirect /Resources must fail");
        assert!(
            err.contains("cannot register font Courier"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_ensure_font_errors_when_inherited_font_dangling() {
        // The old pdf-stamp binary repaired a dangling inherited /Font by
        // writing a fresh direct dict; post-AMPHTT-1159 an unresolvable
        // reference is an error in every branch (intentional divergence).
        let (mut doc, page_ids) = build_test_pdf(2, Some(us_letter_box()));
        let page_id = page_ids[0];

        let pages_id = parent_pages_id(&doc, page_id);
        let mut resources = Dictionary::new();
        resources.set("Font", Object::Reference((9999, 0)));
        if let Ok(Object::Dictionary(ref mut pages_dict)) = doc.get_object_mut(pages_id) {
            pages_dict.set("Resources", Object::Dictionary(resources));
        }

        let err = ensure_font(&mut doc, page_id, "Courier")
            .expect_err("a dangling inherited /Font reference must fail");
        assert!(
            err.contains("cannot register font Courier"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_ensure_font_errors_when_leaf_resources_dangling() {
        // Pre-AMPHTT-1234 a dangling leaf /Resources reference fell through
        // to the Missing branch, which silently replaced it with a fresh
        // direct dict holding only the stamp font. Pins the Err contract
        // and that the corrupt reference is left in place.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Resources", Object::Reference((9999, 0)));
        }
        let object_count = doc.objects.len();

        let err = ensure_font(&mut doc, page_id, "Helvetica")
            .expect_err("a dangling leaf /Resources reference must fail font registration");
        assert!(
            err.contains("cannot resolve /Resources reference"),
            "unexpected error message: {err}"
        );

        // The document must be untouched — the Err fires in the /Parent
        // walk, before Phase 2 could replace the reference or create the
        // (orphan) font object it would add on later error paths.
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a /Parent-walk Err must not add objects"
        );
        match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref page_dict)) => match page_dict.get(b"Resources") {
                Ok(Object::Reference(r)) => assert_eq!(*r, (9999, 0)),
                other => panic!("dangling /Resources reference was replaced with {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        }

        // The ok-but-not-a-Dictionary shape (as_dict failure).
        let int_id = doc.add_object(Object::Integer(7));
        if let Ok(Object::Dictionary(ref mut page_dict)) = doc.get_object_mut(page_id) {
            page_dict.set("Resources", Object::Reference(int_id));
        }
        let object_count = doc.objects.len();
        let err = ensure_font(&mut doc, page_id, "Courier")
            .expect_err("a /Resources reference to a non-dictionary must fail font registration");
        assert!(
            err.contains("cannot resolve /Resources reference"),
            "unexpected error message: {err}"
        );
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a /Parent-walk Err must not add objects"
        );
        match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref page_dict)) => match page_dict.get(b"Resources") {
                Ok(Object::Reference(r)) => assert_eq!(*r, int_id),
                other => panic!("non-dict /Resources reference was replaced with {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_font_errors_when_intermediate_resources_dangling() {
        // Uniform contract: an unresolvable /Resources reference on an
        // intermediate /Parent node is an Err even when a grandparent
        // holds valid resources with the requested font — the walk must
        // not repair corruption by consulting ancestors past the nearest
        // /Resources entry (the one a conforming reader uses, ISO 32000
        // §7.7.3.4).
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let pages_id = parent_pages_id(&doc, page_id);

        let courier_id = add_type1_font(&mut doc, "Courier");
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference(courier_id));
        let mut gp_resources = Dictionary::new();
        gp_resources.set("Font", Object::Dictionary(fonts));
        let mut grandparent = Dictionary::new();
        grandparent.set("Type", Object::Name(b"Pages".to_vec()));
        grandparent.set("Resources", Object::Dictionary(gp_resources));
        let grandparent_id = doc.add_object(Object::Dictionary(grandparent));

        if let Ok(Object::Dictionary(ref mut pages_dict)) = doc.get_object_mut(pages_id) {
            pages_dict.set("Parent", Object::Reference(grandparent_id));
            pages_dict.set("Resources", Object::Reference((9999, 0)));
        }
        let object_count = doc.objects.len();

        let err = ensure_font(&mut doc, page_id, "Courier")
            .expect_err("a dangling intermediate /Resources reference must fail");
        assert!(
            err.contains("cannot resolve /Resources reference"),
            "unexpected error message: {err}"
        );
        assert_eq!(
            doc.objects.len(),
            object_count,
            "a /Parent-walk Err must not add objects"
        );
    }

    #[test]
    fn test_ensure_font_repairs_malformed_direct_font_value() {
        // The direct-repair/indirect-error asymmetry (annotation.rs states
        // it for /Annots): a malformed direct /Font value cannot hold font
        // entries, so replacing it discards nothing — unlike an
        // unresolvable /Font reference, which is an Err.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        set_page_font(&mut doc, page_id, Object::Integer(7));

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("a malformed direct /Font value should be repaired");
        assert_eq!(font_name, b"F_PS_Courier");

        let fonts = page_font_dict(&doc, page_id);
        assert!(
            matches!(fonts.get(b"F_PS_Courier"), Ok(Object::Reference(_))),
            "repaired /Font dict should hold the new font reference"
        );
    }

    #[test]
    fn test_ensure_font_preserves_existing_direct_font_entries() {
        // Guards the d36e4b0f lesson in the new shape: a successful direct
        // insert must never trigger the indirect-failure path, which would
        // rebuild /Font and drop pre-existing entries.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let tr_id = add_type1_font(&mut doc, "Times-Roman");
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference(tr_id));
        set_page_font(&mut doc, page_id, Object::Dictionary(fonts));

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("registration into a direct /Font dict should succeed");
        assert_eq!(font_name, b"F_PS_Courier");

        let fonts = page_font_dict(&doc, page_id);
        assert!(
            fonts.get(b"F1").is_ok(),
            "pre-existing direct entry must survive registration"
        );
        assert!(
            fonts.get(b"F_PS_Courier").is_ok(),
            "new font should be added alongside"
        );
    }

    #[test]
    fn test_stamp_with_indirect_font_dict() {
        // Re-port of the old pdf-stamp binary's AMPHTT-769 regression test:
        // stamping a page whose /Font is an indirect reference must register
        // the stamp font through the reference — before the fix the stamp
        // rendered blank because /F_PS_Courier was never registered anywhere.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id = add_indirect_font_dict(&mut doc, "F1", "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));

        let json = r#"{"stamps": [{"text": "IndirectTest", "x": 50, "y": 50, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [0, 0, 0], "font": "Courier"}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let contents = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array after stamping, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {other:?}"),
        };
        assert!(
            content.contains("(IndirectTest) Tj"),
            "stamp text should be in the content stream, got: {content}"
        );
        assert!(
            content.contains("/F_PS_Courier 12 Tf"),
            "content stream should select the registered font, got: {content}"
        );

        // Both fonts resolve through the page's (still indirect) /Font.
        let resources = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Resources") {
                Ok(Object::Dictionary(ref r)) => r.clone(),
                other => panic!("expected direct /Resources, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        assert_eq!(
            resources.get(b"Font").unwrap(),
            &Object::Reference(fonts_id),
            "/Font should remain an indirect reference"
        );
        assert!(
            find_font_in_resources(&doc, &resources, "Courier").is_some(),
            "Courier should be findable through the reference"
        );
        assert!(
            find_font_in_resources(&doc, &resources, "Times-Roman").is_some(),
            "existing Times-Roman should be preserved"
        );
    }

    #[test]
    fn test_stamp_emits_escaped_font_name_for_non_utf8_key() {
        // End-to-end through the stamps loop: a Times-Roman entry keyed by
        // the non-UTF8 bytes lopdf's parser decodes from /#cb#ce#cc#e5 is
        // found and referenced byte-faithfully as /#CB#CE#CC#E5 in the Tf
        // operator. Pre-fix the lossy key emitted U+FFFD bytes that matched
        // no dictionary entry, so the stamp rendered blank.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id =
            add_indirect_font_dict(&mut doc, b"\xCB\xCE\xCC\xE5".as_slice(), "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));

        let json = r#"{"stamps": [{"text": "NonUtf8", "x": 50, "y": 50, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [0, 0, 0], "font": "Times-Roman"}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let contents = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array after stamping, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {other:?}"),
        };
        assert!(
            content.contains("/#CB#CE#CC#E5 12 Tf"),
            "the non-UTF8 key should be re-encoded byte-faithfully, got: {content}"
        );
        assert!(
            content.contains("(NonUtf8) Tj"),
            "stamp text should be in the content stream, got: {content}"
        );
        assert!(
            !content.contains('\u{FFFD}'),
            "no lossy replacement character may reach the output, got: {content}"
        );
    }

    #[test]
    fn test_stamp_emits_escaped_font_name_for_delimiter_key() {
        // End-to-end through the text_blocks loop (the second emission
        // site): a Courier entry keyed "F/1" — a delimiter that would have
        // desynced the token stream as /F/1 — is emitted as /F#2F1.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id = add_indirect_font_dict(&mut doc, b"F/1".as_slice(), "Courier");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));

        let json = r#"{"text_blocks": [{"text": "Wrapped", "x": 50, "y": 700, "width": 200, "size": 10, "font": "Courier", "line_spacing": 14}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let contents = match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array after stamping, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match doc.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {other:?}"),
        };
        assert!(
            content.contains("/F#2F1 10 Tf"),
            "the delimiter in the key should be #XX-escaped, got: {content}"
        );
        assert!(
            !content.contains("/F/1"),
            "the raw delimiter form must never reach the output, got: {content}"
        );
    }

    #[test]
    fn test_stamped_name_round_trips_through_save_and_reload() {
        // Pins that lopdf's writer (dictionary side) and encode_pdf_name
        // (content-stream side) agree through a full serialization cycle:
        // after save → reload, the /Font dictionary still holds the raw
        // non-UTF8 key and the stamp stream still references its #XX form.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        let fonts_id =
            add_indirect_font_dict(&mut doc, b"\xCB\xCE\xCC\xE5".as_slice(), "Times-Roman");
        set_page_font(&mut doc, page_id, Object::Reference(fonts_id));

        let json = r#"{"stamps": [{"text": "RoundTrip", "x": 50, "y": 50, "origin_x": "left", "origin_y": "bottom", "size": 12, "color": [0, 0, 0], "font": "Times-Roman"}]}"#;
        let config: StampConfig = serde_json::from_str(json).unwrap();
        apply_stamp_config(&mut doc, &config).expect("stamping should succeed");

        let mut buf = Vec::new();
        doc.save_to(&mut buf).expect("saving should succeed");
        let reloaded = Document::load_mem(&buf).expect("reloading should succeed");

        let page_id = *reloaded.get_pages().values().next().expect("one page");
        let resources = match reloaded.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Resources") {
                Ok(Object::Dictionary(ref r)) => r.clone(),
                other => panic!("expected direct /Resources, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        assert_eq!(
            find_font_in_resources(&reloaded, &resources, "Times-Roman").as_deref(),
            Some(b"\xCB\xCE\xCC\xE5".as_slice()),
            "the raw-byte key must survive the save/reload cycle"
        );

        let contents = match reloaded.get_object(page_id) {
            Ok(Object::Dictionary(ref d)) => match d.get(b"Contents") {
                Ok(Object::Array(ref a)) => a.clone(),
                other => panic!("expected /Contents array, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        };
        let stamp_ref = contents[1].as_reference().unwrap();
        let content = match reloaded.get_object(stamp_ref) {
            Ok(Object::Stream(ref s)) => String::from_utf8_lossy(&s.content).into_owned(),
            other => panic!("expected stamp stream, got {other:?}"),
        };
        assert!(
            content.contains("/#CB#CE#CC#E5 12 Tf"),
            "the reloaded stamp stream must still reference the #XX form, got: {content}"
        );
    }

    #[test]
    fn test_ensure_font_self_referential_font_ref_stays_reachable() {
        // Degenerate shape flagged in review: /Font referencing its own
        // containing object (here, the page itself). No guard is needed —
        // the write-through targets whatever object the reference
        // designates, which is by definition the dict a viewer resolves
        // /Font to, so the registered entry stays reachable.
        let (mut doc, page_ids) = build_test_pdf(1, Some(us_letter_box()));
        let page_id = page_ids[0];
        set_page_font(&mut doc, page_id, Object::Reference(page_id));

        let font_name = ensure_font(&mut doc, page_id, "Courier")
            .expect("a self-referential /Font should still register");
        assert_eq!(font_name, b"F_PS_Courier");

        // Reachability the way a viewer resolves it: page /Resources ->
        // /Font (a reference back at the page) -> entry present.
        match doc.get_object(page_id) {
            Ok(Object::Dictionary(ref page_dict)) => match page_dict.get(b"Resources") {
                Ok(Object::Dictionary(ref resources)) => assert!(
                    find_font_in_resources(&doc, resources, "Courier").is_some(),
                    "the registered font must resolve through the self-referential /Font"
                ),
                other => panic!("expected direct /Resources, got {other:?}"),
            },
            other => panic!("expected page dictionary, got {other:?}"),
        }
    }
}
