// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
// Workspace placement and the adopt/release rules are adapted from deskpilot
// (MIT, Copyright (c) 2026 Cillian Berragan); see THIRD_PARTY_NOTICES.md.

//! Agent-owned windows on a hidden Hyprland special workspace.
//!
//! An agent window lives on `special:cua-<session>`: mapped and capturable
//! but not shown on any output, so it never takes the user's view, pointer
//! or focus. Windows are launched there with an exec rule, or adopted (moved)
//! from another workspace and released back to where they came from.
//!
//! Hyprland 0.57 replaced string dispatchers with Lua ones, so every action
//! tries the Lua form first and the legacy string form second. A dispatch
//! reply is never trusted alone: placement is confirmed by reading the
//! window's workspace back by exact address.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::hyprland;

/// Name prefix of every agent workspace.
const WORKSPACE_PREFIX: &str = "special:cua-";
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(2);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const LAUNCH_POLL: Duration = Duration::from_millis(100);
/// How long one clients snapshot answers the per-window `owned` filters.
const OWNED_SNAPSHOT_TTL: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, Deserialize)]
struct Workspace {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
}

/// The slice of a `j/clients` entry placement decisions read.
#[derive(Clone, Debug, Deserialize)]
struct Client {
    address: String,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    pid: i64,
    workspace: Workspace,
    #[serde(default)]
    xwayland: Option<bool>,
    #[serde(default)]
    grouped: Vec<String>,
    #[serde(default)]
    pinned: bool,
    /// 0 is the window with user focus.
    #[serde(rename = "focusHistoryID", default = "never_focused")]
    focus_history: i64,
}

fn never_focused() -> i64 {
    -1
}

impl Client {
    fn address(&self) -> Option<u64> {
        u64::from_str_radix(self.address.strip_prefix("0x")?, 16).ok()
    }
}

/// The compositor surface placement needs, so the rules can be tested
/// against a simulated compositor.
trait Compositor {
    fn clients(&self) -> Result<Vec<Client>>;
    /// Send one `dispatch` argument and return the raw reply.
    fn dispatch(&self, argument: &str) -> Result<String>;
}

struct Live;

impl Compositor for Live {
    fn clients(&self) -> Result<Vec<Client>> {
        hyprland::query("j/clients")
    }

    fn dispatch(&self, argument: &str) -> Result<String> {
        let mut ipc = hyprland::ipc_connection()?;
        let reply = hyprland::read_reply(
            &mut ipc,
            format!("dispatch {argument}").as_bytes(),
            DISPATCH_TIMEOUT,
        )?;
        Ok(String::from_utf8_lossy(&reply).trim().to_owned())
    }
}

/// Where a window was before the agent placed it, and where it was put.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Placement {
    /// Workspace to give the window back to; `None` for a window launched
    /// straight onto the agent workspace, which has nowhere to go back to.
    origin: Option<String>,
    target: String,
}

#[derive(Default)]
struct Placements {
    placed: Mutex<HashMap<u64, Placement>>,
}

static LIVE_PLACEMENTS: std::sync::LazyLock<Placements> =
    std::sync::LazyLock::new(Placements::default);
static AGENT_SNAPSHOT: Mutex<Option<(Instant, HashSet<u64>)>> = Mutex::new(None);

/// `special:cua-<session>`, with the session reduced to `[A-Za-z0-9_.-]`.
pub fn workspace_name(session: &str) -> Result<String> {
    let label: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        bail!("an agent workspace needs a non-empty session label");
    }
    Ok(format!("{WORKSPACE_PREFIX}{label}"))
}

fn is_agent_workspace(name: &str) -> bool {
    name.starts_with(WORKSPACE_PREFIX)
}

fn lua_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Run the Lua dispatcher, then the string one (Hyprland before 0.57). Only
/// an `ok` reply counts. The Lua form is written to be valid on 0.57, where
/// it is rejected before anything runs, so the fallback never repeats an
/// action that already happened.
fn dispatch_either(c: &dyn Compositor, lua: &str, legacy: &str) -> Result<()> {
    let lua_reply = match c.dispatch(lua) {
        Ok(reply) if reply == "ok" => return Ok(()),
        Ok(reply) => reply,
        Err(error) => format!("{error:#}"),
    };
    match c.dispatch(legacy) {
        Ok(reply) if reply == "ok" => Ok(()),
        Ok(reply) => bail!("Hyprland refused `{lua}` ({lua_reply}) and `{legacy}` ({reply})"),
        Err(error) => bail!("Hyprland refused `{lua}` ({lua_reply}); `{legacy}` failed: {error:#}"),
    }
}

