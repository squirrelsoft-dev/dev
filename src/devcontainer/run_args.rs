//! Translate the supported subset of `devcontainer.json` `runArgs` into
//! container-create fields.
//!
//! Background — issue #5: `runArgs` was parsed into `config.run_args` and mapped
//! to `ContainerConfig.extra_args`, but `extra_args` was never consumed by any
//! runtime's create path. Every runArg — `--env-file`, `--env`, `--network`,
//! … — was silently dropped. This module restores the supported subset and
//! rejects everything else before side effects, so no applicable runArg is ever
//! silently ignored.
//!
//! ## Supported subset
//!
//! - `--env-file PATH` and `--env-file=PATH`
//! - `--env KEY=VALUE`, `--env=KEY=VALUE`, `-e KEY=VALUE`, and `-eKEY=VALUE`
//! - `--cap-add VALUE`, `--cap-add=VALUE`
//! - `--security-opt VALUE`, `--security-opt=VALUE`
//! - bare `--privileged` and `--init`
//!
//! ## Environment precedence (last value for a key wins)
//!
//! Image environment is lowest; Dev's effective create-time env map overlays it.
//! Docker CLI then processes all env files in their relative order before every
//! explicit `--env`/`-e` entry in its relative order, regardless of interleaving.
//! The caller inserts the ordered entries returned here into the env map after
//! Dev's create-time env map, so explicit env flags override duplicate file
//! values.
//!
//! ## Env-file format
//!
//! Docker-compatible, matching `docker/cli` `pkg/kvfile`:
//! - file must be valid UTF-8 (BOM on the first line is stripped);
//! - leading whitespace is trimmed per line; trailing whitespace is part of the
//!   value and kept;
//! - blank lines and lines whose first character is `#` are ignored;
//! - `KEY=VALUE` keeps the value as-is (no quoting, interpolation, or
//!   escaping — quotes are part of the value);
//! - a bare env-file `KEY` (no `=`) passes the host environment variable
//!   through: if the host var is set it becomes `KEY=value`, and if it is unset
//!   the key is omitted rather than invented as an empty value;
//! - a key may not be empty or contain whitespace.
//!
//! Relative env-file paths resolve against the workspace folder — the dev
//! equivalent of the directory `docker run` is invoked from, and the same
//! context `${localWorkspaceFolder}` substitution already resolves to.

use crate::error::DevError;
use std::path::{Path, PathBuf};

/// A parsed env entry: `(key, value)`. The returned list is already ordered by
/// Docker CLI precedence, so later entries win when inserted into a map.
pub type EnvEntry = (String, String);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedRunArgs {
    pub env: Vec<EnvEntry>,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub privileged: bool,
    pub init: bool,
}

/// Validate and translate the supported `runArgs` subset into the existing
/// container-create fields Dev already carries.
pub fn resolve_run_args(args: &[String], workspace: &Path) -> Result<ResolvedRunArgs, DevError> {
    let mut env_file_entries: Vec<EnvEntry> = Vec::new();
    let mut env_flag_entries: Vec<EnvEntry> = Vec::new();
    let mut resolved = ResolvedRunArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(path) = arg.strip_prefix("--env-file=") {
            let path = non_empty_inline_value(arg, path, "--env-file")?;
            read_env_file_into(
                &mut env_file_entries,
                &resolve_env_file_path(path, workspace),
            )?;
        } else if arg == "--env-file" {
            let path = next_value(args, &mut i, arg, "--env-file")?;
            reject_empty_value(arg, &path, "--env-file")?;
            read_env_file_into(
                &mut env_file_entries,
                &resolve_env_file_path(&path, workspace),
            )?;
        } else if let Some(rest) = arg.strip_prefix("--env=") {
            push_env_flag_token(&mut env_flag_entries, rest)?;
        } else if arg == "--env" {
            let token = next_value(args, &mut i, arg, "--env")?;
            push_env_flag_token(&mut env_flag_entries, &token)?;
        } else if arg == "-e" {
            let token = next_value(args, &mut i, arg, "-e")?;
            push_env_flag_token(&mut env_flag_entries, &token)?;
        } else if arg.starts_with("-env") {
            return Err(unsupported_flag_error(arg));
        } else if let Some(rest) = arg.strip_prefix("-e")
            && !rest.is_empty()
            && !rest.starts_with('-')
        {
            push_env_flag_token(&mut env_flag_entries, rest)?;
        } else if let Some(value) = arg.strip_prefix("--cap-add=") {
            resolved
                .cap_add
                .push(non_empty_inline_value(arg, value, "--cap-add")?.to_string());
        } else if arg == "--cap-add" {
            let value = next_value(args, &mut i, arg, "--cap-add")?;
            reject_empty_value(arg, &value, "--cap-add")?;
            resolved.cap_add.push(value);
        } else if let Some(value) = arg.strip_prefix("--security-opt=") {
            resolved
                .security_opt
                .push(non_empty_inline_value(arg, value, "--security-opt")?.to_string());
        } else if arg == "--security-opt" {
            let value = next_value(args, &mut i, arg, "--security-opt")?;
            reject_empty_value(arg, &value, "--security-opt")?;
            resolved.security_opt.push(value);
        } else if arg == "--privileged" {
            resolved.privileged = true;
        } else if arg.starts_with("--privileged=") {
            return Err(boolean_assignment_error(arg, "--privileged"));
        } else if arg == "--init" {
            resolved.init = true;
        } else if arg.starts_with("--init=") {
            return Err(boolean_assignment_error(arg, "--init"));
        } else {
            return Err(unsupported_flag_error(arg));
        }
        i += 1;
    }
    // Docker CLI reads every env-file first, then applies every explicit
    // --env/-e value after, regardless of interleaving. Preserve relative
    // order within each group.
    resolved.env = env_file_entries;
    resolved.env.extend(env_flag_entries);
    Ok(resolved)
}

