# lopdf_rb

Ruby native extension wrapping the Rust [lopdf](https://crates.io/crates/lopdf) crate for PDF document manipulation. Built with [magnus](https://github.com/matsadler/magnus) and [rb-sys](https://github.com/oxidize-rb/rb-sys).

Part of the [PatentSafe](https://www.amphora.net/patentsafe) PDF pipeline — replaces subprocess-based PDF tooling with direct Ruby API calls.

## API

```ruby
require "lopdf_rb"

# Load from file
doc = LopdfRb::Document.load("/path/to/file.pdf")

# Load from binary string
doc = LopdfRb::Document.from_bytes(pdf_bytes)

# Inspect
doc.page_count                # => 3
doc.page_dimensions(0)        # => { width: 612.0, height: 792.0 }

# Save to file
doc.save("/path/to/output.pdf")

# Serialize to binary string
bytes = doc.to_bytes
```

### `LopdfRb::Document.load(path)` → `Document`

Load a PDF from a file path. Raises `RuntimeError` if the file cannot be read or parsed.

### `LopdfRb::Document.from_bytes(string)` → `Document`

Load a PDF from a binary string (`String` with `ASCII-8BIT` encoding). Raises `RuntimeError` if the data is not valid PDF.

### `#page_count` → `Integer`

Returns the number of pages in the document.

### `#page_dimensions(page_index)` → `Hash`

Returns a frozen hash `{ width: Float, height: Float }` with the page dimensions in PDF points (1 point = 1/72 inch).

`page_index` is **0-based**. Raises `ArgumentError` if the index is out of range.

Resolves inherited `/MediaBox` and `/CropBox` attributes by walking the page tree per ISO 32000 §7.7.3.4. Falls back to US Letter (612 × 792 points) if no bounding box is found.

### `#save(path)` → `nil`

Save the document to a file. Raises `RuntimeError` on I/O failure.

### `#to_bytes` → `String`

Serialize the document to a binary string (`ASCII-8BIT` encoding).

## Building

Requires Rust (stable) and the `rb_sys` gem:

```bash
cd gems/lopdf_rb
cargo build --release    # compile the native extension
cargo test               # run Rust unit tests
cargo clippy             # lint
```

## License

MIT — see [LICENSE](LICENSE).
