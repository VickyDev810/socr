//! socr — screenshot → OCR → clipboard.

mod bind;
mod capture;
mod clipboard;
mod image;
mod dbus;
mod ocr;
mod util;

use std::io::{Read, Write};
use std::process::ExitCode;

use util::Error;

const HELP: &str = "\
socr — select a screen region, OCR it, copy the text to the clipboard

USAGE:
    socr [OPTIONS]

OPTIONS:
    -l, --lang <LANGS>      Tesseract languages, e.g. eng or eng+deu  [env: SOCR_LANG, default: eng]
    -b, --backend <NAME>    Force a capture backend (see --list-backends)  [env: SOCR_BACKEND]
    -f, --file <PATH>       OCR an existing image instead of capturing ('-' = stdin)
    -j, --join              Join wrapped lines into paragraphs
    -p, --print             Print the text to stdout
    -n, --no-copy           Do not copy to the clipboard (implies --print)
    -q, --quiet             No desktop notification
        --psm <N>           Tesseract page segmentation mode (default: auto)
        --scale <F>         Upscale factor before OCR (default: auto)
        --raw               Skip image preprocessing
        --list-backends     Show capture backends and which are usable here
        --bind [KEYS]       Add a desktop hotkey for socr (default: super+shift+t)
                            Shows the changes and asks first; may replace an
                            existing binding for the same keys.
    -y, --yes               Don't ask for confirmation (with --bind)
    -h, --help              Show this help
    -V, --version           Show version

Exit status: 0 = text copied, 1 = error, 2 = selection cancelled, 3 = no text found.
";

pub struct Opts {
    lang: String,
    backend: Option<String>,
    file: Option<String>,
    join: bool,
    print: bool,
    copy: bool,
    notify: bool,
    psm: Option<u8>,
    scale: Option<f32>,
    raw: bool,
    bind: Option<String>,
    yes: bool,
}

fn parse_args() -> Result<Option<Opts>, Error> {
    let mut o = Opts {
        lang: std::env::var("SOCR_LANG").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "eng".into()),
        backend: std::env::var("SOCR_BACKEND").ok().filter(|s| !s.is_empty()),
        file: None,
        join: false,
        print: false,
        copy: true,
        notify: true,
        psm: None,
        scale: None,
        raw: false,
        bind: None,
        yes: false,
    };
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        // Support --opt=value as well as --opt value.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        if flag == "--bind" {
            // Optional value: `--bind`, `--bind super+x`, `--bind=super+x`.
            let keys = inline.or_else(|| args.next_if(|a| !a.starts_with('-')));
            o.bind = Some(keys.unwrap_or_else(|| "super+shift+t".into()));
            continue;
        }
        let mut value = |name: &str| -> Result<String, Error> {
            inline.clone().or_else(|| args.next()).ok_or_else(|| Error::msg(format!("{name} needs a value")))
        };
        match flag.as_str() {
            "-l" | "--lang" => o.lang = value("--lang")?,
            "-b" | "--backend" => o.backend = Some(value("--backend")?),
            "-f" | "--file" => o.file = Some(value("--file")?),
            "-j" | "--join" => o.join = true,
            "-p" | "--print" => o.print = true,
            "-n" | "--no-copy" => {
                o.copy = false;
                o.print = true;
            }
            "-q" | "--quiet" => o.notify = false,
            "--psm" => {
                o.psm = Some(value("--psm")?.parse().map_err(|_| Error::msg("--psm expects a number 0-13"))?)
            }
            "--scale" => {
                let s: f32 = value("--scale")?.parse().map_err(|_| Error::msg("--scale expects a number"))?;
                if !(0.25..=8.0).contains(&s) {
                    return Err(Error::msg("--scale must be between 0.25 and 8"));
                }
                o.scale = Some(s);
            }
            "--raw" => o.raw = true,
            "-y" | "--yes" => o.yes = true,
            "--list-backends" => {
                capture::list_backends();
                return Ok(None);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("socr {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            _ => return Err(Error::msg(format!("unknown argument '{arg}' (see --help)"))),
        }
    }
    Ok(Some(o))
}

fn run(o: &Opts) -> Result<String, Error> {
    let bytes = match o.file.as_deref() {
        Some("-") => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(path) => std::fs::read(path).map_err(|e| Error::msg(format!("cannot read {path}: {e}")))?,
        None => capture::capture(o.backend.as_deref())?,
    };
    if bytes.is_empty() {
        return Err(Error::Cancelled);
    }

    let text = if o.raw {
        ocr::recognize(&bytes, &o.lang, o.psm.unwrap_or(3))?
    } else {
        match image::decode(&bytes) {
            Ok(img) => {
                let prep = image::prepare(img, o.scale);
                let psm = o.psm.unwrap_or(prep.psm);
                ocr::recognize(&prep.pgm, &o.lang, psm)?
            }
            // Formats we don't decode ourselves (JPEG, WebP, ...) go to tesseract as-is.
            Err(_) => ocr::recognize(&bytes, &o.lang, o.psm.unwrap_or(3))?,
        }
    };

    let text = util::clean(&text, o.join);
    if text.is_empty() {
        return Err(Error::NoText);
    }
    Ok(text)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("socr: {e}");
            return ExitCode::from(1);
        }
    };

    if let Some(keys) = &opts.bind {
        return match bind::run(keys, opts.yes) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("socr: {e}");
                ExitCode::from(1)
            }
        };
    }

    match run(&opts) {
        Ok(text) => {
            if opts.print {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{text}");
            }
            if opts.copy {
                if let Err(e) = clipboard::copy(&text) {
                    eprintln!("socr: {e}");
                    util::notify(opts.notify, "OCR failed", &e.to_string(), true);
                    if !opts.print {
                        println!("{text}");
                    }
                    return ExitCode::from(1);
                }
            }
            if opts.copy {
                util::notify(opts.notify, "Text copied", &util::preview(&text), false);
            }
            ExitCode::SUCCESS
        }
        Err(Error::Cancelled) => ExitCode::from(2),
        Err(Error::NoText) => {
            eprintln!("socr: no text found");
            util::notify(opts.notify, "No text found", "Nothing recognisable in the selection", false);
            ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("socr: {e}");
            util::notify(opts.notify, "OCR failed", &e.to_string(), true);
            ExitCode::from(1)
        }
    }
}
