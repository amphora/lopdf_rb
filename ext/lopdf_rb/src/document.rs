use std::cell::RefCell;
use std::io::Cursor;

use lopdf::Document;
use magnus::prelude::*;
use magnus::value::ReprValue;
use magnus::{function, method, Error, RArray, RHash, RModule, RString, Symbol, Value};

use crate::geometry::get_page_dimensions;

/// Ruby wrapper around a `lopdf::Document`.
///
/// # Interior mutability and the single-borrow-at-a-time invariant
///
/// `inner` is a `RefCell` because magnus exposes every method via `&self`
/// (shared references), yet several methods need `&mut Document` for their
/// underlying lopdf work: `save`, `to_bytes` (`save_to`), `stamp_metadata`,
/// `add_dlp_annotation`, `apply_visible_stamps`, `rotate_all_pages`, and
/// `duplicate`. `RefCell` moves the shared-vs-exclusive borrow check from
/// compile time to run time so those `&mut` operations can be reached through a
/// `&self` method.
///
/// **Invariant: for any one `RbDocument`, at most one borrow of its `inner` is
/// live at a time.** Every method here takes a fresh `borrow()` / `borrow_mut()`
/// on `self.inner`, does its work, and drops the guard before returning to Ruby.
/// Two facts uphold this today:
///
///   1. Ruby's GVL serialises Ruby-thread execution, so no two Ruby threads are
///      ever inside these methods at once — there is no concurrent access.
///   2. No method holds a guard on a cell across a call that borrows that
///      *same* cell.
///
/// (`merge` borrows several *different* documents' cells at once — one shared
/// `borrow()` per input — which is fine: the invariant is per-cell, and those
/// are distinct `RefCell`s.)
///
/// **Primary risk for future refactors.** `RefCell` enforces the invariant at
/// *run time*, not compile time — so the compiler will not catch a violation.
/// A Rust-level composition that holds a `borrow_mut()` guard on a cell while
/// calling a method that borrows the *same* cell — e.g. taking `borrow_mut()`
/// and, while it is still live, calling `page_dimensions` (which takes
/// `borrow()`) — compiles cleanly but panics (`already mutably borrowed`) the
/// first time that path executes, with no compile-time warning. When composing
/// wrapper logic, drop the outer guard before the inner call, or factor the
/// shared work into a helper taking `&mut Document` so both callers pass one
/// borrow down rather than each taking their own. The `nested_borrow_panics` /
/// `nested_borrow_mut_panics` tests pin this failure mode at the `RefCell`
/// level (the wrapper methods need a live Ruby VM, so they are exercised via
/// their delegates — see the borrow-choreography tests).
#[magnus::wrap(class = "LopdfRb::Document")]
pub struct RbDocument {
    inner: RefCell<Document>,
}

impl RbDocument {
    /// `LopdfRb::Document.load(path)` — load a PDF from a file path.
    fn load(path: String) -> Result<Self, Error> {
        let doc = Document::load(&path).map_err(|e| {
            Error::new(
                magnus::exception::runtime_error(),
                format!("Failed to load PDF from '{}': {}", path, e),
            )
        })?;
        Ok(RbDocument {
            inner: RefCell::new(doc),
        })
    }

