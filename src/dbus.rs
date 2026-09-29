//! A tiny hand-rolled D-Bus client — just enough of the wire protocol for the
//! two calls socr makes, so no D-Bus library or helper binary is needed:
//!   * XDG Desktop Portal screenshots (GNOME, KDE, any xdg-desktop-portal backend)
//!   * desktop notifications (org.freedesktop.Notifications)

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

use crate::util::Error;

const PORTAL: &str = "org.freedesktop.portal.Desktop";

fn err(s: impl std::fmt::Display) -> Error {
    Error::msg(format!("portal: {s}"))
}

/// Show a desktop notification. Returns false if no notification server answered.
pub fn notify(summary: &str, body: &str, critical: bool) -> bool {
    let send = || -> Result<(), Error> {
        let (mut bus, _) = Bus::connect()?;
        // Notify(s app_name, u replaces_id, s icon, s summary, s body, as actions, a{sv} hints, i timeout)
        let mut w = Wr::default();
        w.str("socr");
        w.u32(0);
        w.str(if critical { "dialog-error" } else { "edit-copy" });
        w.str(summary);
        w.str(body);
        w.array(4, |_| {});
        w.array(8, |w| {
            w.entry("urgency", "y", |w| w.b.push(if critical { 2 } else { 0 }));
            w.entry("x-canonical-private-synchronous", "s", |w| w.str("socr"));
        });
        w.u32(if critical { 10_000 } else { 2_500 });
        bus.call(
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            "org.freedesktop.Notifications",
            "susssasa{sv}i",
            &w.b,
        )?;
        Ok(())
    };
    send().is_ok()
}

pub fn screenshot() -> Result<Vec<u8>, Error> {
    let (mut bus, sender) = Bus::connect()?;

    let token = format!("socr_{}", std::process::id());
    let predicted = format!("/org/freedesktop/portal/desktop/request/{}/{token}", sender.trim_start_matches(':').replace('.', "_"));

    // Subscribe before calling so the Response signal cannot be missed.
    let mut w = Wr::default();
    w.str("type='signal',interface='org.freedesktop.portal.Request',member='Response'");
    bus.call("/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", "org.freedesktop.DBus", "s", &w.b)?;

    // Screenshot(s parent_window, a{sv} options)
    let mut w = Wr::default();
    w.str("");
    w.array(8, |w| {
        w.entry("handle_token", "s", |w| w.str(&token));
        w.entry("interactive", "b", |w| w.u32(1));
        w.entry("modal", "b", |w| w.u32(1));
    });
    let reply = bus.call("/org/freedesktop/portal/desktop", "org.freedesktop.portal.Screenshot", "Screenshot", PORTAL, "sa{sv}", &w.b)?;
    let handle = Rd::new(&reply.body, reply.le).string().unwrap_or(predicted);

    // Wait (as long as the user takes to select) for our Response.
    let (code, uri) = loop {
        let m = bus.read()?;
        if m.kind == SIGNAL && m.member == "Response" && m.path == handle {
            break parse_response(&m).ok_or_else(|| err("malformed response"))?;
        }
    };
    match code {
        0 => {}
        1 => return Err(Error::Cancelled),
        _ => return Err(err("screenshot request failed")),
    }
    let uri = uri.ok_or_else(|| err("response had no image uri"))?;
    let file = percent_decode(uri.strip_prefix("file://").ok_or_else(|| err("unexpected uri"))?);
    let bytes = std::fs::read(&file).map_err(|e| err(format!("{file}: {e}")))?;
    // The portal saves a fresh file per request; don't litter the user's Pictures folder.
    let _ = std::fs::remove_file(&file);
    Ok(bytes)
}

