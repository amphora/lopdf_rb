require_relative "lib/lopdf_rb/version"

Gem::Specification.new do |spec|
  spec.name = "lopdf_rb"
  spec.version = LopdfRb::VERSION
  spec.authors = [ "Amphora Research Systems" ]
  spec.license = "MIT"

  spec.summary = "Ruby bindings for the lopdf PDF library"
  spec.description = "A native Ruby extension wrapping lopdf, a Rust library for PDF document " \
                     "manipulation. Provides loading, saving, page inspection, and serialization."
  spec.homepage = "https://github.com/amphora/lopdf_rb"
  spec.required_ruby_version = ">= 3.1.0"

  spec.files = Dir[
    "lib/**/*.rb",
    "ext/**/*.{rs,toml,rb}",
    "Cargo.toml",
    "LICENSE",
    "README.md"
  ]

  spec.require_paths = [ "lib" ]
  spec.extensions = [ "ext/lopdf_rb/extconf.rb" ]

  spec.add_dependency "rb_sys", "~> 0.9"
end
