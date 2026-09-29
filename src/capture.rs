//! Interactive region capture across Linux desktops.
//!
//! No single tool works everywhere, so we pick per session:
//!   * wlroots-style Wayland (Hyprland, Sway, river, niri, Wayfire, labwc…): grim + slurp
//!   * KDE Plasma (Wayland or X11): spectacle
//!   * GNOME Wayland (and anything else): XDG Desktop Portal
//!   * X11: maim, scrot, xfce4-screenshooter, gnome-screenshot, ImageMagick import
//!   * flameshot anywhere it's installed
//! If a backend errors (e.g. the compositor lacks a protocol) the next one is tried.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::util::{which, Error};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Grim,
    Spectacle,
    GnomeScreenshot,
    Xfce,
    Maim,
    Scrot,
    Flameshot,
    Import,
    Portal,
}

use Backend::*;

const ALL: &[Backend] = &[
    Grim,
    Spectacle,
    GnomeScreenshot,
    Xfce,
    Maim,
    Scrot,
    Flameshot,
    Import,
    Portal,
];

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Grim => "grim",
            Spectacle => "spectacle",
            GnomeScreenshot => "gnome-screenshot",
            Xfce => "xfce4-screenshooter",
            Maim => "maim",
            Scrot => "scrot",
            Flameshot => "flameshot",
            Import => "import",
                    Portal => "portal",
        }
    }

    fn needs(self) -> &'static [&'static str] {
        match self {
            Grim => &["grim", "slurp"],
            Spectacle => &["spectacle"],
            GnomeScreenshot => &["gnome-screenshot"],
            Xfce => &["xfce4-screenshooter"],
            Maim => &["maim"],
            Scrot => &["scrot"],
            Flameshot => &["flameshot"],
            Import => &["import"],
                    Portal => &[],
        }
    }

    fn available(self) -> bool {
        self.needs().iter().all(|b| which(b))
    }

    fn capture(self) -> Result<Vec<u8>, Error> {
        match self {
            Grim => {
                let geom = run(&mut Command::new("slurp"), "slurp")?;
                let geom = String::from_utf8_lossy(&geom).trim().to_string();
                if geom.is_empty() {
                    return Err(Error::Cancelled);
                }
                // PPM skips PNG compression entirely — fastest path to pixels.
                run(Command::new("grim").args(["-g", &geom, "-t", "ppm", "-"]), "grim")
            }
            Spectacle => via_file("spectacle", &["-b", "-n", "-r", "-o"]),
            GnomeScreenshot => via_file("gnome-screenshot", &["-a", "-f"]),
            Xfce => via_file("xfce4-screenshooter", &["-r", "-s"]),
            Scrot => via_file("scrot", &["-s", "-o"]),
            Maim => run(Command::new("maim").args(["-s", "-u", "-f", "png"]), "maim"),
            Flameshot => run(Command::new("flameshot").args(["gui", "--raw"]), "flameshot"),
            Import => run(Command::new("import").arg("png:-"), "import"),
                    Portal => crate::dbus::screenshot(),
        }
    }
}

fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").or_else(|_| std::env::var("DESKTOP_SESSION")).unwrap_or_default().to_lowercase()
}

/// Compositors built on wlroots (or implementing its screencopy + layer-shell
/// protocols) where grim + slurp is the way to select a region. Their portal
/// backends can't select a region, so we never fall back to the portal there.
fn is_wlroots(de: &str) -> bool {
    const WLROOTS: &[&str] = &["hyprland", "sway", "river", "niri", "wayfire", "labwc", "dwl", "hikari", "qtile", "mango"];
    WLROOTS.iter().any(|w| de.contains(w))
        || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var_os("SWAYSOCK").is_some()
        || std::env::var_os("NIRI_SOCKET").is_some()
}

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Backends to try, best first, for the current session.
fn preferred() -> Vec<Backend> {
    let de = desktop();
    let kde = de.contains("kde") || de.contains("plasma");
    let gnome = de.contains("gnome") || de.contains("unity") || de.contains("budgie") || de.contains("pantheon");
    let xfce = de.contains("xfce");

    let mut order = Vec::new();
    if is_wayland() {
        if kde {
            order.push(Spectacle);
        }
            if gnome {
            // GNOME Shell doesn't expose wlr-screencopy; the portal is the sanctioned route.
            order.push(Portal);
        }
        order.extend([Grim, Flameshot]);
    } else {
        if kde {
            order.push(Spectacle);
        }
        if xfce {
            order.push(Xfce);
        }
        order.extend([Maim, Scrot, Flameshot]);
        if gnome || de.contains("cinnamon") || de.contains("mate") {
            order.push(GnomeScreenshot);
        }
        order.push(Import);
    }
    // Everything else as a last resort, portal last.
    // The portal can only select a region through GNOME's or KDE's own UI;
    // elsewhere (wlroots, X11 window managers) it grabs the whole screen.
    let portal_ok = gnome || kde || (is_wayland() && !is_wlroots(&de));
    order.extend(ALL.iter().copied().filter(|b| match b {
        GnomeScreenshot | Xfce => !is_wayland(),
        Portal => portal_ok,
        _ => true,
    }));
    let mut seen = Vec::new();
    order.retain(|b| {
        let new = !seen.contains(b);
        seen.push(*b);
        new
    });
    order
}