/// Fetch the value token that follows a two-token flag, advancing the index so
/// the loop's trailing `i += 1` lands on the next flag.
fn next_value(
    args: &[String],
    i: &mut usize,
    flag: &str,
    flag_name: &str,
) -> Result<String, DevError> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| DevError::InvalidConfig(format!(
            "`runArgs` flag `{flag}` is missing its value; `{flag_name}` expects a value as the next argument"
        )))
}

fn non_empty_inline_value<'a>(
    flag: &str,
    value: &'a str,
    flag_name: &str,
) -> Result<&'a str, DevError> {
    if value.is_empty() {
        return Err(DevError::InvalidConfig(format!(
            "`runArgs` flag `{flag}` is missing its value; `{flag_name}` expects a non-empty value"
        )));
    }
    Ok(value)
}

fn reject_empty_value(flag: &str, value: &str, flag_name: &str) -> Result<(), DevError> {
    if value.is_empty() {
        return Err(DevError::InvalidConfig(format!(
            "`runArgs` flag `{flag}` is missing its value; `{flag_name}` expects a non-empty value"
        )));
    }
    Ok(())
}

/// Resolve an env-file path: absolute paths are used as-is; relative paths
/// resolve against the workspace folder.
fn resolve_env_file_path(path: &str, workspace: &Path) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        workspace.join(p)
    }
}

/// Parse a single `--env`/`-e` token (`KEY=VALUE` or bare `KEY`) and append it.
///
/// Docker forwards a bare unset `KEY` to the daemon; Dev fails safely instead
/// because the daemon API representation cannot express "explicitly unset this
/// key" without risking silently leaving an image value in place.
fn push_env_flag_token(out: &mut Vec<EnvEntry>, token: &str) -> Result<(), DevError> {
    if let Some((key, value)) = token.split_once('=') {
        if key.is_empty() {
            return Err(DevError::InvalidConfig(
                "invalid `runArgs` environment variable: empty name in `--env`/`-e` argument"
                    .to_string(),
            ));
        }
        out.push((key.to_string(), value.to_string()));
        return Ok(());
    }
    // Bare `KEY` — host pass-through. Fail safely when unset so Dev does not
    // silently leave an image-provided value in place.
    if token.is_empty() {
        return Err(DevError::InvalidConfig(
            "invalid `runArgs` environment variable: cannot be empty".to_string(),
        ));
    }
    let value = std::env::var(token).map_err(|_| {
        DevError::InvalidConfig(format!(
            "`runArgs` environment variable `{token}` uses host pass-through, but `{token}` is unset on the host; set it explicitly as `{token}=VALUE` or define it in `containerEnv`"
        ))
    })?;
    out.push((token.to_string(), value));
    Ok(())
}

