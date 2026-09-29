//! `socr --bind [KEYS]` — add a global hotkey for socr in the current desktop.
//!
//! Every desktop stores shortcuts differently, so we detect the session and
//! either edit its config file (after making a backup) or call its settings
//! tool. Nothing is changed without showing the plan and asking first.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{find_in_path, which, Error};

const MARKER: &str = "added by socr --bind";

#[derive(Clone, Copy, PartialEq)]
enum Mod {
    Super,
    Shift,
    Ctrl,
    Alt,
}

struct Keys {
    mods: Vec<Mod>,
    /// Canonical key: lowercase letter/digit, or a name like "Print" / "F5".
    key: String,
}

fn parse_keys(s: &str) -> Result<Keys, Error> {
    let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect();
    let (key, mods) = parts.split_last().ok_or_else(|| Error::msg("empty key combination"))?;
    let mods = mods
        .iter()
        .map(|m| match m.as_str() {
            "super" | "mod4" | "win" | "meta" | "logo" => Ok(Mod::Super),
            "shift" => Ok(Mod::Shift),
            "ctrl" | "control" => Ok(Mod::Ctrl),
            "alt" | "mod1" => Ok(Mod::Alt),
            _ => Err(Error::msg(format!("unknown modifier '{m}' (use super, shift, ctrl, alt)"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let key = match key.as_str() {
        "print" | "prtsc" | "printscreen" => "Print".to_string(),
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => k.to_string(),
        k if k.starts_with('f') && k[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => k.to_uppercase(),
        k => return Err(Error::msg(format!("unsupported key '{k}' (use a letter, digit, F1-F24 or Print)"))),
    };
    Ok(Keys { mods, key })
}

impl Keys {
    fn has(&self, m: Mod) -> bool {
        self.mods.contains(&m)
    }
    fn upper_key(&self) -> String {
        self.key.to_uppercase().replace("PRINT", "Print")
    }
    /// `SUPER SHIFT, T`
    fn hyprland(&self) -> String {
        let m: Vec<&str> = self.mods.iter().map(|m| match m {
            Mod::Super => "SUPER",
            Mod::Shift => "SHIFT",
            Mod::Ctrl => "CTRL",
            Mod::Alt => "ALT",
        }).collect();
        format!("{}, {}", m.join(" "), self.upper_key())
    }
    /// `Mod4+Shift+t` (sway, i3)
    fn sway(&self) -> String {
        self.joined(&["Mod4", "Shift", "Control", "Mod1"], "+", &self.key)
    }
    /// `Super+Shift+T` (niri)
    fn niri(&self) -> String {
        self.joined(&["Super", "Shift", "Ctrl", "Alt"], "+", &self.upper_key())
    }
    /// `Meta+Shift+T` (KDE)
    fn kde(&self) -> String {
        self.joined(&["Meta", "Shift", "Ctrl", "Alt"], "+", &self.upper_key())
    }
    /// `<Super><Shift>t` (GNOME, XFCE)
    fn gtk(&self) -> String {
        let m: String = self.mods.iter().map(|m| match m {
            Mod::Super => "<Super>",
            Mod::Shift => "<Shift>",
            Mod::Ctrl => "<Control>",
            Mod::Alt => "<Alt>",
        }).collect();
        format!("{m}{}", self.key)
    }
    /// river: (`Super+Shift`, `T`)
    fn river(&self) -> (String, String) {
        let m: Vec<&str> = self.mods.iter().map(|m| match m {
            Mod::Super => "Super",
            Mod::Shift => "Shift",
            Mod::Ctrl => "Control",
            Mod::Alt => "Alt",
        }).collect();
        (if m.is_empty() { "None".into() } else { m.join("+") }, self.upper_key())
    }
    fn joined(&self, names: &[&str; 4], sep: &str, key: &str) -> String {
        let mut parts: Vec<&str> = [Mod::Super, Mod::Shift, Mod::Ctrl, Mod::Alt]
            .iter()
            .zip(names)
            .filter(|(m, _)| self.has(**m))
            .map(|(_, n)| *n)
            .collect();
        parts.push(key);
        parts.join(sep)
    }
}

enum Step {
    /// Append `text` to an existing config file.
    Append(PathBuf, String),
    /// Insert `text` after the first line starting with `anchor` (or append `fallback`).
    InsertAfter(PathBuf, &'static str, String, String),
    /// Write a new file (only if it doesn't exist).
    Create(PathBuf, String),
    Run(Vec<String>),
}

pub fn run(keys: &str, assume_yes: bool) -> Result<(), Error> {
    let keys = parse_keys(keys)?;
    let cmd = socr_command();
    let de = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    let env = |k: &str| std::env::var_os(k).is_some();
    let cfg = config_dir();

    let (desktop, steps, note): (&str, Vec<Step>, &str) = if env("HYPRLAND_INSTANCE_SIGNATURE") || de.contains("hyprland") {
        let line = format!("\n# socr: OCR hotkey ({MARKER})\nbind = {}, exec, {cmd}\n", keys.hyprland());
        ("Hyprland", vec![Step::Append(cfg.join("hypr/hyprland.conf"), line)], "Hyprland reloads its config automatically.")
    } else if env("SWAYSOCK") || de.contains("sway") {
        let line = format!("\n# socr: OCR hotkey ({MARKER})\nbindsym {} exec {cmd}\n", keys.sway());
        ("Sway", vec![Step::Append(cfg.join("sway/config"), line), Step::Run(argv(&["swaymsg", "reload"]))], "")
    } else if env("I3SOCK") || de.contains("i3") {
        let path = [cfg.join("i3/config"), home().join(".i3/config")].into_iter().find(|p| p.is_file()).unwrap_or(cfg.join("i3/config"));
        let line = format!("\n# socr: OCR hotkey ({MARKER})\nbindsym {} exec --no-startup-id {cmd}\n", keys.sway());
        ("i3", vec![Step::Append(path, line), Step::Run(argv(&["i3-msg", "reload"]))], "")
    } else if env("NIRI_SOCKET") || de.contains("niri") {
        let bind = format!("    // socr: OCR hotkey ({MARKER})\n    {} {{ spawn \"{cmd}\"; }}\n", keys.niri());
        let block = format!("\nbinds {{\n{bind}}}\n");
        ("niri", vec![Step::InsertAfter(cfg.join("niri/config.kdl"), "binds {", bind, block)], "niri reloads its config automatically.")
    } else if de.contains("river") {
        let (m, k) = keys.river();
        let line = format!("\n# socr: OCR hotkey ({MARKER})\nriverctl map normal {m} {k} spawn {cmd}\n");
        (
            "river",
            vec![Step::Append(cfg.join("river/init"), line), Step::Run(argv(&["riverctl", "map", "normal", &m, &k, "spawn", &cmd]))],
            "",
        )
    } else if de.contains("gnome") || de.contains("unity") || de.contains("budgie") {
        ("GNOME", gnome_steps(&keys, &cmd)?, "")
    } else if de.contains("kde") || de.contains("plasma") {
        let tool = ["kwriteconfig6", "kwriteconfig5"].into_iter().find(|t| which(t)).ok_or_else(|| Error::msg("kwriteconfig6 not found"))?;
        let mut steps = Vec::new();
        let desktop_file = data_dir().join("applications/socr.desktop");
        if !Path::new("/usr/share/applications/socr.desktop").is_file() {
            steps.push(Step::Create(desktop_file, desktop_entry(&cmd)));
        }
        steps.push(Step::Run(argv(&[
            tool, "--file", "kglobalshortcutsrc", "--group", "services", "--group", "socr.desktop", "--key", "_launch", &keys.kde(),
        ])));
        ("KDE Plasma", steps, "If the shortcut doesn't work right away, log out and back in.")
    } else if de.contains("xfce") {
        let prop = format!("/commands/custom/{}", keys.gtk());
        ("XFCE", vec![Step::Run(argv(&["xfconf-query", "-c", "xfce4-keyboard-shortcuts", "-p", &prop, "-n", "-t", "string", "-s", &cmd]))], "")
    } else {
        return Err(Error::msg(format!(
            "don't know how to add shortcuts for '{}' — add one manually that runs: {cmd}",
            crate::util::session_name()
        )));
    };

    // Refuse to edit config files that don't exist (creating e.g. an empty sway
    // config would drop the system defaults) or that already have our bind.
    for step in &steps {
        if let Step::Append(p, _) | Step::InsertAfter(p, ..) = step {
            let text = std::fs::read_to_string(p).map_err(|e| Error::msg(format!("cannot read {}: {e}", p.display())))?;
            if let Some(line) = text.lines().skip_while(|l| !l.contains(MARKER)).nth(1) {
                println!("socr is already bound in {}:\n    {}\nEdit that line to change the key.", p.display(), line.trim());
                return Ok(());
            }
        }
    }

    println!("Desktop: {desktop}\nShortcut: {} → {cmd}\n\nPlanned changes:", keys.niri());
    for step in &steps {
        match step {
            Step::Append(p, text) | Step::InsertAfter(p, _, text, _) => {
                println!("  • add to {} (backup: {}):", p.display(), backup_path(p).display());
                text.lines().filter(|l| !l.trim().is_empty()).for_each(|l| println!("      {}", l.trim()));
            }
            Step::Create(p, _) => println!("  • create {}", p.display()),
            Step::Run(a) => println!("  • run: {}", a.join(" ")),
        }
    }
    println!(
        "\n⚠  Disclaimer: this changes your desktop configuration. If {} is already bound to\n   \
         something else, that binding may stop working or be replaced. Review your\n   \
         existing shortcuts first; config files are backed up before editing.",
        keys.niri()
    );

    if !assume_yes {
        if !std::io::stdin().is_terminal() {
            return Err(Error::msg("not a terminal; re-run with --yes to confirm"));
        }
        print!("\nProceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return Ok(());
        }
    }

    for step in &steps {
        apply(step)?;
    }
    println!("Done. Press {} to capture text.{}", keys.niri(), if note.is_empty() { String::new() } else { format!(" {note}") });
    Ok(())
}

fn apply(step: &Step) -> Result<(), Error> {
    let io = |p: &Path, e: std::io::Error| Error::msg(format!("{}: {e}", p.display()));
    match step {
        Step::Append(p, text) => {
            std::fs::copy(p, backup_path(p)).map_err(|e| io(p, e))?;
            let mut f = std::fs::OpenOptions::new().append(true).open(p).map_err(|e| io(p, e))?;
            f.write_all(text.as_bytes()).map_err(|e| io(p, e))?;
        }
        Step::InsertAfter(p, anchor, text, fallback) => {
            std::fs::copy(p, backup_path(p)).map_err(|e| io(p, e))?;
            let src = std::fs::read_to_string(p).map_err(|e| io(p, e))?;
            let out = match src.lines().position(|l| l.trim_start().starts_with(anchor)) {
                Some(i) => {
                    let mut lines: Vec<&str> = src.lines().collect();
                    lines.insert(i + 1, text.trim_end_matches('\n'));
                    lines.join("\n") + "\n"
                }
                None => src + fallback,
            };
            std::fs::write(p, out).map_err(|e| io(p, e))?;
        }
        Step::Create(p, text) => {
            if !p.exists() {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
                }
                std::fs::write(p, text).map_err(|e| io(p, e))?;
            }
        }
        Step::Run(a) => {
            let ok = Command::new(&a[0]).args(&a[1..]).status().map(|s| s.success()).unwrap_or(false);
            if !ok {
                return Err(Error::msg(format!("command failed: {}", a.join(" "))));
            }
        }
    }
    Ok(())
}

fn gnome_steps(keys: &Keys, cmd: &str) -> Result<Vec<Step>, Error> {
    const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
    const PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/socr/";
    let out = Command::new("gsettings")
        .args(["get", SCHEMA, "custom-keybindings"])
        .output()
        .map_err(|_| Error::msg("gsettings not found"))?;
    let current = String::from_utf8_lossy(&out.stdout);
    let mut paths: Vec<String> = current.split('\'').skip(1).step_by(2).map(str::to_string).collect();
    if !paths.iter().any(|p| p == PATH) {
        paths.push(PATH.to_string());
    }
    let list = format!("[{}]", paths.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", "));
    let item = format!("{SCHEMA}.custom-keybinding:{PATH}");
    Ok(vec![
        Step::Run(argv(&["gsettings", "set", &item, "name", "socr"])),
        Step::Run(argv(&["gsettings", "set", &item, "command", cmd])),
        Step::Run(argv(&["gsettings", "set", &item, "binding", &keys.gtk()])),
        Step::Run(argv(&["gsettings", "set", SCHEMA, "custom-keybindings", &list])),
    ])
}

fn desktop_entry(cmd: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=socr\nComment=Select a screen region and copy its text\n\
         Exec={cmd}\nIcon=edit-copy\nTerminal=false\nNoDisplay=false\nCategories=Utility;\n"
    )
}

/// "socr" if that's what $PATH resolves to, otherwise this binary's full path.
fn socr_command() -> String {
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let on_path = find_in_path("socr").and_then(|p| p.canonicalize().ok());
    match (me, on_path) {
        (Some(a), Some(b)) if a == b => "socr".into(),
        (Some(a), _) => a.to_string_lossy().into_owned(),
        _ => "socr".into(),
    }
}

fn backup_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".socr-bak");
    PathBuf::from(s)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"))
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_formats() {
        let k = parse_keys("Super+Shift+t").unwrap();
        assert_eq!(k.hyprland(), "SUPER SHIFT, T");
        assert_eq!(k.sway(), "Mod4+Shift+t");
        assert_eq!(k.niri(), "Super+Shift+T");
        assert_eq!(k.kde(), "Meta+Shift+T");
        assert_eq!(k.gtk(), "<Super><Shift>t");
        let p = parse_keys("print").unwrap();
        assert_eq!(p.hyprland(), ", Print");
        assert!(parse_keys("hyper+x").is_err());
    }
}
