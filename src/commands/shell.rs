use std::path::Path;

use crate::devcontainer::compose::load_workspace_config_or_warn;
use crate::runtime::{ContainerState, detect_runtime, resolve_remote_user};
use crate::util::{workspace_folder_name, workspace_labels};

pub async fn run(
    workspace: &Path,
    runtime_override: Option<&str>,
    shell: Option<&str>,
) -> anyhow::Result<()> {
    let runtime = detect_runtime(runtime_override).await?;
    let labels = workspace_labels(workspace, None);
    let filters: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let containers = runtime.list_containers(&filters).await?;

    let container = containers
        .iter()
        .find(|c| c.state == ContainerState::Running)
        .ok_or_else(|| {
            anyhow::anyhow!("No running container found for this workspace. Run `dev up` first.")
        })?;

    // Resolve remoteUser and workspaceFolder from config or image metadata
    let config =
        load_workspace_config_or_warn(workspace, runtime.runtime_name()).map(|(_, config)| config);
    let config_user = config.as_ref().and_then(|c| c.remote_user.clone());
    let user =
        resolve_remote_user(runtime.as_ref(), &container.image, config_user.as_deref()).await?;

    let shell_cmd = if let Some(s) = shell {
        s.to_string()
    } else {
        // Probe for available shells
        let candidates = ["/bin/zsh", "/bin/bash", "/bin/sh"];
        let mut found = None;
        for candidate in &candidates {
            let probe = vec!["test".to_string(), "-x".to_string(), candidate.to_string()];
            let result = runtime
                .exec(&container.id, &probe, user.as_deref(), None)
                .await?;
            if result.exit_code == 0 {
                found = Some(candidate.to_string());
                break;
            }
        }
        found.unwrap_or_else(|| "/bin/sh".to_string())
    };

    // Resolve workspaceFolder the same way `dev up` does, so the shell starts
    // where lifecycle hooks ran.
    let workdir = match config.as_ref() {
        Some(config) => config.workspace_folder_path(workspace, user.as_deref())?,
        None => format!("/workspaces/{}", workspace_folder_name(workspace)),
    };

    let quoted_workdir = single_quoted(&workdir);
    let quoted_shell = single_quoted(&shell_cmd);
    let cmd = vec![
        shell_cmd.clone(),
        "-c".to_string(),
        format!(
            "cd {quoted_workdir} || \
             {{ printf 'dev: could not enter %s\\n' {quoted_workdir} >&2; exit 1; }}; \
             exec {quoted_shell} -l"
        ),
    ];
    let exit_code = runtime
        .exec_interactive(&container.id, &cmd, user.as_deref(), Some(&workdir))
        .await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Wrap a value the caller supplied so the guest's shell reads it as one word.
///
/// Both values interpolated into the `-c` script come from outside this
/// process: the working directory is resolved from `workspaceFolder` or from
/// the `target=` segment of `workspaceMount`, and the shell can be named
/// outright with `--shell`. A path holding a space would otherwise be split
/// (`cd /workspaces/My Projects/repo` enters `/workspaces/My`), and one holding
/// `;` or a backtick would run as a command. Single quotes suppress every
/// expansion the shell performs, so only the quote itself needs escaping — by
/// closing the run, emitting a literal quote, and reopening.
fn single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::single_quoted;

    #[test]
    fn a_quoted_value_survives_the_guest_shell_as_one_word() {
        assert_eq!(single_quoted("/workspaces/repo"), "'/workspaces/repo'");
        assert_eq!(
            single_quoted("/workspaces/My Projects/repo"),
            "'/workspaces/My Projects/repo'"
        );
    }

    /// The shell script this builds is the only place a `workspaceFolder` or a
    /// `--shell` value reaches a command line, so metacharacters must arrive as
    /// text rather than as syntax.
    #[test]
    fn quoting_leaves_no_metacharacter_live() {
        for hostile in [
            "/tmp; rm -rf /",
            "/tmp && whoami",
            "/tmp`id`",
            "/tmp$(id)",
            "/tmp\nid",
            "/tmp|id",
        ] {
            let quoted = single_quoted(hostile);
            assert_eq!(quoted, format!("'{hostile}'"));
        }

        // A quote of its own is the one character single quotes cannot carry,
        // so the run is closed, the quote emitted literally, and the run
        // reopened — never leaving the quoted state.
        assert_eq!(single_quoted("/tmp/it's"), r"'/tmp/it'\''s'");
        assert_eq!(single_quoted("';id;'"), r"''\'';id;'\'''");
    }
}