/// Read an env file and append its entries to `out`, in file order.
fn read_env_file_into(out: &mut Vec<EnvEntry>, path: &Path) -> Result<(), DevError> {
    let bytes = std::fs::read(path).map_err(|e| {
        DevError::InvalidConfig(format!(
            "failed to read `runArgs` env-file `{}`: {e}",
            path.display()
        ))
    })?;
    let body = std::str::from_utf8(&bytes).map_err(|_| {
        // Do not print the bytes — they may be secret-adjacent.
        DevError::InvalidConfig(format!(
            "`runArgs` env-file `{}` is not valid UTF-8",
            path.display()
        ))
    })?;
    for item in parse_env_file_content(body) {
        let entry = item.map_err(|e| {
            DevError::InvalidConfig(format!(
                "`runArgs` env-file `{}` line {}: {}",
                path.display(),
                e.line,
                e.kind
            ))
        })?;
        out.push(entry);
    }
    Ok(())
}

/// A line-level parse error. The key is safe to include (it is not a value);
/// values are never included in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineError {
    EmptyName,
    WhitespaceInName(String),
}

impl std::fmt::Display for LineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LineError::EmptyName => write!(f, "variable name is empty"),
            LineError::WhitespaceInName(k) => {
                write!(f, "variable name `{k}` contains whitespace")
            }
        }
    }
}

/// A parse error annotated with its 1-based physical line number. The line
/// number counts every line in the file (including blanks and comments), so it
/// matches what a user sees in their editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineParseError {
    pub line: usize,
    pub kind: LineError,
}

/// Parse env-file content into ordered entries. Pure (no I/O) so it can be
/// unit-tested directly. BOM is stripped from the first line only.
///
/// Mirrors `docker/cli` `pkg/kvfile.parseKeyValueFile`:
/// - leading whitespace trimmed; trailing whitespace kept as part of the value;
/// - blank lines and `#`-prefixed lines ignored;
/// - `KEY=VALUE` kept as-is (no quoting/interpolation);
/// - bare `KEY` passes the host variable through (set → entry, unset → omitted).
///
/// Blank and comment lines are skipped (not yielded), so yielded errors carry
/// the physical line number they occurred on.
pub fn parse_env_file_content(content: &str) -> Vec<Result<EnvEntry, LineParseError>> {
    const BOM: &str = "\u{FEFF}";

    let mut out: Vec<Result<EnvEntry, LineParseError>> = Vec::new();
    for (idx, raw) in parse_env_file_lines(content).enumerate() {
        let line_no = idx + 1;
        // Strip a leading UTF-8 BOM from the first line only.
        let line = if idx == 0 {
            raw.strip_prefix(BOM).unwrap_or(raw)
        } else {
            raw
        };
        // Trim leading whitespace; trailing whitespace is part of the value.
        let trimmed_start = line.trim_start();
        if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
            continue;
        }
        match trimmed_start.split_once('=') {
            Some((key, value)) => {
                if key.is_empty() {
                    out.push(Err(LineParseError {
                        line: line_no,
                        kind: LineError::EmptyName,
                    }));
                    continue;
                }
                if key.chars().any(|c| c == ' ' || c == '\t') {
                    out.push(Err(LineParseError {
                        line: line_no,
                        kind: LineError::WhitespaceInName(key.to_string()),
                    }));
                    continue;
                }
                out.push(Ok((key.to_string(), value.to_string())));
            }
            None => {
                // Bare `KEY` — host pass-through. Omit when unset (never invent).
                if let Ok(v) = std::env::var(trimmed_start) {
                    out.push(Ok((trimmed_start.to_string(), v)));
                }
            }
        }
    }
    out
}

/// Split content into lines without the trailing newline/CRLF. `\r` is kept
/// here and trimmed as leading whitespace of the next line by `trim_start`,
/// matching Docker's bufio.Scanner which strips a trailing `\r` on the line.
fn parse_env_file_lines(content: &str) -> impl Iterator<Item = &str> + '_ {
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// Build the error for a runArg outside the supported subset.
fn unsupported_flag_error(flag: &str) -> DevError {
    // Strip a trailing `=value` so the message names the flag itself.
    let name = flag.split_once('=').map(|(n, _)| n).unwrap_or(flag);
    DevError::InvalidConfig(format!(
        "unsupported `runArgs` flag `{name}`: `dev` translates only the documented \
         `runArgs` subset into the container create request: `--env-file`, `--env`, `-e`, \
         `--cap-add`, `--security-opt`, `--privileged`, and `--init`. \
         Other Docker/Podman CLI flags have no direct daemon-API equivalent here; use the \
         equivalent first-class devcontainer property where Dev implements one (for example \
         `forwardPorts`, `mounts`, or `containerEnv`). See the README `runArgs` support matrix."
    ))
}

