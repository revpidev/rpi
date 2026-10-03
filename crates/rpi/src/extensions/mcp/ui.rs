//! The built-in `mcp` extension and its manager view.

use rpi_ext_host::api::ExtensionApi;

/// Register the built-in extension (full implementation lands with the
/// connection runtime).
pub fn create_mcp_extension(_api: ExtensionApi) -> Result<(), String> {
    Ok(())
}
