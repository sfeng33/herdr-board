//! [fork] Column `on_enter` hooks (`[[hook]]` in the daemon config).
//!
//! Auto columns: [`run_before_launch`] runs the matching hooks before a run's
//! agent starts and the launch waits for them. Manual columns: [`fire`] starts
//! them after the card lands and returns at once. See
//! [`board_core::config::HookDef`].
//!
//! A card's space cannot be edited while it has an open run, and an auto
//! column's run is already queued when its hook runs. So a hook that needs to
//! move the run elsewhere (e.g. into a worktree it just made) prints
//! `BOARD_SET_SPACE_REF=<workspace label>` and `BOARD_SET_SPACE_CWD=<dir>`
//! lines on stdout, and the launch applies them as a `new_workspace` space.
//! A `BOARD_SET_TASK_FILE=<file>` line makes that file's text the agent's
//! opening message, in place of the card description, comments and protocol
//! note (the column's system prompt still applies).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use board_core::config::HookDef;
use board_core::model::{Card, Column};

use crate::dispatch::board_env;
use crate::state::Daemon;

const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Everything one hook invocation needs, gathered under the store lock.
struct Invocation {
    hooks: Vec<HookDef>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn matches(hook: &HookDef, scope: Option<&str>, column: &str) -> bool {
    hook.column.eq_ignore_ascii_case(column)
        && hook.project.as_deref().is_none_or(|project| {
            scope.is_some_and(|scope| expand_home(project) == Path::new(scope))
        })
}

fn invocation(
    d: &Daemon,
    card: &Card,
    from: Option<&Column>,
    to: &Column,
) -> Result<Option<Invocation>> {
    let scope = d.store.lock().get_board(card.board_id)?.scope_path;
    let hooks: Vec<HookDef> = d
        .config
        .hook
        .iter()
        .filter(|hook| matches(hook, scope.as_deref(), &to.name))
        .cloned()
        .collect();
    if hooks.is_empty() {
        return Ok(None);
    }
    let mut env = board_env(card.id, &card.title, None, &d.socket_path)?;
    env.push(("BOARD_TO_COLUMN".into(), to.name.clone()));
    if let Some(from) = from {
        env.push(("BOARD_FROM_COLUMN".into(), from.name.clone()));
    }
    env.push(("BOARD_PROJECT".into(), scope.clone().unwrap_or_default()));
    let cwd = scope
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    Ok(Some(Invocation { hooks, env, cwd }))
}

/// A new space a hook asked the launch to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpaceRequest {
    pub(crate) space_ref: String,
    pub(crate) space_cwd: String,
}

/// What [`run_before_launch`] did.
pub(crate) struct LaunchHooks {
    /// At least one hook ran, so the card may have changed.
    pub(crate) ran: bool,
    /// The last space a hook asked for, if any.
    pub(crate) space: Option<SpaceRequest>,
    /// The last task file a hook asked for, if any.
    pub(crate) task_file: Option<String>,
}

/// The value of the last `KEY=value` line of `stdout` with `key` = `KEY=`.
fn line_value(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix(key))
        .next_back()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn space_request(stdout: &str) -> Option<SpaceRequest> {
    Some(SpaceRequest {
        space_ref: line_value(stdout, "BOARD_SET_SPACE_REF=")?,
        space_cwd: line_value(stdout, "BOARD_SET_SPACE_CWD=")?,
    })
}