/// Response(u code, a{sv} results) → (code, results["uri"]).
fn parse_response(m: &Msg) -> Option<(u32, Option<String>)> {
    let mut r = Rd::new(&m.body, m.le);
    let code = r.u32()?;
    let len = r.u32()? as usize;
    r.align(8);
    let end = r.pos + len;
    let mut uri = None;
    while r.pos < end {
        r.align(8);
        let key = r.string()?;
        let sig = r.sig()?;
        if key == "uri" && sig == "s" {
            uri = Some(r.string()?);
        } else {
            r.skip(sig.as_bytes())?;
        }
    }
    Some((code, uri))
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- minimal D-Bus connection -------------------------------------------------

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;
const SIGNAL: u8 = 4;

struct Msg {
    kind: u8,
    le: bool,
    path: String,
    member: String,
    error: String,
    reply_serial: u32,
    body: Vec<u8>,
}

struct Bus {
    sock: BufReader<UnixStream>,
    serial: u32,
}

impl Bus {
    /// Connect, authenticate and register; returns the bus and our unique name.
    fn connect() -> Result<(Self, String), Error> {
        let sock = Self::open().ok_or_else(|| err("cannot connect to the D-Bus session bus"))?;
        let mut sock = BufReader::new(sock);

        // SASL EXTERNAL auth with our uid, hex-encoded as ASCII.
        let uid = std::fs::metadata("/proc/self").map_err(err)?.uid();
        let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        sock.get_mut().write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes()).map_err(err)?;
        let mut line = String::new();
        sock.read_line(&mut line).map_err(err)?;
        if !line.starts_with("OK ") {
            return Err(err(format!("D-Bus auth rejected: {}", line.trim())));
        }
        sock.get_mut().write_all(b"BEGIN\r\n").map_err(err)?;
        let mut bus = Bus { sock, serial: 0 };
        let hello = bus.call("/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", "org.freedesktop.DBus", "", &[])?;
        let name = Rd::new(&hello.body, hello.le).string().ok_or_else(|| err("bad Hello reply"))?;
        Ok((bus, name))
    }

    fn open() -> Option<UnixStream> {
        let addrs = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
        for addr in addrs.split(';') {
            let Some(params) = addr.strip_prefix("unix:") else { continue };
            for kv in params.split(',') {
                if let Some(p) = kv.strip_prefix("path=") {
                    if let Ok(s) = UnixStream::connect(percent_decode(p)) {
                        return Some(s);
                    }
                } else if let Some(name) = kv.strip_prefix("abstract=") {
                    use std::os::linux::net::SocketAddrExt;
                    let a = std::os::unix::net::SocketAddr::from_abstract_name(percent_decode(name).as_bytes()).ok()?;
                    if let Ok(s) = UnixStream::connect_addr(&a) {
                        return Some(s);
                    }
                }
            }
        }
        let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        UnixStream::connect(std::path::Path::new(&dir).join("bus")).ok()
    }

    /// Send a method call and wait for its reply. Other messages that arrive
    /// meanwhile are dropped — the portal's Response can only come after the
    /// Screenshot call has returned its request handle.
    fn call(&mut self, path: &str, iface: &str, member: &str, dest: &str, sig: &str, body: &[u8]) -> Result<Msg, Error> {
        self.serial += 1;
        let serial = self.serial;
        let mut w = Wr::default();
        w.b.extend([b'l', METHOD_CALL, 0, 1]);
        w.u32(body.len() as u32);
        w.u32(serial);
        w.array(8, |w| {
            w.field(1, "o", path);
            w.field(2, "s", iface);
            w.field(3, "s", member);
            w.field(6, "s", dest);
            if !sig.is_empty() {
                w.field(8, "g", sig);
            }
        });
        w.pad(8);
        w.b.extend_from_slice(body);
        self.sock.get_mut().write_all(&w.b).map_err(err)?;

        loop {
            let m = self.read()?;
            if m.reply_serial != serial {
                continue;
            }
            return match m.kind {
                METHOD_RETURN => Ok(m),
                ERROR => {
                    let detail = Rd::new(&m.body, m.le).string().unwrap_or_default();
                    Err(err(format!("{member}: {} {detail}", m.error)))
                }
                _ => continue,
            };
        }
    }

    fn read(&mut self) -> Result<Msg, Error> {
        let mut head = [0u8; 16];
        self.sock.read_exact(&mut head).map_err(err)?;
        let le = head[0] == b'l';
        let num = |b: &[u8]| {
            let a = [b[0], b[1], b[2], b[3]];
            if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
        };
        let body_len = num(&head[4..8]) as usize;
        let fields_len = num(&head[12..16]) as usize;
        if body_len > 64 << 20 || fields_len > 64 << 20 {
            return Err(err("oversized D-Bus message"));
        }
        let fields_end = 16 + fields_len;
        let total = fields_end.div_ceil(8) * 8 + body_len;
        let mut buf = head.to_vec();
        buf.resize(total, 0);
        self.sock.read_exact(&mut buf[16..]).map_err(err)?;

        let mut m = Msg { kind: head[1], le, path: String::new(), member: String::new(), error: String::new(), reply_serial: 0, body: Vec::new() };
        let mut r = Rd { b: &buf, pos: 16, le };
        while r.pos < fields_end {
            r.align(8);
            let code = r.u8().ok_or_else(|| err("bad header"))?;
            let sig = r.sig().ok_or_else(|| err("bad header"))?;
            match (code, sig.as_str()) {
                (1, "o") => m.path = r.string().unwrap_or_default(),
                (3, "s") => m.member = r.string().unwrap_or_default(),
                (4, "s") => m.error = r.string().unwrap_or_default(),
                (5, "u") => m.reply_serial = r.u32().unwrap_or(0),
                _ => r.skip(sig.as_bytes()).ok_or_else(|| err("bad header"))?,
            }
        }
        m.body = buf[total - body_len..].to_vec();
        Ok(m)
    }
}

