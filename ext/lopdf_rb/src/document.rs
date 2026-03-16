use magnus::{Error, RModule};

/// Register the Document class under the LopdfRb module.
pub fn init(module: RModule) -> Result<(), Error> {
    let _ = module;
    Ok(())
}