/// Run one hook to completion, killing it after its timeout, and return its
/// stdout. The error names the command and carries the tail of its output.
fn run_one(hook: &HookDef, env: &[(String, String)], cwd: &Path) -> Result<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&hook.on_enter)
        .current_dir(cwd)
        .envs(env.iter().cloned())
        // The board env must not leak a run credential into a hook.
        .env_remove("BOARD_RUN_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("could not start `{}`: {e}", hook.on_enter))?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    };
    let out = read(Box::new(child.stdout.take().expect("piped stdout")));
    let err = read(Box::new(child.stderr.take().expect("piped stderr")));
    let limit = Duration::from_secs(hook.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{}` timed out after {}s", hook.on_enter, limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let output = format!("{stdout}{stderr}");
    tracing::info!(command = %hook.on_enter, %status, output = %output.trim(), "on_enter hook finished");
    if status.success() {
        return Ok(stdout);
    }
    let tail: Vec<&str> = output.trim().lines().rev().take(3).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    bail!(
        "`{}` failed ({status}): {}",
        hook.on_enter,
        tail.join(" | ")
    )
}

/// Auto column: run the hooks for `to` before the launch and wait for them.
/// The caller reloads the card when one ran, and applies the space a hook
/// asked for.
pub(crate) async fn run_before_launch(
    d: &Arc<Daemon>,
    card: &Card,
    to: &Column,
) -> Result<LaunchHooks> {
    let Some(inv) = invocation(d, card, None, to)? else {
        return Ok(LaunchHooks {
            ran: false,
            space: None,
            task_file: None,
        });
    };
    tokio::task::spawn_blocking(move || {
        let mut space = None;
        let mut task_file = None;
        for hook in &inv.hooks {
            let stdout = run_one(hook, &inv.env, &inv.cwd)?;
            space = space_request(&stdout).or(space);
            task_file = line_value(&stdout, "BOARD_SET_TASK_FILE=").or(task_file);
        }
        Ok(LaunchHooks {
            ran: true,
            space,
            task_file,
        })
    })
    .await
    .map_err(|e| anyhow!("on_enter hook task panicked: {e}"))?
}

/// Manual column: start the hooks for `to` in the background. Errors are
/// logged and never reach the caller, whose move already committed.
pub(crate) fn fire(d: &Daemon, card: &Card, from: Option<&Column>, to: &Column) {
    let inv = match invocation(d, card, from, to) {
        Ok(Some(inv)) => inv,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(card_id = card.id, error = %format!("{e:#}"), "on_enter hook lookup failed");
            return;
        }
    };
    let card_id = card.id;
    std::thread::spawn(move || {
        for hook in &inv.hooks {
            if let Err(e) = run_one(hook, &inv.env, &inv.cwd) {
                tracing::warn!(card_id, error = %format!("{e:#}"), "on_enter hook failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(project: Option<&str>, column: &str, command: &str) -> HookDef {
        HookDef {
            project: project.map(str::to_string),
            column: column.to_string(),
            on_enter: command.to_string(),
            timeout_secs: Some(5),
        }
    }

    #[test]
    fn matches_column_case_insensitively_within_the_project() {
        let h = hook(Some("/home/u/vllm"), "Plan", "true");
        assert!(matches(&h, Some("/home/u/vllm"), "plan"));
        assert!(!matches(&h, Some("/home/u/other"), "Plan"));
        assert!(!matches(&h, None, "Plan"));
        assert!(!matches(&h, Some("/home/u/vllm"), "Execute"));
        assert!(matches(&hook(None, "Plan", "true"), None, "Plan"));
    }

    #[test]
    fn space_request_needs_both_lines_and_takes_the_last() {
        assert_eq!(space_request("noise\nBOARD_SET_SPACE_REF=a\n"), None);
        assert_eq!(
            space_request("BOARD_SET_SPACE_REF=a\nBOARD_SET_SPACE_CWD=/x\nBOARD_SET_SPACE_REF=b\n"),
            Some(SpaceRequest {
                space_ref: "b".into(),
                space_cwd: "/x".into()
            })
        );
    }

    #[test]
    fn line_value_takes_the_last_non_empty_value() {
        let out = "BOARD_SET_TASK_FILE=/a\nBOARD_SET_TASK_FILE= /b \nother=1\n";
        assert_eq!(line_value(out, "BOARD_SET_TASK_FILE="), Some("/b".into()));
        assert_eq!(
            line_value("BOARD_SET_TASK_FILE=\n", "BOARD_SET_TASK_FILE="),
            None
        );
    }

    #[test]
    fn run_one_passes_env_and_reports_failures() {
        let dir = std::env::temp_dir();
        let env = vec![("BOARD_TO_COLUMN".to_string(), "Plan".to_string())];
        assert!(run_one(
            &hook(None, "Plan", r#"[ "$BOARD_TO_COLUMN" = Plan ]"#),
            &env,
            &dir
        )
        .is_ok());
        let err = run_one(&hook(None, "Plan", "echo boom >&2; exit 3"), &env, &dir).unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
        let mut slow = hook(None, "Plan", "sleep 5");
        slow.timeout_secs = Some(1);
        assert!(format!("{:#}", run_one(&slow, &env, &dir).unwrap_err()).contains("timed out"));
    }
}
