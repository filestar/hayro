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
use hayro_syntax::content::UntypedIter;
use hayro_syntax::object::ObjRef;
use hayro_syntax::object::Object;
use hayro_syntax::object::dict::keys::{
    ANNOTS, AP, ART_BOX, AS, BBOX, BLEED_BOX, COLORSPACE, CONTENTS, EXT_G_STATE, F, FONT, GROUP,
    MATRIX, N, OC, PATTERN, PROPERTIES, RECT, SHADING, TRIM_BOX, USER_UNIT, XOBJECT,
};
use hayro_syntax::object::{Array, Dict, MaybeRef, Stream};
use hayro_syntax::page::{Page, Resources, Rotation};
use pdf_writer::types::BlendMode;
use pdf_writer::writers::ExtGraphicsState;
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
    /// left out. An annotation without an appearance is drawn as viewers draw it (as hayro-interpret renders it),
    /// one in a layer stays in its layer (with `keep_optional_content`), and they are drawn in the state the page
    /// started in, whatever its content leaves open. The annotations themselves are not copied, so a page that is
    /// not flattened loses them.
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
        raw_content_pages: ctx.raw_content_pages,
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
    /// The pages (indices into the source) whose content did not decode and was copied as it is, with its
    /// filters: a reader that can decode it draws it, one that cannot draws nothing there, as with the source.
    pub raw_content_pages: Vec<usize>,
}

/// A page's content streams, and the form (resource name, reference) its content went into when it had to.
type CachedContent = (Vec<Ref>, Option<(Vec<u8>, Ref)>);

struct ExtractionContext<'a> {
    chunks: Vec<Chunk>,
    visited_objects: HashSet<ObjRef>,
    to_visit_refs: Vec<ObjRef>,
    valid_ref_cache: FxHashMap<ObjRef, bool>,
    root_refs: Vec<Result<Ref, ExtractionError>>,
    pdf: &'a Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    ref_map: FxHashMap<ObjRef, Ref>,
    /// A page's content streams, and the form its content went into when it had to (see `write_contents`).
    cached_content_streams: FxHashMap<usize, CachedContent>,
    page_tree_parent_ref: Ref,
    chunk_settings: ChunkSettings,
    pub(crate) keep_optional_content: bool,
    keep_page_boxes: bool,
    flatten_annotations: bool,
    /// Streams placed with `Do` that are written as form `XObjects`, `Subtype` or not.
    form_refs: HashSet<ObjRef>,
    /// Resource names this copy gave the page being written.
    page_names: HashSet<Vec<u8>>,
    /// The form the page's content went into, when it had to.
    page_form: Option<(Vec<u8>, Ref)>,
    raw_content_pages: Vec<usize>,
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
            form_refs: HashSet::new(),
            page_names: HashSet::new(),
            page_form: None,
            raw_content_pages: Vec::new(),
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
            match &object {
                Object::Stream(stream) if ctx.form_refs.contains(&ref_) => {
                    primitive::write_indirect_as_form(stream, &mut chunk, new_ref, ctx);
                }
                _ => object.write_indirect(&mut chunk, new_ref, ctx),
            }
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
    ctx.page_names.clear();
    ctx.page_form = None;
    let flattened = if ctx.flatten_annotations {
        flatten_annotations(page, ctx)
    } else {
        Flattened::default()
    };
    // Note: We can cache content stream references, but _not_ the page references themselves.
    // Acrobat for some reason doesn't like duplicate page references in the page tree.
    let contents = if let Some((cached, form)) = ctx.cached_content_streams.get(&page_idx) {
        ctx.page_form = form.clone();
        cached.clone()
    } else {
        let contents = write_contents(page, page_idx, &flattened, &mut chunk, ctx);
        ctx.cached_content_streams
            .insert(page_idx, (contents.clone(), ctx.page_form.clone()));
        contents
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
        .parent(ctx.page_tree_parent_ref);
    match contents.as_slice() {
        [one] => {
            pdf_page.contents(*one);
        }
        many => {
            pdf_page.contents_array(many.iter().copied());
        }
    }

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

    let mut extra = flattened.resources();
    if let Some((name, r)) = ctx.page_form.take() {
        extra.x_objects.push((name, r));
    }
    serialize_resources(page.resources(), ctx, &mut pdf_page, &extra);

    pdf_page.finish();

    ctx.chunks.push(chunk);

    Ok(())
}