pub fn capture(forced: Option<&str>) -> Result<Vec<u8>, Error> {
    if let Some(name) = forced {
        let b = ALL.iter().copied().find(|b| b.name() == name).ok_or_else(|| {
            let names: Vec<_> = ALL.iter().map(|b| b.name()).collect();
            Error::msg(format!("unknown backend '{name}' (available: {})", names.join(", ")))
        })?;
        if !b.available() {
            return Err(Error::msg(format!("backend '{name}' needs: {}", b.needs().join(", "))));
        }
        return b.capture();
    }

    let mut errors = Vec::new();
    for b in preferred().into_iter().filter(|b| b.available()) {
        match b.capture() {
            Ok(img) if img.is_empty() => return Err(Error::Cancelled),
            Ok(img) => return Ok(img),
            Err(Error::Msg(e)) => errors.push(e),
            Err(e) => return Err(e),
        }
    }
    let hint = missing_tool_hint();
    Err(Error::msg(if errors.is_empty() { hint } else { format!("{hint} ({})", errors.join("; ")) }))
}

/// What to install for region capture in this session, e.g.
/// "Hyprland (Wayland) detected: install grim and slurp → sudo pacman -S grim slurp".
fn missing_tool_hint() -> String {
    let de = desktop();
    let session = crate::util::session_name();
    let (what, pkgs): (&str, &[&str]) = if !is_wayland() {
        ("a screenshot tool", &["maim"])
    } else if is_wlroots(&de) {
        ("grim and slurp", &["grim", "slurp"])
    } else if de.contains("kde") || de.contains("plasma") {
        ("the KDE screenshot portal", &["xdg-desktop-portal-kde"])
    } else if de.contains("gnome") {
        ("the GNOME screenshot portal", &["xdg-desktop-portal-gnome"])
    } else {
        ("grim and slurp", &["grim", "slurp"])
    };
    format!("{session} detected: install {what} → {}", crate::util::install_hint(pkgs))
}

pub fn list_backends() {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "session: {}", crate::util::session_name());
    let _ = writeln!(out, "auto order (first usable wins, falls through on error):");
    for b in preferred() {
        let mark = if b.available() { "✓" } else { "✗" };
        let needs = if b.needs().is_empty() { "d-bus".to_string() } else { b.needs().join(" + ") };
        let _ = writeln!(out, "  {mark} {:<20} needs {needs}", b.name());
    }
    if !preferred().iter().any(|b| b.available()) {
        let _ = writeln!(out, "\n{}", missing_tool_hint());
    }
}

/// Run a command, returning its stdout. A non-zero exit whose stderr mentions
/// cancelling is reported as a user cancel rather than an error.
fn run(cmd: &mut Command, name: &str) -> Result<Vec<u8>, Error> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| Error::msg(format!("{name}: {e}")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.to_lowercase().contains("cancel") || (err.trim().is_empty() && out.status.code() == Some(1)) {
        return Err(Error::Cancelled);
    }
    let last = err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("failed");
    Err(Error::msg(format!("{name}: {}", last.trim())))
}

/// For tools that can only write to a file: capture into a private temp file,
/// read it back and remove it. A missing/empty file means the user cancelled.
fn via_file(bin: &str, args: &[&str]) -> Result<Vec<u8>, Error> {
    let tmp = TempFile::new("png");
    let path = tmp.0.to_string_lossy().into_owned();
    run(Command::new(bin).args(args).arg(&path), bin)?;
    match std::fs::read(&tmp.0) {
        Ok(b) if !b.is_empty() => Ok(b),
        _ => Err(Error::Cancelled),
    }
}

struct TempFile(PathBuf);

impl TempFile {
    fn new(ext: &str) -> Self {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        TempFile(dir.join(format!("socr-{}-{nanos}.{ext}", std::process::id())))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
