mod caddy;
mod cli;
mod collection;
mod commands;
mod devcontainer;
mod error;
mod oci;
mod runtime;
mod tui;
mod util;

use std::path::PathBuf;

use clap::Parser;

use cli::{Cli, BaseAction, Command, GlobalAction, VscodeAction};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let workspace = cli
        .workspace
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));

    let runtime_override = cli.runtime.as_deref();
    let verbose = cli.verbose;

    match cli.command {
        Command::Init => {
            commands::init::run(&workspace)?;
        }
        Command::New { template, options } => {
            commands::new::run(&workspace, template.as_deref(), &options, verbose).await?;
        }
        Command::Build { tag, no_cache, frozen_lockfile, buildkit, update_remote_user_uid_default } => {
            commands::build::run(&workspace, runtime_override, tag.as_deref(), no_cache, verbose > 0, frozen_lockfile, buildkit, &update_remote_user_uid_default).await?;
        }
        Command::Up { rebuild, no_cache, frozen_lockfile, buildkit, update_remote_user_uid_default, port_overrides } => {
            commands::up::run(&workspace, runtime_override, rebuild, no_cache, verbose > 0, frozen_lockfile, buildkit, &update_remote_user_uid_default, &port_overrides).await?;
        }
        Command::Down { remove } => {
            commands::down::run(&workspace, runtime_override, remove).await?;
        }
        Command::Exec { user, cmd } => {
            commands::exec::run(&workspace, runtime_override, user.as_deref(), &cmd).await?;
        }
        Command::Forward { port, daemon, stop, list } => {
            commands::forward::run(&workspace, runtime_override, &port, daemon, stop, list).await?;
        }
        Command::Shell { shell } => {
            commands::shell::run(&workspace, runtime_override, shell.as_deref()).await?;
        }
        Command::List {
            kind,
            query,
            json,
            refresh,
        } => {
            commands::list::run(&kind, query.as_deref(), json, refresh, verbose).await?;
        }
        Command::Open { insiders } => {
            commands::open::run(&workspace, runtime_override, insiders).await?;
        }
        Command::Status { json } => {
            commands::status::run(&workspace, runtime_override, json).await?;
        }
        Command::Config { action } => {
            commands::config::run_workspace(&workspace, action, verbose).await?;
        }
        Command::Base { action } => match action {
            BaseAction::New => {
                commands::base::new(verbose).await?;
            }
            BaseAction::Edit => {
                commands::base::edit()?;
            }
            BaseAction::Config { action: config_action } => {
                commands::base::config(config_action, verbose).await?;
            }
        },
        Command::Global { action } => match action {
            GlobalAction::New { template, name } => {
                commands::global::new(template.as_deref(), name.as_deref(), verbose).await?;
            }
            GlobalAction::List => {
                commands::global::list()?;
            }
            GlobalAction::Edit { name } => {
                commands::global::edit(&name)?;
            }
            GlobalAction::Remove { name } => {
                commands::global::remove(&name)?;
            }
            GlobalAction::Config { name, action } => {
                commands::global::config(&name, action, verbose).await?;
            }
        },
        Command::Vscode { action } => match action {
            VscodeAction::Repair => {
                commands::vscode::repair(&workspace)?;
            }
        },
    }

    Ok(())
}
