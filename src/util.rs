use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The user aborted the selection.
    Cancelled,
    /// OCR produced no text.
    NoText,
    Msg(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Error::Msg(s.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Cancelled => f.write_str("selection cancelled"),
            Error::NoText => f.write_str("no text found"),
            Error::Msg(s) => f.write_str(s),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Msg(e.to_string())
    }
}

/// Is `name` an executable somewhere on $PATH?
pub fn which(name: &str) -> bool {
    find_in_path(name).is_some()
}

pub fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| {
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

/// Desktop notification over D-Bus (no-op if disabled or no server running).
pub fn notify(enabled: bool, title: &str, body: &str, error: bool) {
    if enabled {
        crate::dbus::notify(title, body, error);
    }
}

/// Human-readable session name for messages, e.g. "Hyprland (Wayland)".
pub fn session_name() -> String {
    let de = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let de = de.split(':').next().unwrap_or("").trim();
    let kind = if std::env::var_os("WAYLAND_DISPLAY").is_some() { "Wayland" } else { "X11" };
    if de.is_empty() { kind.to_string() } else { format!("{de} ({kind})") }
}

#[derive(Clone, Copy, PartialEq)]
enum Distro {
    Arch,
    Debian,
    Fedora,
    Suse,
    Void,
    Alpine,
    Gentoo,
    Nix,
    Other,
}

fn distro() -> Distro {
    let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        os.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim_matches('"').to_lowercase()).unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID="), field("ID_LIKE="));
    let has = |id: &str| ids.split_whitespace().any(|i| i == id);
    if has("arch") {
        Distro::Arch
    } else if has("debian") || has("ubuntu") {
        Distro::Debian
    } else if has("fedora") || has("rhel") {
        Distro::Fedora
    } else if has("suse") || has("opensuse") || ids.contains("opensuse") {
        Distro::Suse
    } else if has("void") {
        Distro::Void
    } else if has("alpine") {
        Distro::Alpine
    } else if has("gentoo") {
        Distro::Gentoo
    } else if has("nixos") {
        Distro::Nix
    } else {
        Distro::Other
    }
}

/// Distro-specific package name for tesseract or one of its language packs
/// (`tesseract-lang-<code>`); other tools share the same name everywhere.
fn package_name(d: Distro, pkg: &str) -> String {
    if let Some(lang) = pkg.strip_prefix("tesseract-lang-") {
        return match d {
            Distro::Arch => format!("tesseract-data-{lang}"),
            Distro::Debian => format!("tesseract-ocr-{lang}"),
            Distro::Fedora => format!("tesseract-langpack-{lang}"),
            Distro::Alpine => format!("tesseract-ocr-data-{lang}"),
            _ => format!("tesseract language data '{lang}'"),
        };
    }
    match (d, pkg) {
        (Distro::Debian | Distro::Suse | Distro::Void | Distro::Alpine, "tesseract") => "tesseract-ocr".into(),
        (Distro::Gentoo, "tesseract") => "app-text/tesseract".into(),
        _ => pkg.to_string(),
    }
}

/// "sudo pacman -S grim slurp" (or the equivalent for this distro).
pub fn install_hint(pkgs: &[&str]) -> String {
    let d = distro();
    let names: Vec<String> = pkgs.iter().map(|p| package_name(d, p)).collect();
    let names = names.join(" ");
    match d {
        Distro::Arch => format!("sudo pacman -S {names}"),
        Distro::Debian => format!("sudo apt install {names}"),
        Distro::Fedora => format!("sudo dnf install {names}"),
        Distro::Suse => format!("sudo zypper install {names}"),
        Distro::Void => format!("sudo xbps-install {names}"),
        Distro::Alpine => format!("doas apk add {names}"),
        Distro::Gentoo => format!("sudo emerge {names}"),
        Distro::Nix => format!("add {names} to your NixOS/home-manager packages"),
        Distro::Other => format!("install: {names}"),
    }
}

/// Short one-glance preview of the recognised text for the notification.
pub fn preview(text: &str) -> String {
    const MAX: usize = 120;
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(MAX).collect();
    if flat.chars().count() > MAX {
        out.push('…');
    }
    // notify-send bodies may be interpreted as markup.
    out.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Tidy tesseract output: drop form feeds and trailing spaces, collapse runs
/// of blank lines, optionally join wrapped lines into paragraphs.
pub fn clean(raw: &str, join: bool) -> String {
    let lines: Vec<&str> = raw.lines().map(|l| l.trim_end_matches(|c: char| c.is_whitespace() || c == '\x0c')).collect();

    let mut out = String::with_capacity(raw.len());
    if join {
        // Paragraphs are separated by blank lines; lines inside a paragraph are
        // joined with a space, or glued together when broken at a hyphen.
        for para in lines.split(|l| l.trim().is_empty()).filter(|p| !p.is_empty()) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            let mut buf = String::new();
            for line in para {
                let line = line.trim();
                if buf.ends_with('-') && line.starts_with(|c: char| c.is_lowercase()) {
                    buf.pop();
                } else if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(line);
            }
            out.push_str(&buf);
        }
    } else {
        let mut blank = 0;
        for line in lines {
            if line.trim().is_empty() {
                blank += 1;
                continue;
            }
            if !out.is_empty() {
                out.push_str(if blank > 0 { "\n\n" } else { "\n" });
            }
            blank = 0;
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collapses_blank_lines() {
        assert_eq!(clean("a  \n\n\n\nb\n\x0c", false), "a\n\nb");
    }

    #[test]
    fn clean_joins_paragraphs() {
        assert_eq!(clean("hello wor-\nld and\nmore\n\nnext", true), "hello world and more\n\nnext");
    }
}
