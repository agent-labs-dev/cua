// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! `launch_app`'s Hyprland agent-workspace modes: `hidden` starts an app on a
//! special workspace nobody is looking at, `adopt` moves an existing native
//! window there and `release` gives it back. See
//! [`crate::wayland::hyprland_agent_ws`].

use cua_driver_core::{
    protocol::ToolResult,
    tool_args::{resolve_session_key, ArgsExt},
};
use serde_json::{json, Value};

use super::impl_::{chromium_family_program, window_record_json};
use crate::wayland::hyprland_agent_ws as agent_ws;

/// Whether `launch_app` was asked for one of the agent-workspace modes.
pub(super) fn requested(args: &Value) -> bool {
    args.opt_bool("hidden").unwrap_or(false)
        || args.opt_u64("adopt").is_some()
        || args.opt_u64("release").is_some()
}

pub(super) async fn run(args: Value) -> ToolResult {
    if !(crate::wayland::is_wayland() && crate::wayland::hyprland::is_session()) {
        return ToolResult::error("hidden, adopt and release need a Hyprland session");
    }
    let session = resolve_session_key(&args);
    let result = tokio::task::spawn_blocking(move || execute(&args, &session)).await;
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => ToolResult::error(format!("{error:#}")),
        Err(error) => ToolResult::error(format!("Task error: {error}")),
    }
}

fn execute(args: &Value, session: &str) -> anyhow::Result<ToolResult> {
    if let Some(window_id) = args.opt_u64("release") {
        let moved = agent_ws::release(window_id)?;
        return Ok(ToolResult::text(if moved {
            format!("Released window {window_id} to its workspace.")
        } else {
            format!("Window {window_id} is no longer agent-owned; it stays where it is.")
        })
        .with_structured(json!({ "window_id": window_id, "moved": moved })));
    }
    if let Some(window_id) = args.opt_u64("adopt") {
        agent_ws::adopt(window_id, session)?;
        return placed(window_id, None, session);
    }
    let command = launch_command(args)?;
    let (window_id, pid) = agent_ws::launch(&command, session)?;
    placed(window_id, Some(pid), session)
}

fn placed(window_id: u64, pid: Option<u32>, session: &str) -> anyhow::Result<ToolResult> {
    let window = crate::wayland::list_windows_dispatch(None)
        .into_iter()
        .find(|w| w.xid == window_id);
    let workspace = agent_ws::workspace_name(session)?;
    Ok(ToolResult::text(format!(
        "Window {window_id} is on the hidden workspace {workspace}."
    ))
    .with_structured(json!({
        "pid": pid.or(window.as_ref().and_then(|w| w.pid)),
        "window_id": window_id,
        "hidden": true,
        "workspace": workspace,
        "windows": window.iter().map(window_record_json).collect::<Vec<_>>(),
    })))
}

/// The shell command line for a hidden launch: `launch_path` or `name`
/// (a program on PATH, else a matching installed desktop app), with the
/// accessibility environment `launch_app` gives every child and the GTK
/// portal switches that keep file dialogs from opening on a user's screen.
fn launch_command(args: &Value) -> anyhow::Result<String> {
    let extra = args.str_array("additional_arguments");
    let given = args
        .opt_str("launch_path")
        .or_else(|| args.opt_str("name"))
        .ok_or_else(|| anyhow::anyhow!("hidden needs launch_path or name"))?;
    let program_exists = given.split_whitespace().next().is_some_and(on_path);
    let command = if program_exists {
        given.clone()
    } else {
        let installed = crate::installed_apps::list_installed_apps();
        super::launch_app::match_installed_app(&installed, &given)
            .map(|app| app.launch_path.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("'{given}' is not on PATH and matches no installed application")
            })?
    };
    let mut words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    words.extend(extra);
    if chromium_family_program(&words[0])
        && !words.iter().any(|w| w == "--force-renderer-accessibility")
    {
        words.push("--force-renderer-accessibility".into());
    }
    let words: Vec<String> = words.iter().map(|w| shell_quote(w)).collect();
    Ok(format!(
        "env ACCESSIBILITY_ENABLED=1 NO_AT_BRIDGE=0 GDK_DEBUG=no-portals GTK_USE_PORTAL=0 {}",
        words.join(" ")
    ))
}

fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &std::path::Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return executable(std::path::Path::new(program));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(program))))
}

fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_single_quoted_for_the_compositor_shell() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_hidden_launch_carries_the_accessibility_and_portal_environment() {
        let command = launch_command(&json!({
            "name": "sh",
            "additional_arguments": ["-c", "exit 0"],
        }))
        .unwrap();
        assert_eq!(
            command,
            "env ACCESSIBILITY_ENABLED=1 NO_AT_BRIDGE=0 GDK_DEBUG=no-portals GTK_USE_PORTAL=0 'sh' '-c' 'exit 0'"
        );
    }

    #[test]
    fn only_the_agent_workspace_arguments_select_this_route() {
        assert!(!requested(&json!({ "name": "sh" })));
        assert!(!requested(&json!({ "hidden": false })));
        assert!(requested(&json!({ "hidden": true })));
        assert!(requested(&json!({ "adopt": 5 })));
        assert!(requested(&json!({ "release": 5 })));
    }
}