    /// `LopdfRb::Document.from_bytes(bytes)` — load a PDF from a binary string.
    ///
    /// Uses `unsafe { bytes.as_slice() }` to borrow the Ruby string's bytes in
    /// place rather than copying the whole PDF into a fresh allocation first.
    ///
    /// # Soundness of the internal `unsafe` borrow
    ///
    /// (`from_bytes` is a *safe* fn — callers carry no obligation. This section
    /// documents why the `unsafe` block below is sound; the `// SAFETY:` comment
    /// on the block is the conventional pointer to it.)
    ///
    /// `RString::as_slice` is unsafe because the returned slice is only valid
    /// while the backing Ruby string is neither freed nor relocated by Ruby's
    /// GC. This call is sound because of three facts, each of which must hold:
    ///
    /// 1. **The slice stays valid for the whole `load_mem` call.** The calling
    ///    Ruby thread holds the GVL for the entire method, and between
    ///    `as_slice()` and `load_mem` returning it allocates no Ruby objects and
    ///    never releases the GVL or re-enters Ruby. MRI's GC — mark-sweep *and*
    ///    the compactor (`GC.compact` / `GC.auto_compact`, either of which an
    ///    application may enable outside this gem's control) — only runs at a
    ///    safe point: a Ruby allocation or a GVL release. Neither occurs in this
    ///    window, so the RString cannot be freed or relocated regardless of
    ///    whether auto-compaction is on. `bytes` is also a live method argument
    ///    reachable from the Ruby stack, so it is uncollectable in any case.
    ///    Note `load_mem` is *not* single-threaded: lopdf's default features
    ///    include `rayon`, so its parser reads the slice from several native
    ///    Rayon worker threads (`reader.rs` `par_iter`). That is sound — the
    ///    workers are pure Rust, never touch the Ruby C API, `&[u8]` is `Sync`,
    ///    and Rayon joins them before `load_mem` returns — but it means the
    ///    guarantee is "the *calling* thread holds the GVL", not "nothing runs
    ///    concurrently". Do not use this comment as licence to add heap
    ///    allocations or GVL-releasing calls inside the borrow window.
    ///
    /// 2. **`load_mem` retains no reference to the slice.** It returns an owned
    ///    `lopdf::Document` — a type with no lifetime parameter — whose parser
    ///    copies every byte it keeps into owned `Vec<u8>`s (`Object::String`,
    ///    `Object::Stream` content via `self.buffer[..].to_vec()`, …); lopdf's
    ///    internal `Reader`, which borrows the slice, is dropped before
    ///    `load_mem` returns. (The `Stream::start_position` that survives is a
    ///    byte-offset integer, not a pointer into the input.) Verified against
    ///    lopdf 0.34.0 (`reader.rs`: `Document::load_mem` → `<&[u8] as
    ///    TryInto<Document>>`). lopdf 0.34.0 also declares
    ///    `#![forbid(unsafe_code)]` (`lib.rs:2`), which makes internal
    ///    raw-pointer retention *structurally impossible* in this version — not
    ///    merely absent — so fact 2 rests on more than a one-time source read.
    ///
    /// 3. **The lopdf version is pinned.** Fact 2 is an internal lopdf
    ///    implementation detail, not a stable API contract. A change making the
    ///    returned document *borrow* the input would need a lifetime on
    ///    `Document`/`load_mem` and so would break *this* call site at compile
    ///    time — but a future lopdf that lifted `forbid(unsafe_code)` and
    ///    retained the pointer via `unsafe`, or that began releasing the GVL /
    ///    calling back into Ruby mid-parse, would be silent. The
    ///    `lopdf = "=0.34"` pin in `Cargo.toml` (and the exact `Cargo.lock`
    ///    version) is therefore a hard stability invariant: any lopdf bump must
    ///    re-verify that `load_mem` still copies out into an owned `Document`,
    ///    keeps `forbid(unsafe_code)`, and never releases the GVL, before the
    ///    pin is moved.
    fn from_bytes(bytes: RString) -> Result<Self, Error> {
        // SAFETY: see "Soundness of the internal `unsafe` borrow" above. The
        // slice is read only while this `load_mem` call runs; the calling thread
        // holds the GVL so MRI GC cannot free or compact the RString, any Rayon
        // worker threads only read the slice and are joined before the call
        // returns, and `load_mem` returns an owned Document that copies the
        // bytes out and keeps no borrow into the slice.
        let slice = unsafe { bytes.as_slice() };
        let doc = Document::load_mem(slice).map_err(|e| {
            Error::new(
                magnus::exception::runtime_error(),
                format!("Failed to load PDF from bytes: {}", e),
            )
        })?;
        Ok(RbDocument {
            inner: RefCell::new(doc),
        })
    }

