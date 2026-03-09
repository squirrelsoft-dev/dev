pub mod config;
pub mod features;
pub mod lifecycle;
pub mod merge;
pub mod templates;
pub mod variables;

pub use config::DevcontainerConfig;
pub use features::{download_features, generate_feature_dockerfile, resolve_features, stage_feature_context};
pub use lifecycle::run_lifecycle_hooks;
pub use templates::apply_template;
pub use variables::{substitute_variables, substitute_variables_with_user};
