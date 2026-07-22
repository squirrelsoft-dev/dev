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

use cli::{BaseAction, Cli, Command, GlobalAction, VscodeAction};

/// How long a blocking task may hold the process up once the command is done.
///
/// Nothing legitimate is outstanding by then: every blocking task this CLI
/// spawns is awaited by the command that spawned it. What can still be parked
/// is a synchronous XPC send whose reply the daemon dropped — the readiness
/// gate and the exec probe bound their *awaits*, but dropping the future only
/// detaches the thread, it cannot cancel the send.
///
/// That thread must not decide whether the user sees anything. A dropped
/// runtime waits for its blocking pool, and `#[tokio::main]` drops it as a
/// temporary *before* `main`'s `Result` reaches anyhow's `Termination` — so a
/// parked send would hold the process at exit with nothing printed, which is
/// the silent hang of issue #4 wearing the gate's error message.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::ZERO;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

async fn run() -> anyhow::Result<()> {
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
        Command::Build {
            tag,
            no_cache,
            frozen_lockfile,
            buildkit,
            update_remote_user_uid_default,
            no_base,
        } => {
            commands::build::run(
                &workspace,
                runtime_override,
                tag.as_deref(),
                no_cache,
                verbose > 0,
                frozen_lockfile,
                buildkit,
                &update_remote_user_uid_default,
                no_base,
            )
            .await?;
        }
        Command::Up {
            rebuild,
            no_cache,
            frozen_lockfile,
            buildkit,
            update_remote_user_uid_default,
            port_overrides,
            no_base,
        } => {
            commands::up::run(
                &workspace,
                runtime_override,
                rebuild,
                no_cache,
                verbose > 0,
                frozen_lockfile,
                buildkit,
                &update_remote_user_uid_default,
                &port_overrides,
                no_base,
            )
            .await?;
        }
        Command::Down { remove } => {
            commands::down::run(&workspace, runtime_override, remove).await?;
        }
        Command::Exec { user, cmd } => {
            commands::exec::run(&workspace, runtime_override, user.as_deref(), &cmd).await?;
        }
        Command::Forward {
            port,
            name,
            keepalive,
            daemon,
            stop,
            list,
        } => {
            commands::forward::run(
                &workspace,
                runtime_override,
                &port,
                name.as_deref(),
                keepalive.as_deref(),
                daemon,
                stop,
                list,
            )
            .await?;
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
            BaseAction::Config {
                action: config_action,
            } => {
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