/// The content of a copied page: its own content, then the flattened annotations drawn in the state the page
/// started in. Content that does not decode is copied as it is (its streams, with their filters), so nothing is
/// lost for a reader that can decode it; the page is listed in [`ExtractionResult::raw_content_pages`].
fn write_contents(
    page: &Page<'_>,
    page_idx: usize,
    flattened: &Flattened,
    chunk: &mut Chunk,
    ctx: &mut ExtractionContext<'_>,
) -> Vec<Ref> {
    let annotations = flattened.content();
    if !page.content_decodes() {
        ctx.raw_content_pages.push(page_idx);
        let raw = raw_content_refs(page, ctx);
        let mut refs = Vec::with_capacity(raw.len() + 2);
        if !annotations.is_empty() {
            let r = ctx.new_ref();
            chunk.stream(r, b"q\n");
            refs.push(r);
        }
        for r in raw {
            ctx.to_visit_refs.push(r);
            refs.push(ctx.map_ref(r));
        }
        if !annotations.is_empty() || refs.is_empty() {
            // What the content leaves open cannot be read here, so only the q above is closed.
            let mut c = Vec::with_capacity(annotations.len() + 4);
            if !annotations.is_empty() {
                c.extend_from_slice(b"\nQ\n");
                c.extend_from_slice(&annotations);
            }
            let r = ctx.new_ref();
            chunk
                .stream(r, &deflate_encode(&c))
                .filter(Filter::FlateDecode);
            refs.push(r);
        }
        return refs;
    }

    let content = page.page_stream().unwrap_or(b"");
    let encoded = if annotations.is_empty() {
        deflate_encode(content)
    } else {
        let open = open_state(content);
        let mut c = Vec::with_capacity(content.len() + annotations.len() + 64);
        if open.underflow > 0 {
            // A Q with nothing to restore would end the q this copy puts around the content, and the
            // annotations would inherit its state; readers ignore such a Q. Only a form XObject reproduces that:
            // the content goes into one, and `Do` restores the state whatever the content does.
            let form = ctx.new_ref();
            let encoded = deflate_encode(content);
            let mut x = chunk.form_xobject(form, &encoded);
            x.deref_mut().filter(Filter::FlateDecode);
            x.bbox(convert_rect(&page.media_box()));
            serialize_resources(page.resources(), ctx, &mut x, &ExtraResources::default());
            x.finish();
            let name = ctx.unique_page_name(page, b"HayroPage");
            c.extend_from_slice(b"/");
            c.extend_from_slice(&name);
            c.extend_from_slice(b" Do\n");
            ctx.page_form = Some((name, form));
        } else {
            // The page's own content in a q/Q pair, with whatever it leaves open closed first, so a state it
            // leaves set (a cm, a clip, an open text object or marked-content sequence) does not move the
            // annotations.
            c.extend_from_slice(b"q\n");
            c.extend_from_slice(content);
            c.extend_from_slice(b"\n");
            if open.path {
                c.extend_from_slice(b"n\n");
            }
            if open.text {
                c.extend_from_slice(b"ET\n");
            }
            for _ in 0..open.marked {
                c.extend_from_slice(b"EMC\n");
            }
            for _ in 0..open.saves {
                c.extend_from_slice(b"Q\n");
            }
            c.extend_from_slice(b"Q\n");
        }
        c.extend_from_slice(&annotations);
        deflate_encode(&c)
    };
    let r = ctx.new_ref();
    chunk.stream(r, &encoded).filter(Filter::FlateDecode);
    vec![r]
}

