use std::cell::RefCell;
use std::io::Cursor;

use lopdf::Document;
use magnus::prelude::*;
use magnus::value::ReprValue;
use magnus::{function, method, Error, RArray, RHash, RModule, RString, Symbol, Value};

use crate::geometry::get_page_dimensions_or_fallback;

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
/// One scope nuance: in the common `delegate(&mut self.inner.borrow_mut(), …)
/// .map_err(|e| Error::new(…))` tail expression, the `RefMut` temporary is not
/// dropped until the whole expression finishes — so the `map_err` closure runs
/// while the guard is technically still live. Benign today (magnus `Error::new`
/// with an exception class and `String` message is a pure data constructor, no
/// Ruby allocation), but error-mapping closures must never re-borrow `inner`
/// or call back into Ruby.
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

        let (width, height) = get_page_dimensions_or_fallback(&doc, *page_id, page_number);

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

    /// `doc.stamp_metadata(reader, ip, timestamp, unique_id)` — set /Info dict entries.
    ///
    /// Positional arguments, in this order: reader name, reader IP, ISO 8601
    /// timestamp, unique ID (the delegate `metadata::set_metadata` takes the
    /// same order).
    ///
    /// Writes 4 custom fields (Reader, ReadTimestamp, UniqueID, ReaderIP) to the
    /// PDF's /Info dictionary. Creates the dictionary if it doesn't exist.
    /// ASCII values are stored verbatim, non-ASCII as UTF-16BE with a BOM.
    ///
    /// Raises `ArgumentError` when any argument exceeds
    /// `metadata::MAX_FIELD_BYTES` (512) bytes of UTF-8, and `RuntimeError`
    /// when /Info cannot be resolved to a dictionary.
    fn stamp_metadata(
        &self,
        reader: String,
        ip: String,
        timestamp: String,
        unique_id: String,
    ) -> Result<(), Error> {
        crate::metadata::validate_field_lengths(&[
            ("reader", &reader),
            ("ip", &ip),
            ("timestamp", &timestamp),
            ("unique_id", &unique_id),
        ])
        .map_err(|e| Error::new(magnus::exception::arg_error(), e))?;

        crate::metadata::set_metadata(
            &mut self.inner.borrow_mut(),
            &reader,
            &ip,
            &timestamp,
            &unique_id,
        )
        .map_err(|e| Error::new(magnus::exception::runtime_error(), e))
    }

    /// `doc.add_dlp_annotation(dlp_json)` — add a hidden FreeText annotation on the last page.
    ///
    /// The annotation is invisible (Hidden + NoView flags) and contains the
    /// provided string as its /Contents. Typically a JSON blob with reader/document metadata.
    ///
    /// Raises `RuntimeError` when the PDF has no pages or the annotation
    /// cannot be wired into the last page's /Annots (inaccessible page
    /// dictionary, or an indirect /Annots that does not resolve to an
    /// array) — the pending annotation object is removed again on failure,
    /// so the error leaves the document's object set unchanged.
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
    ///
    /// Raises `ArgumentError` when the config does not deserialize, and
    /// `RuntimeError` when a stamp cannot be applied (font registration or
    /// content-stream append fails) — the document is partially stamped in
    /// memory at that point and must be discarded, not saved.
    fn apply_visible_stamps(&self, config: Value) -> Result<(), Error> {
        let json_str: String = config.funcall("to_json", ())?;

        let stamp_config: crate::stamp::StampConfig =
            serde_json::from_str(&json_str).map_err(|e| {
                Error::new(
                    magnus::exception::arg_error(),
                    format!("Invalid stamp config: {}", e),
                )
            })?;

        crate::stamp::apply_stamp_config(&mut self.inner.borrow_mut(), &stamp_config)
            .map_err(|e| Error::new(magnus::exception::runtime_error(), e))
    }

    /// `doc.rotate_all_pages(angle)` — rotate all pages by the given angle.
    ///
    /// Angle must be 0, 90, 180, or 270 (clockwise). Cumulative with any
    /// existing `/Rotate` value on each page.
    ///
    /// Raises `ArgumentError` when the angle is not one of 0/90/180/270,
    /// and `RuntimeError` when a page dictionary cannot be resolved for
    /// the write — the document may be partially rotated in memory at
    /// that point and must be discarded, not saved.
    fn rotate_all_pages(&self, angle: i64) -> Result<(), Error> {
        crate::manipulation::validate_rotation_angle(angle)
            .map_err(|e| Error::new(magnus::exception::arg_error(), e))?;
        crate::manipulation::rotate_all_pages(&mut self.inner.borrow_mut(), angle)
            .map_err(|e| Error::new(magnus::exception::runtime_error(), e))
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
    /// Raises `ArgumentError` when an element of the array is not a
    /// `LopdfRb::Document`, and `RuntimeError` when the merge fails (an
    /// empty input list, or a merged page whose dictionary cannot be
    /// resolved for the /Parent rewrite). The inputs are never modified.
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
    use lopdf::{Dictionary, Object, Stream, StringFormat};

    fn us_letter_box() -> Vec<Object> {
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(612),
            Object::Integer(792),
        ]
    }

    // ── RefCell borrow-choreography tests ───────────────────────────────
    //
    // The magnus wrapper methods (`stamp_metadata`, `rotate_all_pages`, …)
    // cannot be invoked from a `#[cfg(test)]` binary — they build Ruby objects
    // (`RHash`, `RString`, magnus `Error`) that need a live Ruby VM. So these
    // tests reproduce each wrapper's exact `RefCell` borrow sequence against a
    // real `lopdf::Document` and call the same `pub(crate)` delegate, asserting
    // the post-condition the wrapper would produce. This exercises the borrow
    // choreography (the subject of the single-borrow-at-a-time invariant) at
    // the delegate level. The Ruby-layer gap is documented in the PR body.

    /// Build an `n`-page PDF for the choreography tests. Distinct from the
    /// dimension-focused `create_test_pdf` in `geometry.rs`: it takes a page
    /// count and returns just the `Document` (each page a US-Letter empty
    /// content page), mirroring the identically-shaped helpers in
    /// `annotation.rs` / `manipulation.rs`.
    fn build_pdf(page_count: usize) -> Document {
        let mut doc = Document::new();
        let pages_id = doc.new_object_id();

        let mut kids = Vec::new();
        for _ in 0..page_count {
            let content_id = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));

            let mut page_dict = Dictionary::new();
            page_dict.set("Type", Object::Name(b"Page".to_vec()));
            page_dict.set("Parent", Object::Reference(pages_id));
            page_dict.set("MediaBox", Object::Array(us_letter_box()));
            page_dict.set("Contents", Object::Reference(content_id));
            kids.push(Object::Reference(doc.add_object(page_dict)));
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

        doc
    }

    /// `stamp_metadata` → `validate_field_lengths` (no borrow, can return
    /// early) → `borrow_mut()` → `metadata::set_metadata`.
    #[test]
    fn borrow_choreography_stamp_metadata() {
        let cell = RefCell::new(build_pdf(1));
        crate::metadata::validate_field_lengths(&[
            ("reader", "Alice"),
            ("ip", "10.0.0.1"),
            ("timestamp", "2026-07-01T00:00:00Z"),
            ("unique_id", "UID-1"),
        ])
        .unwrap();
        crate::metadata::set_metadata(
            &mut cell.borrow_mut(),
            "Alice",
            "10.0.0.1",
            "2026-07-01T00:00:00Z",
            "UID-1",
        )
        .unwrap();

        let doc = cell.borrow();
        let info_id = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        if let Ok(Object::Dictionary(ref info)) = doc.get_object(info_id) {
            // Assert the value, not just key presence, so a transposed-argument
            // regression (reader vs timestamp) would be caught.
            assert_eq!(
                info.get(b"Reader").unwrap(),
                &Object::String(b"Alice".to_vec(), StringFormat::Literal)
            );
        } else {
            panic!("/Info is not a dictionary");
        }
    }

    /// `add_dlp_annotation` → `borrow_mut()` → `annotation::add_invisible_annotation`.
    #[test]
    fn borrow_choreography_add_dlp_annotation() {
        let cell = RefCell::new(build_pdf(2));
        crate::annotation::add_invisible_annotation(
            &mut cell.borrow_mut(),
            r#"{"reader":"Alice"}"#,
        )
        .unwrap();

        let doc = cell.borrow();
        let last_page_id = *doc.get_pages().values().last().unwrap();
        if let Ok(Object::Dictionary(ref page)) = doc.get_object(last_page_id) {
            assert!(
                page.get(b"Annots").is_ok(),
                "last page should gain an /Annots entry"
            );
        } else {
            panic!("page is not a dictionary");
        }
    }

    /// `apply_visible_stamps` → `borrow_mut()` → `stamp::apply_stamp_config`.
    ///
    /// Mirrors the delegate's borrow sequence only. The real wrapper also calls
    /// `config.funcall("to_json", ())` (a Ruby re-entry) *before* it takes
    /// `borrow_mut()`; that step needs a live Ruby VM and is not covered here. A
    /// non-empty config is used deliberately so the post-condition proves the
    /// delegate actually mutated the document — an empty config is a no-op and
    /// would pass vacuously.
    #[test]
    fn borrow_choreography_apply_visible_stamps() {
        let cell = RefCell::new(build_pdf(2));
        // One rectangle → apply_stamp_config appends a content stream to each
        // page, promoting its /Contents (a single Reference) to a 2-element
        // Array [original, stamp].
        let config: crate::stamp::StampConfig =
            serde_json::from_str(r#"{"rectangles":[{"x1":10,"y1":10,"x2":100,"y2":100}]}"#)
                .unwrap();
        crate::stamp::apply_stamp_config(&mut cell.borrow_mut(), &config)
            .expect("stamping should succeed");

        let doc = cell.borrow();
        assert_eq!(doc.get_pages().len(), 2, "page count unchanged");
        for (_num, page_id) in doc.get_pages() {
            if let Ok(Object::Dictionary(ref page)) = doc.get_object(page_id) {
                if let Ok(Object::Array(ref contents)) = page.get(b"Contents") {
                    assert_eq!(contents.len(), 2, "original + appended stamp stream");
                } else {
                    panic!("/Contents should be an Array after stamping");
                }
            } else {
                panic!("page is not a dictionary");
            }
        }
    }

    /// `rotate_all_pages` → `borrow_mut()` → `manipulation::rotate_all_pages`.
    #[test]
    fn borrow_choreography_rotate_all_pages() {
        let cell = RefCell::new(build_pdf(3));
        crate::manipulation::rotate_all_pages(&mut cell.borrow_mut(), 90).unwrap();

        let doc = cell.borrow();
        for (_num, page_id) in doc.get_pages() {
            if let Ok(Object::Dictionary(ref page)) = doc.get_object(page_id) {
                assert_eq!(page.get(b"Rotate").unwrap(), &Object::Integer(90));
            } else {
                panic!("page is not a dictionary");
            }
        }
    }

    /// `split_pages` → `borrow()` (immutable) → `manipulation::split_pages`.
    #[test]
    fn borrow_choreography_split_pages() {
        let cell = RefCell::new(build_pdf(3));
        let parts = crate::manipulation::split_pages(&cell.borrow()).unwrap();
        assert_eq!(parts.len(), 3);
        for part in &parts {
            assert_eq!(part.get_pages().len(), 1);
        }
    }

    /// `duplicate` → `borrow_mut()` → `manipulation::duplicate_document`.
    #[test]
    fn borrow_choreography_duplicate() {
        let cell = RefCell::new(build_pdf(2));
        let dup = crate::manipulation::duplicate_document(&mut cell.borrow_mut()).unwrap();
        assert_eq!(dup.get_pages().len(), 2);
        assert_eq!(cell.borrow().get_pages().len(), 2, "source is left intact");
    }

    /// `merge` → multiple immutable `borrow()`s → `manipulation::merge_documents`.
    #[test]
    fn borrow_choreography_merge() {
        let a = RefCell::new(build_pdf(2));
        let b = RefCell::new(build_pdf(3));
        // Mirrors the wrapper: collect the borrows, then take `&Document` refs.
        let ba = a.borrow();
        let bb = b.borrow();
        let merged = crate::manipulation::merge_documents(&[&*ba, &*bb]).unwrap();
        assert_eq!(merged.get_pages().len(), 5);
    }

    /// The invariant's failure mode, pinned at the `RefCell` level. A future
    /// wrapper composition that holds a `borrow_mut()` guard and then reaches a
    /// path taking `borrow()` on the *same* cell (e.g. `borrow_mut()` then
    /// `page_dimensions`, which takes `borrow()`) panics at run time;
    /// `RefCell::borrow` reports `already mutably borrowed`. The wrapper methods
    /// need a live Ruby VM, so this pins the mechanism on a bare cell.
    #[test]
    #[should_panic(expected = "already mutably borrowed")]
    fn nested_borrow_panics() {
        let cell = RefCell::new(build_pdf(1));
        let _guard = cell.borrow_mut();
        let _second = cell.borrow();
    }

    /// The complementary direction: a live shared `borrow()` guard, then a
    /// `borrow_mut()` on the same cell. `RefCell::borrow_mut` reports
    /// `already borrowed` — a distinct message from the case above, so both
    /// directions are pinned rather than assumed symmetric.
    #[test]
    #[should_panic(expected = "already borrowed")]
    fn nested_borrow_mut_panics() {
        let cell = RefCell::new(build_pdf(1));
        let _guard = cell.borrow();
        let _second = cell.borrow_mut();
    }
}
