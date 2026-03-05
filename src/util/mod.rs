pub mod naming;
pub mod paths;
pub mod workspace;

pub use naming::{container_name, workspace_label};
pub use workspace::{find_devcontainer_config, workspace_folder_name};