/// The page's own content streams, by reference, as its `Contents` names them.
fn raw_content_refs(page: &Page<'_>, ctx: &ExtractionContext<'_>) -> Vec<ObjRef> {
    fn push_items(array: &Array<'_>, out: &mut Vec<ObjRef>) {
        for item in array.raw_iter() {
            if let MaybeRef::Ref(r) = item {
                out.push(r);
            }
        }
    }
    let mut out = Vec::new();
    match page.raw().get_raw::<Object<'_>>(CONTENTS) {
        Some(MaybeRef::Ref(r)) => match ctx.pdf.xref().get::<Object<'_>>(r.into()) {
            Some(Object::Stream(_)) => out.push(r),
            Some(Object::Array(array)) => push_items(&array, &mut out),
            _ => {}
        },
        Some(MaybeRef::NotRef(Object::Array(array))) => push_items(&array, &mut out),
        _ => {}
    }
    out
}

/// What a content stream leaves open at its end.
#[derive(Debug, Default, PartialEq, Eq)]
struct OpenState {
    /// `q` without their `Q`.
    saves: usize,
    /// `Q` with no `q` to restore (readers ignore them).
    underflow: usize,
    /// `BMC`/`BDC` without their `EMC`.
    marked: usize,
    /// Inside `BT` … `ET`.
    text: bool,
    /// A path is being built (no painting operator yet).
    path: bool,
}

fn open_state(content: &[u8]) -> OpenState {
    let mut state = OpenState::default();
    let mut ops = UntypedIter::new(content);
    while let Some(op) = ops.next() {
        match op.operator.as_ref() {
            b"q" => state.saves += 1,
            b"Q" => {
                if state.saves > 0 {
                    state.saves -= 1;
                } else {
                    state.underflow += 1;
                }
            }
            b"BMC" | b"BDC" => state.marked += 1,
            b"EMC" => state.marked = state.marked.saturating_sub(1),
            b"BT" => state.text = true,
            b"ET" => state.text = false,
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => state.path = true,
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                state.path = false;
            }
            _ => {}
        }
    }
    state
}

/// Resources a copy adds to a page's own: `XObject` and `Properties` entries.
#[derive(Default)]
struct ExtraResources {
    x_objects: Vec<(Vec<u8>, Ref)>,
    properties: Vec<(Vec<u8>, Ref)>,
}

/// A page's annotations as they are drawn into it.
#[derive(Default)]
struct Flattened {
    /// One per annotation drawn, in the order of `Annots`.
    items: Vec<FlatAnnotation>,
}

struct FlatAnnotation {
    /// The `XObject` resource name and its reference.
    name: Vec<u8>,
    xobject: Ref,
    /// The matrix `Do` is placed with.
    matrix: [f32; 6],
    /// The `Properties` resource name and reference of the annotation's optional content (`OC`).
    oc: Option<(Vec<u8>, Ref)>,
}

impl Flattened {
    /// The operators that draw the annotations, each in its own q/Q, and in its layer (`/OC … BDC`).
    fn content(&self) -> Vec<u8> {
        if self.items.is_empty() {
            return Vec::new();
        }
        let mut c = Content::new();
        for a in &self.items {
            if let Some((name, _)) = &a.oc {
                c.begin_marked_content_with_properties(Name(b"OC"))
                    .properties_named(Name(name));
            }
            c.save_state();
            if a.matrix != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] {
                c.transform(a.matrix);
            }
            c.x_object(Name(&a.name));
            c.restore_state();
            if a.oc.is_some() {
                c.end_marked_content();
            }
        }
        c.finish().into_vec()
    }

    fn resources(&self) -> ExtraResources {
        ExtraResources {
            x_objects: self
                .items
                .iter()
                .map(|a| (a.name.clone(), a.xobject))
                .collect(),
            properties: self.items.iter().filter_map(|a| a.oc.clone()).collect(),
        }
    }
}