    /// `doc.page_count` — returns the number of pages in the document.
    fn page_count(&self) -> usize {
        self.inner.borrow().get_pages().len()
    }

    /// `doc.page_dimensions(page_index)` — returns `{ width:, height: }` hash.
    ///
    /// `page_index` is 0-based (Ruby convention). Internally converted to
    /// lopdf's 1-based page numbers. Walks the /Parent chain to resolve
    /// inherited MediaBox/CropBox per ISO 32000 §7.7.3.4.
    fn page_dimensions(&self, page_index: usize) -> Result<RHash, Error> {
        let doc = self.inner.borrow();
        let pages = doc.get_pages();
        let page_number = (page_index + 1) as u32;

        let page_id = pages.get(&page_number).ok_or_else(|| {
            Error::new(
                magnus::exception::arg_error(),
                format!(
                    "Page index {} out of range (document has {} pages)",
                    page_index,
                    pages.len()
                ),
            )
        })?;

        let (width, height) = get_page_dimensions(&doc, *page_id);

        let hash = RHash::new();
        hash.aset(Symbol::new("width"), width)?;
        hash.aset(Symbol::new("height"), height)?;
        hash.freeze();

        Ok(hash)
    }

    /// `doc.save(path)` — save the document to a file.
    fn save(&self, path: String) -> Result<(), Error> {
        self.inner.borrow_mut().save(&path).map_err(|e| {
            Error::new(
                magnus::exception::runtime_error(),
                format!("Failed to save PDF to '{}': {}", path, e),
            )
        })?;
        Ok(())
    }

    /// `doc.to_bytes` — serialize the document to a binary string.
    ///
    /// Requires `&mut self` on the lopdf Document because `save_to` updates
    /// internal cross-reference tables during serialization.
    fn to_bytes(&self) -> Result<RString, Error> {
        let mut buf = Cursor::new(Vec::new());
        self.inner.borrow_mut().save_to(&mut buf).map_err(|e| {
            Error::new(
                magnus::exception::runtime_error(),
                format!("Failed to serialize PDF to bytes: {}", e),
            )
        })?;
        Ok(RString::from_slice(&buf.into_inner()))
    }

    /// `doc.stamp_metadata(reader:, ip:, timestamp:, unique_id:)` — set /Info dict entries.
    ///
    /// Writes 4 custom fields (Reader, ReadTimestamp, UniqueID, ReaderIP) to the
    /// PDF's /Info dictionary. Creates the dictionary if it doesn't exist.
    fn stamp_metadata(
        &self,
        reader: String,
        ip: String,
        timestamp: String,
        unique_id: String,
    ) -> Result<(), Error> {
        crate::metadata::set_metadata(
            &mut self.inner.borrow_mut(),
            &reader,
            &timestamp,
            &unique_id,
            &ip,
        );
        Ok(())
    }

    /// `doc.add_dlp_annotation(dlp_json)` — add a hidden FreeText annotation on the last page.
    ///
    /// The annotation is invisible (Hidden + NoView flags) and contains the
    /// provided string as its /Contents. Typically a JSON blob with reader/document metadata.
    fn add_dlp_annotation(&self, dlp_data: String) -> Result<(), Error> {
        crate::annotation::add_invisible_annotation(
            &mut self.inner.borrow_mut(),
            &dlp_data,
        ).map_err(|e| Error::new(magnus::exception::runtime_error(), e))
    }

    /// `doc.apply_visible_stamps(config_hash)` — render stamps on every page.
    ///
    /// Takes a Ruby Hash with keys :stamps, :text_blocks, :lines, :rectangles.
    /// Converts to JSON via `to_json`, then deserializes into StampConfig to
    /// preserve all serde defaults. The JSON round-trip overhead (~microseconds)
    /// is negligible vs PDF processing.
    fn apply_visible_stamps(&self, config: Value) -> Result<(), Error> {
        let json_str: String = config.funcall("to_json", ())?;

        let stamp_config: crate::stamp::StampConfig =
            serde_json::from_str(&json_str).map_err(|e| {
                Error::new(
                    magnus::exception::arg_error(),
                    format!("Invalid stamp config: {}", e),
                )
            })?;

        crate::stamp::apply_stamp_config(&mut self.inner.borrow_mut(), &stamp_config);
        Ok(())
    }

