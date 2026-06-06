//! Devcontainer library — config parsing, layered merge, variable substitution,
//! and container runtime abstraction.
//!
//! Extracted from the `dev` CLI to enable programmatic use by tools like
//! fleet-commander.

pub mod devcontainer;
pub mod error;
pub mod oci;
pub mod runtime;
pub mod util;

// Re-export the most commonly used types at the crate root.
pub use devcontainer::config::DevcontainerConfig;
pub use devcontainer::jsonc::parse_jsonc;
pub use devcontainer::merge::{merge_layer, merge_layers};
pub use devcontainer::recipe::Recipe;
pub use devcontainer::variables::{substitute_variables, substitute_variables_with_user};
pub use error::DevError;
pub use runtime::{ContainerRuntime, detect_runtime};