impl ExtractionContext<'_> {
    /// A resource name `{prefix}{n}` that neither the page's own resources nor this copy use yet.
    fn unique_page_name(&mut self, page: &Page<'_>, prefix: &[u8]) -> Vec<u8> {
        let resources = page.resources();
        let mut n = 0_usize;
        loop {
            let mut name = prefix.to_vec();
            name.extend_from_slice(n.to_string().as_bytes());
            n += 1;
            if !resources.x_objects.contains_key(name.as_slice())
                && !resources.properties.contains_key(name.as_slice())
                && !self.page_names.contains(&name)
            {
                self.page_names.insert(name.clone());
                return name;
            }
        }
    }
}

/// The annotations to draw on a flattened page (PDF 2.0 12.5.5): each annotation's normal appearance (the state
/// `AS` names), as an `XObject` with the matrix A that maps its transformed bounding box onto the annotation's
/// `Rect` (the stream's own `Matrix` is applied by `Do`). An annotation without `AP` is drawn as viewers draw it
/// (`hayro_syntax::annotation::synthesise`, as hayro-interpret renders it). Hidden and no-view annotations are
/// left out, and one in a layer keeps its layer (`OC`) when optional content is kept.
fn flatten_annotations(page: &Page<'_>, ctx: &mut ExtractionContext<'_>) -> Flattened {
    const HIDDEN: u32 = 1 << 1;
    const NO_VIEW: u32 = 1 << 5;
    let mut out = Flattened::default();
    let Some(annots) = page.raw().get::<Array<'_>>(ANNOTS) else {
        return out;
    };
    for annot in annots.iter::<Dict<'_>>() {
        if annot.get::<u32>(F).unwrap_or(0) & (HIDDEN | NO_VIEW) != 0 {
            continue;
        }
        let placed = if annot.contains_key(AP) {
            appearance(&annot, ctx)
        } else {
            synthesised(&annot, page, ctx)
        };
        let Some((xobject, matrix)) = placed else {
            continue;
        };
        let name = ctx.unique_page_name(page, b"HayroAnnot");
        let oc = if ctx.keep_optional_content {
            annotation_oc(&annot, ctx).map(|r| (ctx.unique_page_name(page, b"HayroOC"), r))
        } else {
            None
        };
        out.items.push(FlatAnnotation {
            name,
            xobject,
            matrix,
            oc,
        });
    }
    out
}

/// The annotation's normal appearance stream, written as a form `XObject`, and the matrix that places it.
fn appearance(annot: &Dict<'_>, ctx: &mut ExtractionContext<'_>) -> Option<(Ref, [f32; 6])> {
    let ap = annot.get::<Dict<'_>>(AP)?;
    let raw = match ap.get::<Object<'_>>(N) {
        Some(Object::Stream(_)) => ap.get_raw::<Object<'_>>(N),
        Some(Object::Dict(states)) => annot
            .get::<hayro_syntax::object::Name<'_>>(AS)
            .and_then(|state| states.get_raw::<Object<'_>>(state.deref()))
            .or_else(|| states.get_raw::<Object<'_>>(b"Off")),
        _ => None,
    };
    let Some(MaybeRef::Ref(r)) = raw else {
        return None;
    };
    let stream = ctx.pdf.xref().get::<Stream<'_>>(r.into())?;
    let d = stream.dict();
    let (bbox, rect) = (d.get::<[f32; 4]>(BBOX)?, annot.get::<[f32; 4]>(RECT)?);
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
        return None;
    }
    let (sx, sy) = ((rx1 - rx0) / w, (ry1 - ry0) / h);
    let a = [sx, 0.0, 0.0, sy, rx0 - sx * tx0, ry0 - sy * ty0].map(|v| v as f32);
    if !a.iter().all(|v| v.is_finite()) {
        return None;
    }
    // Placed with Do, so it must be a form XObject even when its writer left `Subtype` out (viewers draw an
    // annotation's appearance stream either way).
    ctx.form_refs.insert(r);
    ctx.to_visit_refs.push(r);
    Some((ctx.map_ref(r), a))
}