    /// `doc.rotate_all_pages(angle)` — rotate all pages by the given angle.
    ///
    /// Angle must be 0, 90, 180, or 270 (clockwise). Cumulative with any
    /// existing `/Rotate` value on each page.
    fn rotate_all_pages(&self, angle: i64) -> Result<(), Error> {
        crate::manipulation::rotate_all_pages(&mut self.inner.borrow_mut(), angle)
            .map_err(|e| Error::new(magnus::exception::arg_error(), e))
    }

    /// `doc.split_pages` — split into individual single-page documents.
    ///
    /// Returns a Ruby Array of `LopdfRb::Document` instances, one per page.
    fn split_pages(&self) -> Result<RArray, Error> {
        let docs = crate::manipulation::split_pages(&self.inner.borrow())
            .map_err(|e| Error::new(magnus::exception::runtime_error(), e))?;

        let array = RArray::new();
        for doc in docs {
            array.push(RbDocument {
                inner: RefCell::new(doc),
            })?;
        }
        Ok(array)
    }

    /// `doc.duplicate` — deep-copy this document.
    ///
    /// Returns a new independent `LopdfRb::Document` via serialize round-trip.
    fn duplicate(&self) -> Result<Self, Error> {
        let doc = crate::manipulation::duplicate_document(&mut self.inner.borrow_mut())
            .map_err(|e| Error::new(magnus::exception::runtime_error(), e))?;
        Ok(RbDocument {
            inner: RefCell::new(doc),
        })
    }

    /// `LopdfRb::Document.merge(docs)` — merge multiple documents into one.
    ///
    /// Takes a Ruby Array of `LopdfRb::Document` instances. Returns a new
    /// merged document with all pages in order.
    ///
    /// Uses manual iteration with `TryConvert` because `Obj<T>` does not
    /// implement `TryConvertOwned` (required by `RArray::to_vec`).
    fn merge(docs: RArray) -> Result<Self, Error> {
        let len = docs.len();
        let mut typed: Vec<magnus::typed_data::Obj<RbDocument>> = Vec::with_capacity(len);

        for i in 0..len {
            let val: Value = docs.entry(i as isize)?;
            let obj = <magnus::typed_data::Obj<RbDocument> as magnus::TryConvert>::try_convert(val)
                .map_err(|_| {
                    Error::new(
                        magnus::exception::arg_error(),
                        format!("Element {} is not a LopdfRb::Document", i),
                    )
                })?;
            typed.push(obj);
        }

        // Borrow all RefCells — safe under GVL (no concurrent mutation)
        let borrows: Vec<_> = typed.iter().map(|obj| obj.inner.borrow()).collect();
        let refs: Vec<&Document> = borrows.iter().map(|b| &**b).collect();

        let merged = crate::manipulation::merge_documents(&refs)
            .map_err(|e| Error::new(magnus::exception::runtime_error(), e))?;
        Ok(RbDocument {
            inner: RefCell::new(merged),
        })
    }
}

