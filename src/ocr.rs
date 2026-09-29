use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::util::{install_hint, which, Error};

/// Run tesseract on an in-memory image (PGM/PNG/JPEG/... anything leptonica reads).
pub fn recognize(image: &[u8], lang: &str, psm: u8) -> Result<String, Error> {
    if !which("tesseract") {
        return Err(Error::msg(format!("OCR engine missing: {}", install_hint(&["tesseract", "tesseract-lang-eng"]))));
    }

    let mut cmd = Command::new("tesseract");
    if let Some(dir) = bundled_tessdata(lang) {
        cmd.arg("--tessdata-dir").arg(dir);
    }
    let mut child = cmd
        .args(["stdin", "stdout", "--dpi", "300", "-l", lang, "--psm", &psm.to_string()])
        .args(["-c", "preserve_interword_spaces=1"])
        // OpenMP thread spin-up costs more than it saves on screenshot-sized
        // images; a single thread is noticeably faster end to end.
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write on a separate thread so a large image can't deadlock against a full stdout pipe.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let data = image.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));

    let out = child.wait_with_output()?;
    let _ = writer.join();

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("Failed loading language") || err.contains("Error opening data file") {
            let missing: Vec<String> = lang.split('+').filter(|l| !installed_langs().iter().any(|i| i == l)).map(|l| format!("tesseract-lang-{l}")).collect();
            let mut missing: Vec<&str> = missing.iter().map(String::as_str).collect();
            if missing.is_empty() {
                missing.push("tesseract-lang-eng");
            }
            return Err(Error::msg(format!("OCR language '{lang}' not installed: {}", install_hint(&missing))));
        }
        let last = err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("unknown error");
        return Err(Error::msg(format!("tesseract failed: {last}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn installed_langs() -> Vec<String> {
    Command::new("tesseract")
        .arg("--list-langs")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().skip(1).map(str::to_string).collect())
        .unwrap_or_default()
}

/// socr's own model directory (user dir first, then the one socr-lite ships),
/// used when it holds every requested language; otherwise tesseract's default.
fn bundled_tessdata(lang: &str) -> Option<PathBuf> {
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("socr/tessdata"));
    user.into_iter()
        .chain([PathBuf::from("/usr/share/socr/tessdata")])
        .find(|dir| lang.split('+').all(|l| dir.join(format!("{l}.traineddata")).is_file()))
}