/// An annotation without `AP` drawn as viewers draw it, as a form `XObject` in default user space. One with an
/// opacity below 1, or a multiply blend (text highlights), is a transparency group drawn through a wrapper that
/// sets them, which is how hayro-interpret composites it.
fn synthesised(
    annot: &Dict<'_>,
    page: &Page<'_>,
    ctx: &mut ExtractionContext<'_>,
) -> Option<(Ref, [f32; 6])> {
    let synth = hayro_syntax::annotation::synthesise(annot)?;
    let bbox = convert_rect(&page.media_box());
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let inner = ctx.new_ref();
    let group = synth.opacity < 1.0 || synth.multiply;
    {
        let encoded = deflate_encode(synth.ops.as_bytes());
        let mut x = chunk.form_xobject(inner, &encoded);
        x.deref_mut().filter(Filter::FlateDecode);
        x.bbox(bbox);
        x.resources().finish();
        if group {
            x.group().transparency();
        }
    }
    let placed = if group {
        let outer = ctx.new_ref();
        let mut c = Content::new();
        c.set_parameters(Name(b"G0")).x_object(Name(b"X0"));
        let content = c.finish();
        let mut x = chunk.form_xobject(outer, content.as_slice());
        x.bbox(bbox);
        let mut res = x.resources();
        res.x_objects().pair(Name(b"X0"), inner);
        let mut states = res.ext_g_states();
        let mut gs = states.insert(Name(b"G0")).start::<ExtGraphicsState<'_>>();
        gs.non_stroking_alpha(synth.opacity)
            .stroking_alpha(synth.opacity);
        if synth.multiply {
            gs.blend_mode(BlendMode::Multiply);
        }
        gs.finish();
        states.finish();
        res.finish();
        x.finish();
        outer
    } else {
        inner
    };
    ctx.chunks.push(chunk);
    Some((placed, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]))
}

