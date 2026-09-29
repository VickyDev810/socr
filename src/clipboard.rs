use std::io::Write;
use std::process::{Command, Stdio};

use crate::util::{which, Error};

/// Copy `text` to the system clipboard using whatever tool the session offers.
///
/// Clipboard tools (wl-copy, xclip, xsel) fork a background process that keeps
/// serving the selection after we exit, so their stdout/stderr must not be
/// piped to us or we would block waiting for EOF.
pub fn copy(text: &str) -> Result<(), Error> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();

    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &["--type", "text/plain;charset=utf-8"]));
    }
    if x11 {
        candidates.push(("xclip", &["-selection", "clipboard", "-in"]));
        candidates.push(("xsel", &["--clipboard", "--input"]));
    }

    for (bin, args) in &candidates {
        if !which(bin) {
            continue;
        }
        let mut child = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
        if child.wait()?.success() {
            return Ok(());
        }
    }

    let session = crate::util::session_name();
    Err(Error::msg(match (wayland, x11) {
        (true, _) => format!("{session} detected, no clipboard tool: {}", crate::util::install_hint(&["wl-clipboard"])),
        (false, true) => format!("{session} detected, no clipboard tool: {}", crate::util::install_hint(&["xclip"])),
        _ => "no graphical session found (WAYLAND_DISPLAY and DISPLAY are unset)".to_string(),
    }))
}
