use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "dev", about = "Devcontainer management CLI", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Override workspace directory (default: current directory)
    #[arg(long, global = true)]
    pub workspace: Option<PathBuf>,

    /// Override container runtime
    #[arg(long, global = true, value_parser = ["docker", "podman", "apple"])]
    pub runtime: Option<String>,

    /// Increase verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a minimal .devcontainer/ with Dockerfile
    Init,

    /// Create .devcontainer/ from a template
    New {
        /// Template ID to use
        #[arg(long)]
        template: Option<String>,

        /// Template options as key=val pairs
        #[arg(long, value_delimiter = ',')]
        options: Vec<String>,
    },

    /// Build the devcontainer image
    Build {
        /// Tag for the built image
        #[arg(long)]
        tag: Option<String>,

        /// Don't use cache when building
        #[arg(long)]
        no_cache: bool,

        /// Error if lockfile is missing or features don't match it
        #[arg(long)]
        frozen_lockfile: bool,

        /// Use BuildKit-optimized feature installation
        #[arg(long)]
        buildkit: bool,

        /// Default policy for updating the remote user's UID/GID to match the host
        #[arg(long, value_parser = ["never", "on", "off"], default_value = "on")]
        update_remote_user_uid_default: String,
    },

    /// Start devcontainer for current directory
    Up {
        /// Rebuild container even if it exists
        #[arg(long)]
        rebuild: bool,

        /// Don't use cache when building
        #[arg(long)]
        no_cache: bool,

        /// Error if lockfile is missing or features don't match it
        #[arg(long)]
        frozen_lockfile: bool,

        /// Use BuildKit-optimized feature installation
        #[arg(long)]
        buildkit: bool,

        /// Default policy for updating the remote user's UID/GID to match the host
        #[arg(long, value_parser = ["never", "on", "off"], default_value = "on")]
        update_remote_user_uid_default: String,
    },

    /// Stop (optionally remove) container
    Down {
        /// Remove container after stopping
        #[arg(long)]
        remove: bool,
    },

    /// Run a command in the container
    Exec {
        /// User to run command as
        #[arg(short = 'u', long)]
        user: Option<String>,

        /// Command to run
        #[arg(required = true)]
        cmd: Vec<String>,
    },

    /// Open an interactive shell in the container
    Shell {
        /// Shell path to use
        #[arg(long)]
        shell: Option<String>,
    },

    /// Browse available templates and features
    List {
        /// What to list: templates or features
        #[arg(value_parser = ["templates", "features"])]
        kind: String,

        /// Search query
        #[arg(short = 'q', long)]
        query: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Force refresh of cached data
        #[arg(long)]
        refresh: bool,
    },

    /// Show container state for current directory
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// View or modify devcontainer configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },

    /// Manage global devcontainer templates
    Global {
        #[command(subcommand)]
        action: GlobalAction,
    },

    /// Manage base devcontainer configuration
    Base {
        #[command(subcommand)]
        action: BaseAction,
    },

    /// Open VS Code attached to the running devcontainer
    Open {
        /// Use VS Code Insiders
        #[arg(long)]
        insiders: bool,
    },

    /// VS Code integration
    Vscode {
        #[command(subcommand)]
        action: VscodeAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum VscodeAction {
    /// Repair VS Code symlinks for devcontainer configs
    Repair,
}

#[derive(Subcommand, Debug)]
pub enum GlobalAction {
    /// Create a new global template
    New {
        /// Template ID to use
        #[arg(long)]
        template: Option<String>,

        /// Name for the global template
        #[arg(long)]
        name: Option<String>,
    },

    /// List saved global templates
    List,

    /// Open a global template config in $EDITOR
    Edit {
        /// Name of the global template
        name: String,
    },

    /// Remove a global template
    Remove {
        /// Name of the global template
        name: String,
    },

    /// View or modify a global template's configuration
    Config {
        /// Name of the global template
        name: String,

        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

#[derive(Subcommand, Debug)]
pub enum BaseAction {
    /// Open base config in $EDITOR
    Edit,

    /// View or modify base configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Set a configuration property
    Set {
        /// Property name (e.g. image, remoteUser, postCreateCommand)
        property: String,
        /// Value to set
        value: String,
    },

    /// Remove a configuration property
    Unset {
        /// Property name to remove
        property: String,
    },

    /// Add a value to an array or map property
    Add {
        /// Property name (e.g. features, forwardPorts, remoteEnv, mounts)
        property: String,
        /// Value to add (OCI ref, port number, KEY=VALUE, or mount string)
        value: String,
    },

    /// Remove a value from an array or map property
    Remove {
        /// Property name (e.g. features, forwardPorts, remoteEnv, mounts)
        property: String,
        /// Value to remove
        value: String,
    },

    /// Show current configuration summary
    List,
}
