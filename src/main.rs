// vim-hypr-nav — Navigate between Hyprland windows and Vim/Neovim splits
// using the same keybindings. Requires the accompanying vim plugin.
// Usage: vim-hypr-nav u|r|d|l

use serde::Deserialize;
use std::{
    fs,
    os::unix::fs::FileTypeExt,
    path::PathBuf,
    process::{Command, ExitCode},
};

// ── types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ActiveWindow {
    pid: u32,
}

// ── entry point ──────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let dir = match args.get(1).map(String::as_str) {
        Some(d @ ("u" | "r" | "d" | "l")) => d,
        _ => {
            eprintln!("USAGE: {} u|r|d|l", args[0]);
            return ExitCode::FAILURE;
        }
    };

    // Try the vim/nvim path; fall back to hyprctl on any failure.
    if let Some(focused_pid) = focused_pid() {
        let pts = list_pts();
        if let Some(vim_pid) = find_descendant_vim_pid(focused_pid, &pts) {
            if try_vim_nav(vim_pid, dir) {
                return ExitCode::SUCCESS;
            }
        }
    }

    move_focus(dir);
    ExitCode::SUCCESS
}

// ── hyprctl helpers ──────────────────────────────────────────────────────────

/// Ask hyprctl for the focused window's PID. No jq needed.
fn focused_pid() -> Option<u32> {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    let win: ActiveWindow = serde_json::from_slice(&out.stdout).ok()?;
    if win.pid == 0 { None } else { Some(win.pid) }
}

/// Tell hyprctl to move focus in the given direction.
fn move_focus(dir: &str) {
    let _ = Command::new("hyprctl")
        .args(["dispatch", "movefocus", dir])
        .status();
}

// ── /dev/pts helpers ─────────────────────────────────────────────────────────

/// Collect all pts terminal names (e.g. "pts/0") into a Vec.
/// Replaces the `find /dev/pts -type c -not -name ptmx | sed | tr` pipeline.
fn list_pts() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/dev/pts") else {
        return vec![];
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name() != "ptmx"
                && e.file_type().map(|t| t.is_char_device()).unwrap_or(false)
        })
        .map(|e| format!("pts/{}", e.file_name().to_string_lossy()))
        .collect()
}

// ── /proc helpers ────────────────────────────────────────────────────────────

/// Read /proc/<pid>/comm and return it trimmed.
fn proc_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Read /proc/<pid>/cmdline (NUL-separated args).
fn proc_cmdline(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Parse the tty_nr field from /proc/<pid>/stat and convert to a "pts/N" string.
/// Returns None for non-pts or missing data.
fn proc_tty(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Skip past the closing ')' of the comm field (may contain spaces/parens).
    let after_comm = stat.rfind(')')?.checked_add(1)?;
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    // After ')': state ppid pgrp session tty_nr ...  → tty_nr is index 4
    let tty_nr: i64 = fields.get(4)?.parse().ok()?;
    if tty_nr == 0 {
        return None;
    }
    // Linux tty_nr encoding: bits 8-15 = major, bits 0-7 = minor
    let major = (tty_nr >> 8) & 0xff;
    let minor = tty_nr & 0xff;
    if major == 136 {
        Some(format!("pts/{minor}"))
    } else {
        None
    }
}

/// Return direct children of `pid` by scanning /proc.
fn children_of(pid: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|&child| {
            let stat = fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            let after = match stat.rfind(')') {
                Some(i) => i + 1,
                None => return false,
            };
            let mut fields = stat[after..].split_whitespace();
            fields.next(); // state
            fields
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .map(|ppid| ppid == pid)
                .unwrap_or(false)
        })
        .collect()
}

/// Return true if `comm` looks like a vim/nvim executable name.
fn is_vim_comm(comm: &str) -> bool {
    // Mirrors the shell regex: ^g?(view|n?vim?x?)(diff)?$
    let s = comm.trim_start_matches('g');
    let s = s.strip_suffix("diff").unwrap_or(s);
    let s = s.strip_suffix('x').unwrap_or(s);
    matches!(s, "vim" | "vi" | "nvim" | "view" | "nvi" | "nview")
}

/// Look for an `nvim --embed` child of `pid`.
fn find_embed_child(pid: u32) -> Option<u32> {
    children_of(pid).into_iter().find(|&child| {
        proc_comm(child).as_deref() == Some("nvim")
            && proc_cmdline(child).iter().any(|a| a == "--embed")
    })
}

/// Recursively walk the process tree rooted at `pid` looking for vim/nvim.
/// Only descends into children whose tty is in `pts`.
fn find_descendant_vim_pid(pid: u32, pts: &[String]) -> Option<u32> {
    if let Some(comm) = proc_comm(pid) {
        if is_vim_comm(&comm) {
            return Some(find_embed_child(pid).unwrap_or(pid));
        }
    }
    for child in children_of(pid) {
        let tty_ok = proc_tty(child)
            .map(|t| pts.contains(&t))
            .unwrap_or(true);
        if tty_ok {
            if let Some(found) = find_descendant_vim_pid(child, pts) {
                return Some(found);
            }
        }
    }
    None
}

// ── vim/nvim IPC ─────────────────────────────────────────────────────────────

/// Read the servername file and ask vim/nvim to handle the navigation.
/// Returns true only if vim successfully handled the keypress.
fn try_vim_nav(vim_pid: u32, dir: &str) -> bool {
    try_vim_nav_inner(vim_pid, dir).unwrap_or(false)
}

/// Inner helper so we can use `?` for early returns on Option.
fn try_vim_nav_inner(vim_pid: u32, dir: &str) -> Option<bool> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let server_file = PathBuf::from(runtime_dir)
        .join(format!("vim-hypr-nav.{vim_pid}.servername"));

    let contents = fs::read_to_string(&server_file).ok()?;
    let mut parts = contents.split_whitespace();
    let program = parts.next()?.to_owned();
    let servername = parts.next()?.to_owned();

    let server_arg = match program.as_str() {
        "vim" => "--servername",
        "nvim" => "--server",
        _ => return Some(false),
    };

    Some(
        Command::new(&program)
            .args([
                server_arg,
                servername.as_str(),
                "--remote-expr",
                &format!("VimHyprNav('{dir}')"),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    )
}
