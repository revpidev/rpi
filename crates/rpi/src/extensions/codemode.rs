//! The built-in `codemode` extension (port of
//! `packages/coding-agent/src/extensions/codemode/index.ts` @ a13d35a74).
//!
//! Registered inactive (`exposure: model-only`, `defaultActive: false`);
//! activation is the `defaultTools`/`--tools` governance surface (V16-13).

pub mod description;
pub mod execute;
pub mod tool;

use std::sync::Arc;

use rpi_ext_host::loader::InlineExtension;

pub use tool::{CodemodeMode, CodemodeSettings, CodemodeSettingsFn};

/// The built-in hidden `codemode` extension. `replaceable`/`builtin` naming
/// and the `-builtin:` disable surface are V16-13's governance scope.
pub fn inline_extension(
    settings: CodemodeSettingsFn,
    model_runtime: Arc<crate::core::model_runtime::ModelRuntime>,
) -> InlineExtension {
    InlineExtension::Named {
        name: "codemode".to_owned(),
        hidden: true,
        factory: Arc::new(move |api| {
            let settings = settings.clone();
            let model_runtime = model_runtime.clone();
            Box::pin(async move {
                api.register_tool(tool::create_codemode_tool_definition(
                    api.clone(),
                    model_runtime,
                    settings,
                ))
                .map_err(|error| error.to_string())?;
                Ok(())
            })
        }),
    }
}