fn boolean_assignment_error(flag: &str, flag_name: &str) -> DevError {
    DevError::InvalidConfig(format!(
        "`runArgs` flag `{flag}` is not supported: `{flag_name}` is a boolean flag and must be supplied without `=VALUE`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn resolve(args: &[&str], workspace: &Path) -> Result<Vec<EnvEntry>, DevError> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        resolve_run_args(&args, workspace).map(|resolved| resolved.env)
    }

    fn env_map(entries: &[EnvEntry]) -> std::collections::HashMap<String, String> {
        entries.iter().cloned().collect()
    }

    #[test]
    fn empty_run_args_produces_no_env() {
        let ws = PathBuf::from("/ws");
        assert!(resolve(&[], &ws).unwrap().is_empty());
    }

    #[test]
    fn env_flag_long_space_form() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["--env", "A=1"], &ws).unwrap();
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn env_flag_long_equals_form() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["--env=B=2"], &ws).unwrap();
        assert_eq!(env_map(&e).get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn env_flag_short_space_form() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["-e", "C=3"], &ws).unwrap();
        assert_eq!(env_map(&e).get("C").map(String::as_str), Some("3"));
    }

    #[test]
    fn env_flag_short_attached_form() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["-eD=4"], &ws).unwrap();
        assert_eq!(env_map(&e).get("D").map(String::as_str), Some("4"));
    }

    #[test]
    fn env_flag_empty_value_is_kept() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["--env", "EMPTY="], &ws).unwrap();
        assert_eq!(env_map(&e).get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn env_flag_value_may_contain_equals() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["--env", "URL=postgres://u:p@h/db"], &ws).unwrap();
        assert_eq!(
            env_map(&e).get("URL").map(String::as_str),
            Some("postgres://u:p@h/db")
        );
    }

    #[test]
    fn env_flag_empty_name_is_rejected() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--env", "=super-secret"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("empty name"));
        assert!(
            !msg.contains("super-secret"),
            "inline env errors must not leak values: {msg}"
        );
    }

    #[test]
    fn env_flag_bare_key_errors_when_host_var_is_unset() {
        let ws = PathBuf::from("/ws");
        unsafe { std::env::remove_var("DEV_RUNARGS_UNSET_FLAG") };
        let err = resolve(&["--env", "DEV_RUNARGS_UNSET_FLAG"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("DEV_RUNARGS_UNSET_FLAG"),
            "error should name the missing host variable: {msg}"
        );
        assert!(
            msg.contains("unset"),
            "error should explain the host variable is unset: {msg}"
        );
    }

    #[test]
    fn repeated_env_flags_last_value_wins() {
        let ws = PathBuf::from("/ws");
        let e = resolve(&["--env", "K=1", "--env", "K=2", "-eK=3"], &ws).unwrap();
        // Last value for a key wins after merging.
        assert_eq!(env_map(&e).get("K").map(String::as_str), Some("3"));
    }

    #[test]
    fn env_flags_override_env_files_even_when_flags_come_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("vars.env"), "K=file\nOTHER=file\n").unwrap();
        let e = resolve(
            &[
                "--env",
                "K=flag",
                "--env-file",
                tmp.path().join("vars.env").to_str().unwrap(),
            ],
            tmp.path(),
        )
        .unwrap();

        let map = env_map(&e);
        assert_eq!(map.get("K").map(String::as_str), Some("flag"));
        assert_eq!(map.get("OTHER").map(String::as_str), Some("file"));
    }

    #[test]
    fn env_flag_missing_value_errors() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--env"], &ws).unwrap_err();
        assert!(format!("{err}").contains("--env"));
    }

    #[test]
    fn env_file_flag_missing_value_errors() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--env-file"], &ws).unwrap_err();
        assert!(format!("{err}").contains("--env-file"));
    }

    #[test]
    fn unsupported_flag_errors() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--network", "host"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("--network"), "named the flag: {msg}");
        assert!(msg.contains("runArgs"), "points at runArgs: {msg}");
    }

    #[test]
    fn unsupported_flag_with_equals_errors_naming_the_flag() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--add-host=host:1.2.3.4"], &ws).unwrap_err();
        assert!(format!("{err}").contains("--add-host"));
    }

    #[test]
    fn bare_dash_flag_is_unsupported() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["--network"], &ws).unwrap_err();
        assert!(format!("{err}").contains("--network"));
    }

    #[test]
    fn malformed_attached_env_spellings_are_not_reinterpreted() {
        let ws = PathBuf::from("/ws");
        let err = resolve(&["-envFOO=bar"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-envFOO"), "names original flag: {msg}");
        assert!(
            msg.contains("unsupported `runArgs` flag"),
            "malformed -e spelling must be rejected as an unsupported argument: {msg}"
        );

        let err = resolve(&["-e-file", "vars.env"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-e-file"), "names original flag: {msg}");
        assert!(
            !msg.contains("vars.env"),
            "must not shift the unsupported boundary to the next token: {msg}"
        );
    }

    #[test]
    fn cap_security_and_boolean_flags_are_accepted_by_resolved_run_args() {
        let ws = PathBuf::from("/ws");
        let args: Vec<String> = [
            "--cap-add",
            "SYS_PTRACE",
            "--cap-add=NET_ADMIN",
            "--security-opt",
            "seccomp=unconfined",
            "--security-opt=label=disable",
            "--privileged",
            "--init",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let resolved = super::resolve_run_args(&args, &ws).unwrap();
        assert_eq!(resolved.cap_add, vec!["SYS_PTRACE", "NET_ADMIN"]);
        assert_eq!(
            resolved.security_opt,
            vec!["seccomp=unconfined", "label=disable"]
        );
        assert!(resolved.privileged);
        assert!(resolved.init);
    }

    #[test]
    fn capability_flags_require_values_and_boolean_flags_reject_assignments() {
        let ws = PathBuf::from("/ws");
        let err = super::resolve_run_args(&["--cap-add".to_string()], &ws).unwrap_err();
        assert!(format!("{err}").contains("--cap-add"));

        let err = super::resolve_run_args(&["--security-opt=".to_string()], &ws).unwrap_err();
        assert!(format!("{err}").contains("--security-opt"));

        let err = super::resolve_run_args(&["--privileged=false".to_string()], &ws).unwrap_err();
        assert!(format!("{err}").contains("--privileged=false"));

        let err = super::resolve_run_args(&["--init=true".to_string()], &ws).unwrap_err();
        assert!(format!("{err}").contains("--init=true"));
    }

    // ---- env-file parsing (pure) ----

    fn parse(content: &str) -> Vec<EnvEntry> {
        parse_env_file_content(content)
            .into_iter()
            .map(|r| r.expect("line should parse"))
            .collect()
    }

    #[test]
    fn env_file_basic_key_value() {
        let e = parse("A=1\nB=two\n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
        assert_eq!(env_map(&e).get("B").map(String::as_str), Some("two"));
    }

    #[test]
    fn env_file_blank_and_comment_lines_ignored() {
        let e = parse("\n# a comment\n   \n#indented comment\nA=1\n");
        assert_eq!(e.len(), 1);
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn env_file_empty_value() {
        let e = parse("EMPTY=\n");
        assert_eq!(env_map(&e).get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn env_file_trailing_whitespace_is_part_of_value() {
        // Docker keeps trailing whitespace as part of the value.
        let e = parse("A=value   \n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("value   "));
    }

    #[test]
    fn env_file_leading_whitespace_trimmed() {
        let e = parse("   A=1\n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn env_file_quotes_are_part_of_value() {
        // Docker's kvfile does not strip quotes.
        let e = parse("A=\"quoted\"\n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("\"quoted\""));
    }

    #[test]
    fn env_file_bom_stripped_on_first_line() {
        let e = parse("\u{FEFF}A=1\nB=2\n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
        assert_eq!(env_map(&e).get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn env_file_crlf_line_endings() {
        let e = parse("A=1\r\nB=2\r\n");
        assert_eq!(env_map(&e).get("A").map(String::as_str), Some("1"));
        assert_eq!(env_map(&e).get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn env_file_empty_name_errors() {
        let err = parse_env_file_content("=value\n")
            .into_iter()
            .next()
            .unwrap()
            .unwrap_err();
        assert_eq!(err.line, 1);
        assert_eq!(err.kind, LineError::EmptyName);
    }

    #[test]
    fn env_file_whitespace_in_name_errors() {
        let err = parse_env_file_content("GOOD=1\nBAD KEY=value\n")
            .into_iter()
            .nth(1)
            .unwrap()
            .unwrap_err();
        assert_eq!(err.line, 2);
        assert!(matches!(err.kind, LineError::WhitespaceInName(_)));
    }

    #[test]
    fn env_file_bare_key_passes_through_host_var_when_set() {
        unsafe { std::env::set_var("DEV_RUNARGS_TEST_HOST", "hostval") };
        let e = parse("DEV_RUNARGS_TEST_HOST\n");
        unsafe { std::env::remove_var("DEV_RUNARGS_TEST_HOST") };
        assert_eq!(
            env_map(&e).get("DEV_RUNARGS_TEST_HOST").map(String::as_str),
            Some("hostval")
        );
    }

    #[test]
    fn env_file_bare_key_omitted_when_host_unset() {
        unsafe { std::env::remove_var("DEV_RUNARGS_TEST_UNSET") };
        let e = parse("DEV_RUNARGS_TEST_UNSET\n");
        // Unset host var must not invent an empty value.
        assert!(
            e.is_empty(),
            "unset host pass-through must be omitted, got {e:?}"
        );
    }

    #[test]
    fn env_file_last_value_for_key_wins_across_lines() {
        let e = parse("K=1\nK=2\n");
        assert_eq!(env_map(&e).get("K").map(String::as_str), Some("2"));
    }

    // ---- env-file I/O + path resolution ----

    #[test]
    fn resolve_env_file_relative_to_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "FROM_FILE=1\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let e = resolve(&["--env-file", ".devcontainer/.env"], &ws).unwrap();
        assert_eq!(env_map(&e).get("FROM_FILE").map(String::as_str), Some("1"));
    }

    #[test]
    fn resolve_env_file_absolute_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("abs.env");
        std::fs::write(&path, "ABS=yes\n").unwrap();
        let ws = PathBuf::from("/some/other/workspace");
        let e = resolve(&["--env-file", path.to_str().unwrap()], &ws).unwrap();
        assert_eq!(env_map(&e).get("ABS").map(String::as_str), Some("yes"));
    }

    #[test]
    fn resolve_env_file_equals_form_relative() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "EQ=1\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let e = resolve(&["--env-file=.devcontainer/.env"], &ws).unwrap();
        assert_eq!(env_map(&e).get("EQ").map(String::as_str), Some("1"));
    }

    #[test]
    fn missing_env_file_errors_naming_path() {
        let ws = PathBuf::from("/no/such/workspace");
        let err = resolve(&["--env-file", "missing.env"], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing.env"), "named the path: {msg}");
        assert!(msg.contains("env-file"), "says env-file: {msg}");
    }

    #[test]
    fn malformed_env_file_errors_naming_path_and_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bad.env");
        std::fs::write(&path, "GOOD=1\n=bad\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let err = resolve(&["--env-file", path.to_str().unwrap()], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bad.env"), "named the file: {msg}");
        assert!(msg.contains("line 2"), "named the line: {msg}");
        // Must not leak the value of any line.
        assert!(!msg.contains("=bad"));
    }

    #[test]
    fn non_utf8_env_file_errors_without_leaking_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("binary.env");
        std::fs::write(&path, b"OK=1\n\xff\xfeBAD\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let err = resolve(&["--env-file", path.to_str().unwrap()], &ws).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("UTF-8"), "says UTF-8: {msg}");
        assert!(!msg.contains('\u{FF}'), "does not leak raw bytes: {msg:?}");
    }

    #[test]
    fn env_file_then_env_flag_left_to_right_last_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "K=file\nOTHER=file\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let e = resolve(
            &["--env-file", ".devcontainer/.env", "--env", "K=flag"],
            &ws,
        )
        .unwrap();
        assert_eq!(env_map(&e).get("K").map(String::as_str), Some("flag"));
        assert_eq!(env_map(&e).get("OTHER").map(String::as_str), Some("file"));
    }

    #[test]
    fn repeated_env_files_left_to_right_last_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.env"), "S=1\n").unwrap();
        std::fs::write(dir.join("b.env"), "S=2\n").unwrap();
        let ws = tmp.path().to_path_buf();
        let e = resolve(
            &[
                "--env-file",
                ".devcontainer/a.env",
                "--env-file",
                ".devcontainer/b.env",
            ],
            &ws,
        )
        .unwrap();
        assert_eq!(env_map(&e).get("S").map(String::as_str), Some("2"));
    }
}
