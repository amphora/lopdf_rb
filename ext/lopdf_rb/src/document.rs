use std::cell::RefCell;
use std::io::Cursor;

use lopdf::Document;
use magnus::prelude::*;
use magnus::{function, method, Error, RHash, RModule, RString, Symbol};

use crate::geometry::get_page_dimensions;

/// Ruby wrapper around a `lopdf::Document`.
///
/// Uses `RefCell` for interior mutability because magnus exposes methods via
/// `&self` (shared references), but several lopdf operations (save, serialize)
/// require `&mut self`. Safe under Ruby's GVL — no concurrent access.
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
    /// Uses `unsafe { bytes.as_slice() }` to avoid copying the entire PDF into
    /// a new allocation. This is safe because `Document::load_mem` copies the
    /// data into its own structures before returning, so the slice is not held
    /// beyond this call. Safe under the GVL.
    fn from_bytes(bytes: RString) -> Result<Self, Error> {
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
}

/// Register the `Document` class under the `LopdfRb` module.
pub fn init(module: RModule) -> Result<(), Error> {
    let class = module.define_class("Document", magnus::class::object())?;

    class.define_singleton_method("load", function!(RbDocument::load, 1))?;
    class.define_singleton_method("from_bytes", function!(RbDocument::from_bytes, 1))?;
    class.define_method("page_count", method!(RbDocument::page_count, 0))?;
    class.define_method("page_dimensions", method!(RbDocument::page_dimensions, 1))?;
    class.define_method("save", method!(RbDocument::save, 1))?;
    class.define_method("to_bytes", method!(RbDocument::to_bytes, 0))?;

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