// ---- marshalling ----------------------------------------------------------------

#[derive(Default)]
struct Wr {
    b: Vec<u8>,
}

impl Wr {
    fn pad(&mut self, n: usize) {
        while self.b.len() % n != 0 {
            self.b.push(0);
        }
    }
    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.b.extend(v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.b.extend(s.as_bytes());
        self.b.push(0);
    }
    fn sig(&mut self, s: &str) {
        self.b.push(s.len() as u8);
        self.b.extend(s.as_bytes());
        self.b.push(0);
    }
    /// Array whose elements have alignment `elem_align`; length excludes the
    /// padding before the first element.
    fn array(&mut self, elem_align: usize, f: impl FnOnce(&mut Self)) {
        self.u32(0);
        let at = self.b.len() - 4;
        self.pad(elem_align);
        let start = self.b.len();
        f(self);
        let len = (self.b.len() - start) as u32;
        self.b[at..at + 4].copy_from_slice(&len.to_le_bytes());
    }
    /// Header field: struct (byte code, variant value).
    fn field(&mut self, code: u8, sig: &str, val: &str) {
        self.pad(8);
        self.b.push(code);
        self.sig(sig);
        if sig == "g" { self.sig(val) } else { self.str(val) }
    }
    /// a{sv} dict entry.
    fn entry(&mut self, key: &str, sig: &str, val: impl FnOnce(&mut Self)) {
        self.pad(8);
        self.str(key);
        self.sig(sig);
        val(self);
    }
}

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
    le: bool,
}

impl<'a> Rd<'a> {
    /// Reader over a message body (which always starts 8-aligned in the message).
    fn new(b: &'a [u8], le: bool) -> Self {
        Rd { b, pos: 0, le }
    }
    fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n) * n;
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let b = self.take(4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) })
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = String::from_utf8(self.take(n)?.to_vec()).ok()?;
        self.pos += 1;
        Some(s)
    }
    fn sig(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = String::from_utf8(self.take(n)?.to_vec()).ok()?;
        self.pos += 1;
        Some(s)
    }
    /// Skip over one value of each complete type in `sig`.
    fn skip(&mut self, mut sig: &[u8]) -> Option<()> {
        while !sig.is_empty() {
            let n = single_type_len(sig)?;
            self.skip_one(&sig[..n])?;
            sig = &sig[n..];
        }
        Some(())
    }
    fn skip_one(&mut self, t: &[u8]) -> Option<()> {
        match t[0] {
            b'y' => self.pos += 1,
            b'n' | b'q' => {
                self.align(2);
                self.pos += 2
            }
            b'b' | b'i' | b'u' | b'h' => {
                self.align(4);
                self.pos += 4
            }
            b'x' | b't' | b'd' => {
                self.align(8);
                self.pos += 8
            }
            b's' | b'o' => {
                self.string()?;
            }
            b'g' => {
                self.sig()?;
            }
            b'v' => {
                let s = self.sig()?;
                self.skip(s.as_bytes())?;
            }
            b'a' => {
                let len = self.u32()? as usize;
                self.align(alignment(t[1]));
                self.pos += len;
            }
            b'(' | b'{' => {
                self.align(8);
                self.skip(&t[1..t.len() - 1])?;
            }
            _ => return None,
        }
        (self.pos <= self.b.len()).then_some(())
    }
}

fn alignment(t: u8) -> usize {
    match t {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// Length of the first complete type in a signature.
fn single_type_len(sig: &[u8]) -> Option<usize> {
    match *sig.first()? {
        b'a' => Some(1 + single_type_len(&sig[1..])?),
        open @ (b'(' | b'{') => {
            let close = if open == b'(' { b')' } else { b'}' };
            let mut depth = 0;
            for (i, &c) in sig.iter().enumerate() {
                if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
            }
            None
        }
        _ => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_roundtrip() {
        // Response(u 0, {"color": <(ddd)>, "uri": <"file:///tmp/a%20b.png">})
        let mut w = Wr::default();
        w.u32(0);
        w.array(8, |w| {
            w.entry("color", "(ddd)", |w| {
                w.pad(8);
                w.b.extend([0u8; 24]);
            });
            w.entry("uri", "s", |w| w.str("file:///tmp/a%20b.png"));
        });
        let m = Msg { kind: SIGNAL, le: true, path: String::new(), member: String::new(), error: String::new(), reply_serial: 0, body: w.b };
        let (code, uri) = parse_response(&m).unwrap();
        assert_eq!(code, 0);
        assert_eq!(percent_decode(uri.unwrap().strip_prefix("file://").unwrap()), "/tmp/a b.png");
    }
}
