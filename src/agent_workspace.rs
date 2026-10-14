//! The sandboxed directory every octomind subprocess runs inside.
//!
//! `acp.rs` sets this as the agent's cwd and `--sandbox` root, so the agent's
//! filesystem writes stay here instead of landing in the user's home.
//! Octomind also discovers local tools at `<cwd>/.agents/tools/*` — running
//! inside our workspace would otherwise silently hide the tools the user
//! installed in their own `~/.agents/tools/`, so we symlink those in.
//!
//! A2UI surfaces do NOT go through here. `render_ui` is a tool on octoweb's
//! MCP server (see `mcp.rs`), so there is nothing to install for it.

use std::fs;
use std::path::{Path, PathBuf};

/// Octoweb's internal data root, and the cwd + `--sandbox` root the agent
/// runs inside. Derived from `~/.local/share` to match the project's existing
/// XDG-style layout.
pub fn workspace_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("octoweb")
}

/// Where octomind looks for local tools, given cwd = `workspace_dir()`.
fn tools_dir() -> PathBuf {
    workspace_dir().join(".agents").join("tools")
}

/// Create the workspace and mirror the user's own local tools into it.
/// Best-effort: a failure here only costs the user their custom tools, so we
/// log rather than propagate.
pub fn prepare() {
    if let Err(e) = fs::create_dir_all(tools_dir()) {
        tracing::warn!(error = %e, "agent workspace tools dir not created — user local tools unavailable");
        return;
    }
    remove_legacy_render_ui();
    link_user_tools();
}

/// Versions before `render_ui` moved onto the MCP server installed it as a
/// bash script polling a file queue — first at `~/.agents/tools/render_ui`,
/// later in this workspace's tools dir. Left in place, `link_user_tools`
/// mirrors the home copy in, octomind discovers it as a local tool next to
/// `octoweb:render_ui`, and the agent fails bootstrap on the duplicate tool
/// name. Delete every copy and both queues. A user's own `render_ui` is kept:
/// ours is recognised by its content.
fn remove_legacy_render_ui() {
    let home = dirs::home_dir();
    // Workspace entry first: it may be a symlink to the home copy, which is
    // only recognisable through the link while the target still exists.
    let scripts = [
        Some(tools_dir().join("render_ui")),
        home.as_ref()
            .map(|h| h.join(".agents").join("tools").join("render_ui")),
    ];
    for script in scripts.into_iter().flatten() {
        if !is_legacy_render_ui(&script) {
            continue;
        }
        match fs::remove_file(&script) {
            Ok(()) => tracing::info!(
                path = %script.display(),
                "removed legacy render_ui script — superseded by octoweb:render_ui"
            ),
            Err(e) => tracing::warn!(
                path = %script.display(),
                error = %e,
                "legacy render_ui script could not be removed"
            ),
        }
    }
    let _ = fs::remove_dir_all(workspace_dir().join("a2ui"));
    if let Some(home) = home {
        let _ = fs::remove_dir_all(home.join(".local").join("share").join("a2ui"));
    }
}

/// Every shipped version of the legacy script carried both markers: the A2UI
/// v0.9 description and a queue under `~/.local/share/`.
fn is_legacy_render_ui(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|script| {
        script.contains("A2UI v0.9") && script.contains("queue=\"${HOME}/.local/share/")
    })
}

/// Symlink `~/.agents/tools/*` into the workspace tools dir. Skips entries
/// that already exist; stale symlinks pointing at removed user tools are
/// cleaned up first.
fn link_user_tools() {
    let Some(home) = dirs::home_dir() else { return };
    let user_dir = home.join(".agents").join("tools");
    let ws_dir = tools_dir();

    // Remove dangling symlinks left from previously deleted user tools.
    if let Ok(entries) = fs::read_dir(&ws_dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_symlink() && fs::metadata(&p).is_err() {
                let _ = fs::remove_file(&p);
            }
        }
    }

    let Ok(entries) = fs::read_dir(&user_dir) else {
        return;
    };
    for ent in entries.flatten() {
        let src = ent.path();
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = ws_dir.join(name);
        if dst.symlink_metadata().is_ok() {
            continue;
        }
        if let Err(e) = std::os::unix::fs::symlink(&src, &dst) {
            tracing::warn!(src = %src.display(), error = %e, "user tool symlink failed");
        }
    }
}
