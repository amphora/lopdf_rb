# Ruby bindings for the lopdf PDF library.
#
# Provides one class:
#   LopdfRb::Document — load, inspect, and save PDF documents
#
# The native extension is a Rust cdylib built via rb_sys and magnus.

require "json"
require_relative "lopdf_rb/version"

# Load the compiled Rust native extension. rb_sys places the .so under a
# Ruby-version-specific directory (e.g. lopdf_rb/3.3/lopdf_rb.so);
# fall back to the unversioned path for development builds.
begin
  ruby_api_version = RUBY_VERSION[/\d+\.\d+/]
  require "lopdf_rb/#{ruby_api_version}/lopdf_rb"
rescue LoadError
  begin
    require "lopdf_rb/lopdf_rb"
  rescue LoadError => e
    raise LoadError,
      "Failed to load lopdf_rb native extension. " \
      "Tried #{ruby_api_version}/ and unversioned paths. " \
      "Run `cd ext/lopdf_rb && cargo build --release` to compile. " \
      "Original error: #{e.message}"
  end
end