/// Register the `Document` class under the `LopdfRb` module.
pub fn init(module: RModule) -> Result<(), Error> {
    let class = module.define_class("Document", magnus::class::object())?;

    class.define_singleton_method("load", function!(RbDocument::load, 1))?;
    class.define_singleton_method("from_bytes", function!(RbDocument::from_bytes, 1))?;
    class.define_singleton_method("merge", function!(RbDocument::merge, 1))?;
    class.define_method("page_count", method!(RbDocument::page_count, 0))?;
    class.define_method("page_dimensions", method!(RbDocument::page_dimensions, 1))?;
    class.define_method("save", method!(RbDocument::save, 1))?;
    class.define_method("to_bytes", method!(RbDocument::to_bytes, 0))?;
    class.define_method("stamp_metadata", method!(RbDocument::stamp_metadata, 4))?;
    class.define_method("add_dlp_annotation", method!(RbDocument::add_dlp_annotation, 1))?;
    class.define_method("apply_visible_stamps", method!(RbDocument::apply_visible_stamps, 1))?;
    class.define_method("rotate_all_pages", method!(RbDocument::rotate_all_pages, 1))?;
    class.define_method("split_pages", method!(RbDocument::split_pages, 0))?;
    class.define_method("duplicate", method!(RbDocument::duplicate, 0))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::obj_to_f64;
    use lopdf::{Dictionary, Object, ObjectId, Stream};

    /// Build a minimal PDF document programmatically for testing.
    /// If `media_box` is Some, sets /MediaBox on the page dict.
    /// If `crop_box` is Some, sets /CropBox on the page dict.
    /// If `parent_media_box` is Some, sets /MediaBox on the parent /Pages dict
    /// (and omits it from the page dict, to test inheritance).
    fn create_test_pdf(
        media_box: Option<Vec<Object>>,
        crop_box: Option<Vec<Object>>,
        parent_media_box: Option<Vec<Object>>,
    ) -> (Document, ObjectId) {
        let mut doc = Document::new();

        // Create a minimal content stream (empty page)
        let content_stream = Stream::new(Dictionary::new(), Vec::new());
        let content_id = doc.add_object(content_stream);

        // Build the /Pages (parent) dictionary
        let pages_id = doc.new_object_id();

        // Build the page dictionary
        let mut page_dict = Dictionary::new();
        page_dict.set("Type", Object::Name(b"Page".to_vec()));
        page_dict.set("Parent", Object::Reference(pages_id));
        page_dict.set("Contents", Object::Reference(content_id));

        if let Some(mb) = media_box {
            page_dict.set("MediaBox", Object::Array(mb));
        }
        if let Some(cb) = crop_box {
            page_dict.set("CropBox", Object::Array(cb));
        }

        let page_id = doc.add_object(page_dict);

        // Build the parent /Pages dictionary
        let mut pages_dict = Dictionary::new();
        pages_dict.set("Type", Object::Name(b"Pages".to_vec()));
        pages_dict.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages_dict.set("Count", Object::Integer(1));

        if let Some(pmb) = parent_media_box {
            pages_dict.set("MediaBox", Object::Array(pmb));
        }

        doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

        // Set the catalog
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));

        (doc, page_id)
    }

    fn us_letter_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(612),
            Object::Integer(792),
        ]
    }

    fn a4_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(595.28),
            Object::Real(841.89),
        ]
    }

    #[test]
    fn test_page_dimensions_direct_mediabox() {
        let (doc, page_id) = create_test_pdf(Some(us_letter_box()), None, None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 612.0).abs() < 0.01);
        assert!((h - 792.0).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_inherited_from_parent() {
        // No MediaBox on the page, but parent /Pages has one
        let (doc, page_id) = create_test_pdf(None, None, Some(a4_box()));
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_cropbox_fallback() {
        // No MediaBox anywhere, only CropBox on the page
        let (doc, page_id) = create_test_pdf(None, Some(a4_box()), None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn test_page_dimensions_default_us_letter() {
        // No MediaBox or CropBox anywhere → falls back to US Letter
        let (doc, page_id) = create_test_pdf(None, None, None);
        let (w, h) = get_page_dimensions(&doc, page_id);
        assert!((w - 612.0).abs() < 0.01);
        assert!((h - 792.0).abs() < 0.01);
    }

    #[test]
    fn test_obj_to_f64_integer_and_real() {
        assert!((obj_to_f64(&Object::Integer(42)) - 42.0).abs() < f64::EPSILON);
        assert!((obj_to_f64(&Object::Real(3.14)) - 3.14).abs() < 0.001);
    }

    #[test]
    fn test_obj_to_f64_unsupported() {
        assert!((obj_to_f64(&Object::Boolean(true))).abs() < f64::EPSILON);
        assert!((obj_to_f64(&Object::Null)).abs() < f64::EPSILON);
    }
}