/// The reference of an annotation's optional content (an OCG or OCMD), written as an object of its own when it is
/// given directly.
fn annotation_oc(annot: &Dict<'_>, ctx: &mut ExtractionContext<'_>) -> Option<Ref> {
    match annot.get_raw::<Object<'_>>(OC)? {
        MaybeRef::Ref(r) => {
            ctx.pdf.xref().get::<Dict<'_>>(r.into())?;
            ctx.to_visit_refs.push(r);
            Some(ctx.map_ref(r))
        }
        MaybeRef::NotRef(Object::Dict(d)) => {
            let r = ctx.new_ref();
            let mut chunk = Chunk::with_settings(ctx.chunk_settings);
            d.write_direct(chunk.indirect(r), ctx);
            ctx.chunks.push(chunk);
            Some(r)
        }
        _ => None,
    }
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

    serialize_resources(
        page.resources(),
        ctx,
        &mut x_object,
        &ExtraResources::default(),
    );

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
    extra: &ExtraResources,
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

    macro_rules! write_with_extra {
        ($name:ident, $key:expr, $extra:expr) => {
            if !resources.$name.is_empty() || !$extra.is_empty() {
                let mut dict = output.insert(Name($key)).dict();
                for (name, obj) in resources.$name.entries() {
                    obj.write_direct(dict.insert(Name(name.deref())), ctx);
                }
                for (name, r) in $extra {
                    dict.pair(Name(name), *r);
                }
            }
        };
    }

    write!(ext_g_states, EXT_G_STATE);
    write!(shadings, SHADING);
    write!(patterns, PATTERN);
    write_with_extra!(x_objects, XOBJECT, &extra.x_objects);
    write!(color_spaces, COLORSPACE);
    write!(fonts, FONT);
    write_with_extra!(properties, PROPERTIES, &extra.properties);
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

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::object::Name as SyntaxName;
    use pdf_writer::TextStr;

    /// A one-page PDF (200 x 100): `content` as the page content, `annots` writing the page's annotations, `res`
    /// its resources. Also written: 10 = an appearance stream without Type/Subtype, 11 = an OCG, 12 = a stream
    /// behind an unknown filter.
    fn pdf_with(
        content: &[u8],
        contents_ref: Ref,
        annots: impl FnOnce(&mut pdf_writer::Array<'_>),
        res: impl FnOnce(&mut pdf_writer::writers::Resources<'_>),
        page_extra: impl FnOnce(&mut pdf_writer::writers::Page<'_>),
    ) -> Vec<u8> {
        let mut w = pdf_writer::Pdf::new();
        let (catalog, pages, page) = (Ref::new(1), Ref::new(2), Ref::new(3));
        w.catalog(catalog).pages(pages);
        w.pages(pages).kids([page]).count(1);
        w.stream(Ref::new(4), content);
        let mut p = w.page(page);
        p.parent(pages)
            .media_box(Rect::new(0.0, 0.0, 200.0, 100.0))
            .contents(contents_ref);
        res(&mut p.resources());
        annots(&mut p.insert(Name(ANNOTS)).array());
        page_extra(&mut p);
        p.finish();
        w.stream(Ref::new(10), b"0 0 1 rg 0 0 10 5 re f")
            .insert(Name(b"BBox"))
            .array()
            .items([0.0_f32, 0.0, 10.0, 5.0]);
        w.indirect(Ref::new(11))
            .dict()
            .pair(Name(b"Type"), Name(b"OCG"))
            .pair(Name(b"Name"), TextStr("Notes"));
        w.stream(Ref::new(12), b" not decodable")
            .pair(Name(b"Filter"), Name(b"BogusDecode"));
        w.finish()
    }

    fn pdf(
        content: &[u8],
        annots: impl FnOnce(&mut pdf_writer::Array<'_>),
        res: impl FnOnce(&mut pdf_writer::writers::Resources<'_>),
    ) -> Vec<u8> {
        pdf_with(content, Ref::new(4), annots, res, |_| {})
    }

    fn square<'a>(a: &'a mut pdf_writer::Array<'_>) -> pdf_writer::Dict<'a> {
        let mut d = a.push().dict();
        d.pair(Name(b"Type"), Name(b"Annot"))
            .pair(Name(b"Subtype"), Name(b"Square"))
            .pair(Name(b"F"), 4);
        d.insert(Name(b"Rect"))
            .array()
            .items([20.0_f32, 20.0, 60.0, 40.0]);
        d.insert(Name(b"C")).array().items([1.0_f32, 0.0, 0.0]);
        d
    }

    fn copy(data: Vec<u8>) -> (Vec<u8>, ExtractionResult) {
        let src = Pdf::new(data).unwrap();
        let mut out = pdf_writer::Pdf::new();
        let mut next = Ref::new(1);
        let catalog = next.bump();
        let options = ExtractionOptions {
            keep_optional_content: true,
            keep_page_boxes: true,
            catalog_keys: vec![],
            flatten_annotations: true,
        };
        let extracted = extract_with(
            &src,
            Box::new(|| next.bump()),
            ChunkSettings::default(),
            &[ExtractionQuery::new_page(0)],
            options,
        )
        .unwrap();
        out.catalog(catalog).pages(extracted.page_tree_parent_ref);
        out.pages(extracted.page_tree_parent_ref)
            .kids([extracted.root_refs[0].unwrap()])
            .count(1);
        out.extend(&extracted.chunk);
        (out.finish(), extracted)
    }

    fn page_content(pdf: &Pdf) -> String {
        String::from_utf8_lossy(pdf.pages()[0].page_stream().unwrap()).into_owned()
    }

    fn x_object<'a>(pdf: &'a Pdf, name: &[u8]) -> Stream<'a> {
        pdf.pages()[0]
            .resources()
            .x_objects
            .get::<Stream<'_>>(name)
            .unwrap()
    }

    #[test]
    fn annotation_without_appearance_is_drawn_as_viewers_draw_it() {
        let src = pdf(
            b"0 0 1 rg 0 0 10 10 re f",
            |a| {
                square(a).finish();
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let content = page_content(&pdf);
        assert!(content.contains("/HayroAnnot0 Do"), "{content}");
        let form = x_object(&pdf, b"HayroAnnot0");
        let ops = String::from_utf8_lossy(&form.decoded().unwrap()).into_owned();
        // pdf.js: a 1-pt red border inset by half its width.
        assert_eq!(ops, "1 0 0 RG 1 w 20.5 20.5 39 19 re S");
        assert!(form.dict().get::<SyntaxName<'_>>(b"Subtype").is_some());
    }

    #[test]
    fn a_translucent_highlight_is_a_multiplied_group() {
        let src = pdf(
            b"",
            |a| {
                let mut d = a.push().dict();
                d.pair(Name(b"Subtype"), Name(b"Highlight"))
                    .pair(Name(b"CA"), 0.5);
                d.insert(Name(b"Rect"))
                    .array()
                    .items([10.0_f32, 10.0, 50.0, 20.0]);
                d.insert(Name(b"QuadPoints"))
                    .array()
                    .items([10.0_f32, 20.0, 50.0, 20.0, 10.0, 10.0, 50.0, 10.0]);
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let outer = x_object(&pdf, b"HayroAnnot0");
        let res = Resources::new(outer.dict().get::<Dict<'_>>(b"Resources").unwrap());
        let gs = res.ext_g_states.get::<Dict<'_>>(b"G0").unwrap();
        assert_eq!(gs.get::<f32>(b"ca"), Some(0.5));
        assert_eq!(
            gs.get::<SyntaxName<'_>>(b"BM").as_deref(),
            Some(&b"Multiply"[..])
        );
        let inner = res.x_objects.get::<Stream<'_>>(b"X0").unwrap();
        assert!(inner.dict().get::<Dict<'_>>(b"Group").is_some());
    }

    #[test]
    fn content_that_does_not_decode_is_copied_as_it_is() {
        let src = pdf_with(b"", Ref::new(12), |_| {}, |_| {}, |_| {});
        let (out, extracted) = copy(src);
        assert_eq!(extracted.raw_content_pages, vec![0]);
        let pdf = Pdf::new(out).unwrap();
        let page = &pdf.pages()[0];
        assert!(!page.content_decodes());
        let stream = page.raw().get::<Stream<'_>>(CONTENTS).unwrap();
        assert_eq!(&*stream.raw_data(), b"\x01\x02\x03 not decodable");
        assert_eq!(
            stream.dict().get::<SyntaxName<'_>>(b"Filter").as_deref(),
            Some(&b"BogusDecode"[..])
        );
    }

    #[test]
    fn annotations_start_from_the_page_state_whatever_the_content_leaves_open() {
        let src = pdf(
            b"q 1 0 0 -1 0 100 cm /OC /L BDC BT /F1 12 Tf 10 10 m 20 20 l",
            |a| {
                square(a).finish();
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let content = page_content(&Pdf::new(out).unwrap());
        let tail = &content[content.find("20 20 l").unwrap()..];
        assert!(tail.starts_with("20 20 l\nn\nET\nEMC\nQ\nQ\n"), "{tail}");
        assert!(content.starts_with("q\n"));
        assert_eq!(
            open_state(b"q Q Q q (q) Tj BI /W 1 ID q EI"),
            OpenState {
                saves: 1,
                underflow: 1,
                ..OpenState::default()
            }
        );
    }

    #[test]
    fn content_with_a_stray_restore_goes_into_a_form() {
        let src = pdf(
            b"Q 2 0 0 2 0 0 cm 0 0 10 10 re f",
            |a| {
                square(a).finish();
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let content = page_content(&pdf);
        assert!(content.starts_with("/HayroPage0 Do\n"), "{content}");
        let form = x_object(&pdf, b"HayroPage0");
        assert_eq!(
            &*form.decoded().unwrap(),
            b"Q 2 0 0 2 0 0 cm 0 0 10 10 re f"
        );
    }

    #[test]
    fn appearance_without_subtype_is_written_as_a_form() {
        let src = pdf(
            b"",
            |a| {
                let mut d = square(a);
                d.insert(Name(b"AP")).dict().pair(Name(b"N"), Ref::new(10));
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let form = x_object(&pdf, b"HayroAnnot0");
        let d = form.dict();
        assert_eq!(
            d.get::<SyntaxName<'_>>(b"Subtype").as_deref(),
            Some(&b"Form"[..])
        );
        assert_eq!(
            d.get::<SyntaxName<'_>>(b"Type").as_deref(),
            Some(&b"XObject"[..])
        );
        assert!(page_content(&pdf).contains("4 0 0 4 20 20 cm"));
    }

    #[test]
    fn annotation_keeps_its_layer() {
        let src = pdf(
            b"",
            |a| {
                let mut d = square(a);
                d.pair(Name(b"OC"), Ref::new(11));
            },
            |_| {},
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let content = page_content(&pdf);
        assert!(content.contains("/OC /HayroOC0 BDC"), "{content}");
        assert!(content.trim_end().ends_with("EMC"), "{content}");
        let props = &pdf.pages()[0].resources().properties;
        let ocg = props.get::<Dict<'_>>(b"HayroOC0").unwrap();
        assert_eq!(
            ocg.get::<SyntaxName<'_>>(b"Type").as_deref(),
            Some(&b"OCG"[..])
        );
    }

    #[test]
    fn names_never_collide_with_the_page_resources() {
        let src = pdf(
            b"/HayroAnnot0 Do",
            |a| {
                square(a).finish();
            },
            |res| {
                res.x_objects().pair(Name(b"HayroAnnot0"), Ref::new(10));
            },
        );
        let (out, _) = copy(src);
        let pdf = Pdf::new(out).unwrap();
        let content = page_content(&pdf);
        assert!(content.contains("/HayroAnnot1 Do"), "{content}");
        assert_eq!(pdf.pages()[0].resources().x_objects.entries().count(), 2);
    }

    #[test]
    fn numbers_beyond_f32_are_clamped_not_inf() {
        assert_eq!(primitive::finite_f32(1e39), f32::MAX);
        assert_eq!(primitive::finite_f32(-1e300), -f32::MAX);
        // A whole number of 42 digits (1e41) in the page's group dictionary, which the copy writes.
        let placeholder = [b"/".as_slice(), &[b'X'; 41]].concat();
        let mut src = pdf_with(
            b"",
            Ref::new(4),
            |_| {},
            |_| {},
            |p| {
                p.insert(Name(b"Group"))
                    .dict()
                    .pair(Name(b"S"), Name(b"Transparency"))
                    .pair(Name(b"Big"), Name(&placeholder[1..]));
            },
        );
        let at = src
            .windows(42)
            .position(|w| w == placeholder.as_slice())
            .unwrap();
        src[at..at + 42].copy_from_slice(&[b"1".as_slice(), &[b'0'; 41]].concat());
        let (out, _) = copy(src);
        assert!(
            !out.windows(3).any(|w| w == b"inf"),
            "{}",
            String::from_utf8_lossy(&out)
        );
    }
}
