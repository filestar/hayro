/*!
A crate for converting PDF pages into either `XObjects` or a new page via [`pdf-writer`](https://docs.rs/pdf-writer/).

This is an internal crate and not meant for external use. Therefore, it's not very
well-documented.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod primitive;

use crate::primitive::{WriteDirect, WriteIndirect};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use hayro_syntax::object::ObjRef;
use hayro_syntax::object::Object;
use hayro_syntax::object::dict::keys::{
    ANNOTS, AP, ART_BOX, AS, BBOX, BLEED_BOX, COLORSPACE, EXT_G_STATE, FONT, GROUP, MATRIX,
    PATTERN, PROPERTIES, RECT, SHADING, TRIM_BOX, USER_UNIT, XOBJECT,
};
use hayro_syntax::object::{Array, Dict, MaybeRef, Stream};
use hayro_syntax::page::{Page, Resources, Rotation};
use pdf_writer::{Chunk, Content, Filter, Finish, Name, Rect, Ref};
use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;

pub use hayro_syntax;
use hayro_syntax::Pdf;
pub use pdf_writer;
pub use pdf_writer::Settings as ChunkSettings;

/// Apply the extraction queries to the given PDF and return the results.
pub fn extract<'a>(
    pdf: &Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    chunk_settings: ChunkSettings,
    queries: &[ExtractionQuery],
) -> Result<ExtractionResult, ExtractionError> {
    extract_with(
        pdf,
        new_ref,
        chunk_settings,
        queries,
        ExtractionOptions::default(),
    )
}

/// What a copy keeps beyond what embedding a page as an `XObject` needs. The default keeps nothing more, as
/// [`extract`] does.
#[derive(Clone, Debug, Default)]
pub struct ExtractionOptions {
    /// Keep `/OC` entries (the optional content an `XObject` or annotation belongs to). Without them, and without the
    /// catalog's `OCProperties`, content in a layer that is off is drawn. Copy `OCProperties` with
    /// `catalog_keys` alongside.
    pub keep_optional_content: bool,
    /// Write a copied page's `TrimBox`, `BleedBox`, `ArtBox` and `UserUnit` as well as its media and crop box.
    pub keep_page_boxes: bool,
    /// Catalog entries to copy (for example `OCProperties`, `OutputIntents`), each as an indirect object of its
    /// own; their references are in [`ExtractionResult::catalog_entries`], in this order, for the ones that exist.
    pub catalog_keys: Vec<&'static [u8]>,
    /// Draw a copied page's annotations into its content, as a viewer shows them: each annotation's normal
    /// appearance (the state `AS` names) placed on its `Rect` as PDF 2.0 12.5.5 describes, hidden and no-view ones
    /// left out. The annotations themselves are not copied, so a page that is not flattened loses them.
    pub flatten_annotations: bool,
}

/// [`extract`] with options: what a whole-document copy needs, such as optional content and the page boxes.
pub fn extract_with<'a>(
    pdf: &Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    chunk_settings: ChunkSettings,
    queries: &[ExtractionQuery],
    options: ExtractionOptions,
) -> Result<ExtractionResult, ExtractionError> {
    let pages = pdf.pages();
    let mut ctx = ExtractionContext::new(new_ref, pdf, chunk_settings);
    ctx.keep_optional_content = options.keep_optional_content;
    ctx.keep_page_boxes = options.keep_page_boxes;
    ctx.flatten_annotations = options.flatten_annotations;

    for query in queries {
        let page = pages
            .get(query.page_index)
            .ok_or(ExtractionError::InvalidPageIndex(query.page_index))?;

        let root_ref = ctx.new_ref();

        let res = match query.query_type {
            ExtractionQueryType::XObject => write_xobject(page, root_ref, &mut ctx),
            ExtractionQueryType::Page => write_page(page, root_ref, query.page_index, &mut ctx),
        };

        ctx.root_refs.push(res.map(|_| root_ref));
    }

    let mut catalog_entries = Vec::new();
    if !options.catalog_keys.is_empty()
        && let Some(catalog) = pdf.xref().get::<Dict<'_>>(pdf.xref().root_id())
    {
        for key in &options.catalog_keys {
            let Some(value) = catalog.get_raw::<Object<'_>>(key) else {
                continue;
            };
            let entry_ref = match value {
                MaybeRef::Ref(r) => {
                    if pdf.xref().get::<Object<'_>>(r.into()).is_none() {
                        continue;
                    }
                    ctx.to_visit_refs.push(r);
                    ctx.map_ref(r)
                }
                MaybeRef::NotRef(Object::Stream(_)) => continue,
                MaybeRef::NotRef(o) => {
                    let r = ctx.new_ref();
                    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
                    o.write_direct(chunk.indirect(r), &mut ctx);
                    ctx.chunks.push(chunk);
                    r
                }
            };
            catalog_entries.push((key.to_vec(), entry_ref));
        }
    }

    // Now we have shallowly extracted all pages, now go through all dependencies until there aren't
    // any anymore.
    write_dependencies(pdf, &mut ctx);

    let mut global_chunk = Chunk::with_settings(chunk_settings);

    for chunk in &ctx.chunks {
        global_chunk.extend(chunk);
    }

    Ok(ExtractionResult {
        chunk: global_chunk,
        root_refs: ctx.root_refs,
        page_tree_parent_ref: ctx.page_tree_parent_ref,
        catalog_entries,
    })
}

/// A type of extraction query, indicating as what kind of
/// object you want to extract the page.
#[derive(Copy, Clone, Debug)]
pub enum ExtractionQueryType {
    /// Extract the page as an `XObject`.
    XObject,
    /// Extract the page as a new page.
    Page,
}

/// An extraction query.
#[derive(Copy, Clone, Debug)]
pub struct ExtractionQuery {
    query_type: ExtractionQueryType,
    page_index: usize,
}

impl ExtractionQuery {
    /// Create a new page extraction query with the given page index.
    pub fn new_page(page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::Page,
            page_index,
        }
    }

    /// Create a new `XObject` extraction query with the given page index.
    pub fn new_xobject(page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::XObject,
            page_index,
        }
    }
}

/// An error that occurred during page extraction.
#[derive(Debug, Copy, Clone)]
pub enum ExtractionError {
    /// An invalid page index was given.
    InvalidPageIndex(usize),
}

/// The result of an extraction.
#[derive(Debug)]
pub struct ExtractionResult {
    /// The chunk containing all objects as well as their dependencies.
    pub chunk: Chunk,
    /// The root references of the pages/XObject, one for each extraction query.
    pub root_refs: Vec<Result<Ref, ExtractionError>>,
    /// The reference to the page tree parent that was generated.
    pub page_tree_parent_ref: Ref,
    /// The catalog entries asked for in [`ExtractionOptions::catalog_keys`] that exist: key and reference.
    pub catalog_entries: Vec<(Vec<u8>, Ref)>,
}

struct ExtractionContext<'a> {
    chunks: Vec<Chunk>,
    visited_objects: HashSet<ObjRef>,
    to_visit_refs: Vec<ObjRef>,
    valid_ref_cache: FxHashMap<ObjRef, bool>,
    root_refs: Vec<Result<Ref, ExtractionError>>,
    pdf: &'a Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    ref_map: FxHashMap<ObjRef, Ref>,
    cached_content_streams: FxHashMap<usize, Ref>,
    page_tree_parent_ref: Ref,
    chunk_settings: ChunkSettings,
    pub(crate) keep_optional_content: bool,
    keep_page_boxes: bool,
    flatten_annotations: bool,
}

impl<'a> ExtractionContext<'a> {
    fn new(
        mut new_ref: Box<dyn FnMut() -> Ref + 'a>,
        pdf: &'a Pdf,
        chunk_settings: ChunkSettings,
    ) -> Self {
        let page_tree_parent_ref = new_ref();
        Self {
            chunks: vec![],
            visited_objects: HashSet::new(),
            to_visit_refs: Vec::new(),
            valid_ref_cache: FxHashMap::default(),
            pdf,
            new_ref,
            ref_map: FxHashMap::default(),
            cached_content_streams: FxHashMap::default(),
            root_refs: Vec::new(),
            page_tree_parent_ref,
            chunk_settings,
            keep_optional_content: false,
            keep_page_boxes: false,
            flatten_annotations: false,
        }
    }

    pub(crate) fn map_ref(&mut self, ref_: ObjRef) -> Ref {
        if let Some(ref_) = self.ref_map.get(&ref_) {
            *ref_
        } else {
            let new_ref = self.new_ref();
            self.ref_map.insert(ref_, new_ref);

            new_ref
        }
    }

    pub(crate) fn new_ref(&mut self) -> Ref {
        (self.new_ref)()
    }
}

fn write_dependencies(pdf: &Pdf, ctx: &mut ExtractionContext<'_>) {
    while let Some(ref_) = ctx.to_visit_refs.pop() {
        // Don't visit objects twice!
        if ctx.visited_objects.contains(&ref_) {
            continue;
        }

        let mut chunk = Chunk::with_settings(ctx.chunk_settings);
        if let Some(object) = pdf.xref().get::<Object<'_>>(ref_.into()) {
            let new_ref = ctx.map_ref(ref_);
            object.write_indirect(&mut chunk, new_ref, ctx);
            ctx.chunks.push(chunk);

            ctx.visited_objects.insert(ref_);
        }
    }
}

/// Extract the given pages from the PDF and resave them as a new PDF. This function shouldn't be
/// used directly and only exists for test purposes.
#[doc(hidden)]
pub fn extract_pages_to_pdf(hayro_pdf: &Pdf, page_indices: &[usize]) -> Vec<u8> {
    let mut pdf = pdf_writer::Pdf::new();
    let mut next_ref = Ref::new(1);
    let requests = page_indices
        .iter()
        .map(|i| ExtractionQuery {
            query_type: ExtractionQueryType::Page,
            page_index: *i,
        })
        .collect::<Vec<_>>();

    let catalog_id = next_ref.bump();

    let extracted = extract(
        hayro_pdf,
        Box::new(|| next_ref.bump()),
        ChunkSettings::default(),
        &requests,
    )
    .unwrap();
    pdf.catalog(catalog_id)
        .pages(extracted.page_tree_parent_ref);
    let count = extracted.root_refs.len();
    pdf.pages(extracted.page_tree_parent_ref)
        .kids(extracted.root_refs.iter().map(|r| r.unwrap()))
        .count(count as i32);
    pdf.extend(&extracted.chunk);

    pdf.finish()
}

/// Extract the given pages as XObjects from the PDF and resave them as a new PDF.
/// This function shouldn't be used directly and only exists for test purposes.
#[doc(hidden)]
pub fn extract_pages_as_xobject_to_pdf(hayro_pdf: &Pdf, page_indices: &[usize]) -> Vec<u8> {
    let hayro_pages = hayro_pdf.pages();
    let page_list = hayro_pages.as_ref();

    let mut pdf = pdf_writer::Pdf::new();
    let mut next_ref = Ref::new(1);

    let catalog_id = next_ref.bump();
    let requests = page_indices
        .iter()
        .map(|i| ExtractionQuery {
            query_type: ExtractionQueryType::XObject,
            page_index: *i,
        })
        .collect::<Vec<_>>();

    let extracted = extract(
        hayro_pdf,
        Box::new(|| next_ref.bump()),
        ChunkSettings::default(),
        &requests,
    )
    .unwrap();

    pdf.catalog(catalog_id)
        .pages(extracted.page_tree_parent_ref);
    let mut page_refs = vec![];

    for (x_object_ref, page_idx) in extracted.root_refs.iter().zip(page_indices) {
        let page = &page_list[*page_idx];
        let render_dimensions = page.render_dimensions();

        let mut content = Content::new();
        content.x_object(Name(b"O1"));

        let finished = content.finish();

        let page_id = next_ref.bump();
        let stream_id = next_ref.bump();
        page_refs.push(page_id);

        let mut page = pdf.page(page_id);
        page.resources()
            .x_objects()
            .pair(Name(b"O1"), x_object_ref.unwrap());
        page.media_box(Rect::new(
            0.0,
            0.0,
            render_dimensions.0,
            render_dimensions.1,
        ));
        page.parent(extracted.page_tree_parent_ref);
        page.contents(stream_id);
        page.finish();

        pdf.stream(stream_id, finished.as_slice());
    }

    let count = extracted.root_refs.len();
    pdf.pages(extracted.page_tree_parent_ref)
        .kids(page_refs)
        .count(count as i32);
    pdf.extend(&extracted.chunk);

    pdf.finish()
}

fn write_page(
    page: &Page<'_>,
    page_ref: Ref,
    page_idx: usize,
    ctx: &mut ExtractionContext<'_>,
) -> Result<(), ExtractionError> {
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let annotations = if ctx.flatten_annotations {
        annotation_appearances(page, ctx)
    } else {
        Vec::new()
    };
    // Note: We can cache content stream references, but _not_ the page references themselves.
    // Acrobat for some reason doesn't like duplicate page references in the page tree.
    let stream_ref = if let Some(cached) = ctx.cached_content_streams.get(&page_idx) {
        *cached
    } else {
        let stream_ref = ctx.new_ref();
        let content = page.page_stream().unwrap_or(b"");
        let content = if annotations.is_empty() {
            deflate_encode(content)
        } else {
            // The page's own content in a q/Q pair, so a state it leaves set does not move the annotations.
            let mut c = Vec::with_capacity(content.len() + 64 * annotations.len());
            c.extend_from_slice(
                b"q
",
            );
            c.extend_from_slice(content);
            c.extend_from_slice(
                b"
Q
",
            );
            for (name, _, m) in &annotations {
                c.extend_from_slice(
                    format!(
                        "q {} {} {} {} {} {} cm /",
                        m[0], m[1], m[2], m[3], m[4], m[5]
                    )
                    .as_bytes(),
                );
                c.extend_from_slice(name);
                c.extend_from_slice(
                    b" Do Q
",
                );
            }
            deflate_encode(&c)
        };

        chunk
            .stream(stream_ref, &content)
            .filter(Filter::FlateDecode);
        ctx.cached_content_streams.insert(page_idx, stream_ref);

        stream_ref
    };

    let mut pdf_page = chunk.page(page_ref);

    pdf_page
        .media_box(convert_rect(&page.media_box()))
        .crop_box(convert_rect(&page.crop_box()))
        .rotate(match page.rotation() {
            Rotation::None => 0,
            Rotation::Horizontal => 90,
            Rotation::Flipped => 180,
            Rotation::FlippedHorizontal => 270,
        })
        .parent(ctx.page_tree_parent_ref)
        .contents(stream_ref);

    let raw_dict = page.raw();

    if let Some(group) = raw_dict.get_raw::<Object<'_>>(GROUP) {
        group.write_direct(pdf_page.insert(Name(GROUP)), ctx);
    }

    if ctx.keep_page_boxes {
        // Not inheritable (unlike the media and crop box), so the page's own entries are all there is.
        for key in [TRIM_BOX, BLEED_BOX, ART_BOX, USER_UNIT] {
            if let Some(value) = raw_dict.get_raw::<Object<'_>>(key) {
                value.write_direct(pdf_page.insert(Name(key)), ctx);
            }
        }
    }

    let extra: Vec<(Vec<u8>, Ref)> = annotations.into_iter().map(|(n, r, _)| (n, r)).collect();
    serialize_resources(page.resources(), ctx, &mut pdf_page, &extra);

    pdf_page.finish();

    ctx.chunks.push(chunk);

    Ok(())
}

/// The annotations to draw on a flattened page: a name for each appearance stream (its `XObject` resource), the
/// stream's new reference, and the matrix A that maps its transformed bounding box onto the annotation's `Rect`
/// (PDF 2.0 12.5.5; the stream's own `Matrix` is applied by `Do`).
fn annotation_appearances(
    page: &Page<'_>,
    ctx: &mut ExtractionContext<'_>,
) -> Vec<(Vec<u8>, Ref, [f64; 6])> {
    const HIDDEN: u32 = 1 << 1;
    const NO_VIEW: u32 = 1 << 5;
    let mut out = Vec::new();
    let Some(annots) = page.raw().get::<Array<'_>>(ANNOTS) else {
        return out;
    };
    for annot in annots.iter::<Dict<'_>>() {
        if annot.get::<u32>(b"F").unwrap_or(0) & (HIDDEN | NO_VIEW) != 0 {
            continue;
        }
        let Some(ap) = annot.get::<Dict<'_>>(AP) else {
            continue;
        };
        let raw = match ap.get::<Object<'_>>(b"N") {
            Some(Object::Stream(_)) => ap.get_raw::<Object<'_>>(b"N"),
            Some(Object::Dict(states)) => annot
                .get::<hayro_syntax::object::Name<'_>>(AS)
                .and_then(|state| states.get_raw::<Object<'_>>(state.deref()))
                .or_else(|| states.get_raw::<Object<'_>>(b"Off")),
            _ => None,
        };
        let Some(MaybeRef::Ref(r)) = raw else {
            continue;
        };
        let Some(stream) = ctx.pdf.xref().get::<Stream<'_>>(r.into()) else {
            continue;
        };
        let d = stream.dict();
        let (Some(bbox), Some(rect)) = (d.get::<[f32; 4]>(BBOX), annot.get::<[f32; 4]>(RECT))
        else {
            continue;
        };
        let m = d
            .get::<[f64; 6]>(MATRIX)
            .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let corners = [
            (bbox[0], bbox[1]),
            (bbox[2], bbox[1]),
            (bbox[0], bbox[3]),
            (bbox[2], bbox[3]),
        ]
        .map(|(x, y)| (f64::from(x), f64::from(y)))
        .map(|(x, y)| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]));
        let (tx0, tx1) = corners
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (ty0, ty1) = corners
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        let (rx0, rx1) = (
            f64::from(rect[0].min(rect[2])),
            f64::from(rect[0].max(rect[2])),
        );
        let (ry0, ry1) = (
            f64::from(rect[1].min(rect[3])),
            f64::from(rect[1].max(rect[3])),
        );
        let (w, h) = (tx1 - tx0, ty1 - ty0);
        if !(w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
            continue;
        }
        let (sx, sy) = ((rx1 - rx0) / w, (ry1 - ry0) / h);
        let a = [sx, 0.0, 0.0, sy, rx0 - sx * tx0, ry0 - sy * ty0];
        ctx.to_visit_refs.push(r);
        let mapped = ctx.map_ref(r);
        out.push((format!("HayroAnnot{}", out.len()).into_bytes(), mapped, a));
    }
    out
}

fn write_xobject(
    page: &Page<'_>,
    xobj_ref: Ref,
    ctx: &mut ExtractionContext<'_>,
) -> Result<(), ExtractionError> {
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let encoded_stream = deflate_encode(page.page_stream().unwrap_or(b""));
    let mut x_object = chunk.form_xobject(xobj_ref, &encoded_stream);
    x_object.deref_mut().filter(Filter::FlateDecode);

    let bbox = page.crop_box();
    let initial_transform = page.initial_transform(false);

    x_object.bbox(Rect::new(
        bbox.x0 as f32,
        bbox.y0 as f32,
        bbox.x1 as f32,
        bbox.y1 as f32,
    ));

    let i = initial_transform.as_coeffs();
    x_object.matrix([
        i[0] as f32,
        i[1] as f32,
        i[2] as f32,
        i[3] as f32,
        i[4] as f32,
        i[5] as f32,
    ]);

    serialize_resources(page.resources(), ctx, &mut x_object, &[]);

    let raw_dict = page.raw();

    if let Some(group) = raw_dict.get_raw::<Object<'_>>(GROUP) {
        group.write_direct(x_object.insert(Name(GROUP)), ctx);
    }

    x_object.finish();
    ctx.chunks.push(chunk);

    Ok(())
}

fn serialize_resources(
    resources: &Resources<'_>,
    ctx: &mut ExtractionContext<'_>,
    writer: &mut impl ResourcesExt,
    extra_x_objects: &[(Vec<u8>, Ref)],
) {
    // Resource dictionary is always required (unless it can be inherited), so
    // let's just be safe and always write it.
    let mut output = writer.resources();

    macro_rules! write {
        ($name:ident, $key:expr) => {
            if !resources.$name.is_empty() {
                let mut dict = output.insert(Name($key)).dict();

                for (name, obj) in resources.$name.entries() {
                    obj.write_direct(dict.insert(Name(name.deref())), ctx);
                }
            }
        };
    }

    write!(ext_g_states, EXT_G_STATE);
    write!(shadings, SHADING);
    write!(patterns, PATTERN);
    if !resources.x_objects.is_empty() || !extra_x_objects.is_empty() {
        let mut dict = output.insert(Name(XOBJECT)).dict();
        for (name, obj) in resources.x_objects.entries() {
            obj.write_direct(dict.insert(Name(name.deref())), ctx);
        }
        for (name, r) in extra_x_objects {
            dict.pair(Name(name), *r);
        }
    }
    write!(color_spaces, COLORSPACE);
    write!(fonts, FONT);
    write!(properties, PROPERTIES);
}

pub(crate) fn deflate_encode(data: &[u8]) -> Vec<u8> {
    use std::io::Write;

    const COMPRESSION_LEVEL: u8 = 6;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL as u32));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn convert_rect(hy_rect: &hayro_syntax::object::Rect) -> Rect {
    Rect::new(
        hy_rect.x0 as f32,
        hy_rect.y0 as f32,
        hy_rect.x1 as f32,
        hy_rect.y1 as f32,
    )
}

trait ResourcesExt {
    fn resources(&mut self) -> pdf_writer::writers::Resources<'_>;
}

impl ResourcesExt for pdf_writer::writers::Page<'_> {
    fn resources(&mut self) -> pdf_writer::writers::Resources<'_> {
        Self::resources(self)
    }
}

impl ResourcesExt for pdf_writer::writers::FormXObject<'_> {
    fn resources(&mut self) -> pdf_writer::writers::Resources<'_> {
        Self::resources(self)
    }
}
