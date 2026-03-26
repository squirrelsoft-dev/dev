pub mod compose;
pub mod config;
pub mod features;
pub mod jsonc;
pub mod lifecycle;
pub mod lockfile;
pub mod merge;
pub mod recipe;
pub mod templates;
pub mod uid;
pub mod variables;

pub use compose::compose_and_write;
#[allow(unused_imports)]
pub use compose::compose_config;
pub use config::DevcontainerConfig;
pub use features::{
    download_features, merge_feature_capabilities, resolve_features, stage_feature_context,
};
pub use lifecycle::run_lifecycle_hooks;
#[allow(unused_imports)]
pub use lifecycle::run_post_attach_hooks;
pub use recipe::Recipe;
pub use templates::apply_template;
pub use variables::{substitute_variables, substitute_variables_with_user};
