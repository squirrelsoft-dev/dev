pub mod compose;
pub mod config;
pub mod features;
pub mod lifecycle;
pub mod merge;
pub mod recipe;
pub mod templates;
pub mod variables;

pub use compose::{compose_and_write, compose_config};
pub use config::DevcontainerConfig;
pub use features::{download_features, generate_feature_dockerfile, resolve_features, stage_feature_context};
pub use lifecycle::run_lifecycle_hooks;
pub use recipe::Recipe;
pub use templates::apply_template;
pub use variables::{substitute_variables, substitute_variables_with_user};