fn find(clients: &[Client], address: u64) -> Option<&Client> {
    clients.iter().find(|c| c.address() == Some(address))
}

/// Start `cmd` (a shell command line) on this session's agent workspace
/// without showing it, and return the address and pid of the window that
/// mapped there. Fails loudly when nothing maps or the window lands elsewhere
/// (a single-instance app hands the request to a running process).
pub fn launch(cmd: &str, session: &str) -> Result<(u64, u32)> {
    launch_with(&Live, &LIVE_PLACEMENTS, cmd, session, LAUNCH_TIMEOUT)
}

fn launch_with(
    c: &dyn Compositor,
    placements: &Placements,
    cmd: &str,
    session: &str,
    timeout: Duration,
) -> Result<(u64, u32)> {
    let target = workspace_name(session)?;
    let before: HashSet<u64> = c.clients()?.iter().filter_map(Client::address).collect();
    // `silent` rides inside the workspace string: as a table key 0.57 rejects
    // it yet still runs the command with no rule, opening a window in view.
    let lua = format!(
        "hl.dsp.exec_cmd({}, {{ workspace = {} }})",
        lua_string(cmd),
        lua_string(&format!("{target} silent"))
    );
    let legacy = format!("exec [workspace {target} silent] {cmd}");
    dispatch_either(c, &lua, &legacy)?;

    let deadline = Instant::now() + timeout;
    let mut elsewhere = None;
    loop {
        for client in c.clients()? {
            let Some(address) = client.address() else {
                continue;
            };
            if !client.mapped || before.contains(&address) {
                continue;
            }
            if client.workspace.name == target {
                let pid = u32::try_from(client.pid).context("launched window reports no pid")?;
                placements.record(address, None, target);
                return Ok((address, pid));
            }
            elsewhere = Some((address, client.workspace.name.clone()));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(LAUNCH_POLL);
    }
    match elsewhere {
        Some((address, workspace)) => bail!(
            "a new window 0x{address:x} mapped on workspace '{workspace}', not {target}; it was not placed"
        ),
        None => bail!(
            "no new window mapped on {target} within {}s (a single-instance app may have handed the request to a running process)",
            timeout.as_secs()
        ),
    }
}

/// Move the window with this exact address onto the session's agent
/// workspace, remembering where it came from. Refuses XWayland, grouped,
/// pinned and focused windows, and any window already on an agent workspace.
pub fn adopt(address: u64, session: &str) -> Result<()> {
    adopt_with(&Live, &LIVE_PLACEMENTS, address, session)
}

fn adopt_with(
    c: &dyn Compositor,
    placements: &Placements,
    address: u64,
    session: &str,
) -> Result<()> {
    let target = workspace_name(session)?;
    let clients = c.clients()?;
    let client = find(&clients, address)
        .filter(|client| client.mapped)
        .with_context(|| format!("no mapped window 0x{address:x} to adopt"))?;
    if client.xwayland != Some(false) {
        bail!("window 0x{address:x} is not proven native Wayland; only native windows are adopted");
    }
    if !client.grouped.is_empty() {
        bail!("window 0x{address:x} is grouped; ungroup it before adopting");
    }
    if client.pinned {
        bail!("window 0x{address:x} is pinned and stays on every workspace");
    }
    if client.focus_history == 0 {
        bail!("window 0x{address:x} has user focus; adopting it would move what the user is using");
    }
    if is_agent_workspace(&client.workspace.name) {
        bail!(
            "window 0x{address:x} is already on {}",
            client.workspace.name
        );
    }
    // A numbered workspace goes back by id; a special or named one by name.
    let origin = if client.workspace.id > 0 {
        client.workspace.id.to_string()
    } else {
        client.workspace.name.clone()
    };
    move_window(c, address, &target)?;
    placements.record(address, Some(origin), target);
    Ok(())
}

/// Give an adopted window back to the workspace it came from, but only if it
/// is still where the agent put it: a window the user has since moved is left
/// alone. A window launched onto the agent workspace has no origin and is
/// only forgotten. Returns whether the window was moved.
pub fn release(address: u64) -> Result<bool> {
    release_with(&Live, &LIVE_PLACEMENTS, address)
}

fn release_with(c: &dyn Compositor, placements: &Placements, address: u64) -> Result<bool> {
    let placement = placements
        .take(address)
        .with_context(|| format!("window 0x{address:x} is not owned by this driver"))?;
    let Some(origin) = placement.origin else {
        return Ok(false);
    };
    let clients = c.clients()?;
    let Some(client) = find(&clients, address) else {
        return Ok(false);
    };
    if client.workspace.name != placement.target {
        return Ok(false);
    }
    move_window(c, address, &origin)?;
    Ok(true)
}

/// Move one window to `workspace` without following it, then read the
/// placement back by address.
fn move_window(c: &dyn Compositor, address: u64, workspace: &str) -> Result<()> {
    let lua = format!(
        "hl.dsp.window.move({{ workspace = {}, follow = false, window = \"address:0x{address:x}\" }})",
        lua_string(workspace)
    );
    let legacy = format!("movetoworkspacesilent {workspace},address:0x{address:x}");
    dispatch_either(c, &lua, &legacy)?;
    let clients = c.clients()?;
    let landed = find(&clients, address).map(|client| &client.workspace);
    let expected = |w: &Workspace| w.name == workspace || w.id.to_string() == workspace;
    if !landed.is_some_and(expected) {
        bail!("window 0x{address:x} did not land on workspace {workspace}");
    }
    Ok(())
}

/// Whether the window is one the agent owns: placed by this driver, or on an
/// agent workspace (a driver restart keeps the windows, so the name counts).
pub fn owned(address: u64) -> bool {
    hyprland::is_session() && owned_addresses().contains(&address)
}

/// The tools' "is this window present" test: on screen or agent-owned.
pub fn present(window: &crate::x11::WindowInfo) -> bool {
    window.is_on_screen || owned(window.xid)
}

fn owned_addresses() -> HashSet<u64> {
    let mut snapshot = AGENT_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((taken, addresses)) = snapshot.as_ref() {
        if taken.elapsed() < OWNED_SNAPSHOT_TTL {
            return addresses.clone();
        }
    }
    let mut addresses = LIVE_PLACEMENTS.addresses();
    match Live.clients() {
        Ok(clients) => addresses.extend(
            clients
                .iter()
                .filter(|c| c.mapped && is_agent_workspace(&c.workspace.name))
                .filter_map(Client::address),
        ),
        Err(error) => tracing::warn!("agent workspace ownership unavailable: {error:#}"),
    }
    *snapshot = Some((Instant::now(), addresses.clone()));
    addresses
}

/// `activate_window` for a Hyprland target. An owned window needs no
/// activation: accessibility, menu and agent-seat input reach it where it is.
/// Any other window is focused by its exact address (never by title) and the
/// focus is read back.
pub fn activate(address: u64, pid: Option<u32>) -> Result<()> {
    if owned(address) {
        return Ok(());
    }
    focus_with(&Live, address)?;
    if hyprland::target_is_active(address, pid)? {
        return Ok(());
    }
    bail!("foreground_unavailable: Hyprland did not focus window 0x{address:x}");
}

fn focus_with(c: &dyn Compositor, address: u64) -> Result<()> {
    dispatch_either(
        c,
        &format!("hl.dsp.focus({{ window = \"address:0x{address:x}\" }})"),
        &format!("focuswindow address:0x{address:x}"),
    )
}

/// Move the pointer in layout coordinates. `dispatch movecursor` is gone on
/// 0.57, where the Lua form is the only one.
pub fn move_cursor(x: i64, y: i64) -> Result<()> {
    dispatch_either(
        &Live,
        &format!("hl.dsp.cursor.move({{ x = {x}, y = {y} }})"),
        &format!("movecursor {x} {y}"),
    )
}

impl Placements {
    fn record(&self, address: u64, origin: Option<String>, target: String) {
        self.placed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(address, Placement { origin, target });
    }

    fn take(&self, address: u64) -> Option<Placement> {
        self.placed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&address)
    }

    fn addresses(&self) -> HashSet<u64> {
        self.placed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A compositor holding a client list that dispatches mutate, with a
    /// switch for which dispatcher dialect it speaks.
    struct Fake {
        clients: RefCell<Vec<Client>>,
        lua: bool,
        log: RefCell<Vec<String>>,
        /// Address and workspace of the window an exec dispatch maps.
        spawns: RefCell<Option<(u64, String)>>,
    }

    fn client(address: u64, id: i64, name: &str) -> Client {
        Client {
            address: format!("0x{address:x}"),
            mapped: true,
            pid: 4242,
            workspace: Workspace {
                id,
                name: name.into(),
            },
            xwayland: Some(false),
            grouped: vec![],
            pinned: false,
            focus_history: 3,
        }
    }

    impl Fake {
        fn new(lua: bool, clients: Vec<Client>) -> Self {
            Self {
                clients: RefCell::new(clients),
                lua,
                log: RefCell::new(vec![]),
                spawns: RefCell::new(None),
            }
        }
    }

    impl Compositor for Fake {
        fn clients(&self) -> Result<Vec<Client>> {
            Ok(self.clients.borrow().clone())
        }

        fn dispatch(&self, argument: &str) -> Result<String> {
            self.log.borrow_mut().push(argument.to_owned());
            let is_lua = argument.starts_with("hl.");
            if is_lua != self.lua {
                return Ok("Invalid dispatcher".into());
            }
            if argument.contains("exec") {
                if let Some((address, workspace)) = self.spawns.borrow_mut().take() {
                    let id = if workspace.starts_with("special:") {
                        -99
                    } else {
                        1
                    };
                    self.clients
                        .borrow_mut()
                        .push(client(address, id, &workspace));
                }
            } else if argument.contains("move") && !argument.contains("cursor") {
                let address = argument.split("0x").nth(1).unwrap();
                let address = u64::from_str_radix(
                    address.trim_end_matches(|c: char| !c.is_ascii_hexdigit()),
                    16,
                )
                .unwrap();
                let workspace = if is_lua {
                    argument.split('"').nth(1).unwrap().to_owned()
                } else {
                    argument
                        .trim_start_matches("movetoworkspacesilent ")
                        .split(',')
                        .next()
                        .unwrap()
                        .to_owned()
                };
                for c in self.clients.borrow_mut().iter_mut() {
                    if c.address() == Some(address) {
                        c.workspace = Workspace {
                            id: workspace.parse().unwrap_or(-99),
                            name: workspace.clone(),
                        };
                    }
                }
            }
            Ok("ok".into())
        }
    }

    #[test]
    fn workspace_names_are_sanitised() {
        assert_eq!(workspace_name("a b/c").unwrap(), "special:cua-a_b_c");
        assert_eq!(workspace_name("ok-1.2_x").unwrap(), "special:cua-ok-1.2_x");
        assert!(workspace_name("").is_err());
    }

    #[test]
    fn lua_strings_escape_quotes_and_control_bytes() {
        assert_eq!(lua_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\001\"");
    }

    #[test]
    fn launch_puts_silent_inside_the_workspace_string_and_verifies_the_window() {
        let fake = Fake::new(true, vec![client(0x10, 1, "1")]);
        *fake.spawns.borrow_mut() = Some((0x20, "special:cua-s1".into()));
        let placements = Placements::default();
        let (address, pid) = launch_with(
            &fake,
            &placements,
            "gnome-text-editor",
            "s1",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!((address, pid), (0x20, 4242));
        let sent = fake.log.borrow()[0].clone();
        assert_eq!(
            sent,
            "hl.dsp.exec_cmd(\"gnome-text-editor\", { workspace = \"special:cua-s1 silent\" })"
        );
        assert!(!sent.contains("silent ="), "silent is never a table key");
        assert!(placements.addresses().contains(&0x20));
    }

    #[test]
    fn launch_falls_back_to_the_legacy_dispatcher() {
        let fake = Fake::new(false, vec![]);
        *fake.spawns.borrow_mut() = Some((0x20, "special:cua-s1".into()));
        launch_with(
            &fake,
            &Placements::default(),
            "calc",
            "s1",
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            fake.log.borrow().last().unwrap(),
            "exec [workspace special:cua-s1 silent] calc"
        );
    }

    #[test]
    fn launch_reports_a_window_that_mapped_elsewhere_and_one_that_never_mapped() {
        let fake = Fake::new(true, vec![]);
        *fake.spawns.borrow_mut() = Some((0x20, "3".into()));
        let error = launch_with(
            &fake,
            &Placements::default(),
            "x",
            "s1",
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("mapped on workspace '3'"));

        let fake = Fake::new(true, vec![]);
        let error = launch_with(
            &fake,
            &Placements::default(),
            "x",
            "s1",
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("no new window mapped"));
    }

    #[test]
    fn adopt_then_release_gives_back_only_when_untouched() {
        let fake = Fake::new(true, vec![client(0x10, 9, "9"), client(0x11, 9, "9")]);
        let placements = Placements::default();
        adopt_with(&fake, &placements, 0x10, "s1").unwrap();
        adopt_with(&fake, &placements, 0x11, "s1").unwrap();
        assert!(fake
            .clients
            .borrow()
            .iter()
            .all(|c| c.workspace.name == "special:cua-s1"));

        // The user moves 0x11 on their own: release leaves it alone.
        fake.clients.borrow_mut()[1].workspace = Workspace {
            id: 4,
            name: "4".into(),
        };
        assert!(release_with(&fake, &placements, 0x10).unwrap());
        assert!(!release_with(&fake, &placements, 0x11).unwrap());
        let clients = fake.clients.borrow();
        assert_eq!(clients[0].workspace.name, "9");
        assert_eq!(clients[1].workspace.name, "4");
        assert!(placements.addresses().is_empty());
    }

    #[test]
    fn adopt_refuses_what_it_must_not_move() {
        let placements = Placements::default();
        type Edit = Box<dyn Fn(&mut Client)>;
        let cases: Vec<(&str, Edit)> = vec![
            ("not proven native", Box::new(|c| c.xwayland = None)),
            ("grouped", Box::new(|c| c.grouped = vec!["0x1".into()])),
            ("pinned", Box::new(|c| c.pinned = true)),
            ("user focus", Box::new(|c| c.focus_history = 0)),
            (
                "already on",
                Box::new(|c| {
                    c.workspace = Workspace {
                        id: -99,
                        name: "special:cua-other".into(),
                    }
                }),
            ),
        ];
        for (reason, edit) in cases {
            let mut c = client(0x10, 1, "1");
            edit(&mut c);
            let fake = Fake::new(true, vec![c]);
            let error = adopt_with(&fake, &placements, 0x10, "s1").unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{reason}: {error:#}");
            assert!(fake.log.borrow().is_empty(), "{reason}: nothing dispatched");
        }
    }

    #[test]
    fn a_failed_move_is_not_recorded_as_placed() {
        struct Stuck(Fake);
        impl Compositor for Stuck {
            fn clients(&self) -> Result<Vec<Client>> {
                self.0.clients()
            }
            fn dispatch(&self, _: &str) -> Result<String> {
                Ok("ok".into())
            }
        }
        let stuck = Stuck(Fake::new(true, vec![client(0x10, 1, "1")]));
        let placements = Placements::default();
        let error = adopt_with(&stuck, &placements, 0x10, "s1").unwrap_err();
        assert!(format!("{error:#}").contains("did not land"));
        assert!(placements.addresses().is_empty());
    }

    #[test]
    fn focus_and_cursor_use_the_lua_form_first_and_then_the_legacy_one() {
        let fake = Fake::new(true, vec![]);
        focus_with(&fake, 0xabc).unwrap();
        assert_eq!(
            fake.log.borrow()[0],
            "hl.dsp.focus({ window = \"address:0xabc\" })"
        );
        let legacy = Fake::new(false, vec![]);
        focus_with(&legacy, 0xabc).unwrap();
        assert_eq!(legacy.log.borrow()[1], "focuswindow address:0xabc");
    }

    #[test]
    fn a_dispatch_both_dialects_refuse_is_an_error_naming_both_replies() {
        struct Refuses;
        impl Compositor for Refuses {
            fn clients(&self) -> Result<Vec<Client>> {
                Ok(vec![])
            }
            fn dispatch(&self, _: &str) -> Result<String> {
                Ok("bad".into())
            }
        }
        let error = focus_with(&Refuses, 1).unwrap_err();
        assert!(format!("{error:#}").contains("hl.dsp.focus"));
        assert!(format!("{error:#}").contains("focuswindow"));
    }
}
