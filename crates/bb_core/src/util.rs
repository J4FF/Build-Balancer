// util -- the small things every program needs: output, time, sizes and durations as people
// write them, ids, relative paths that cannot leave their folder, the command line

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// text to standard output
pub fn out(s: &str) {
    plat::write_stdout(s.as_bytes());
}

/// text to standard error
pub fn err(s: &str) {
    plat::write_stderr(s.as_bytes());
}

/// seconds since 1970 (UTC)
pub fn now() -> i64 {
    plat::unix_time_ns().div_euclid(1_000_000_000)
}

/// milliseconds on a clock that only goes forward (for measuring, not for dates)
pub fn tick_ms() -> u64 {
    plat::time_us() / 1000
}

/// a byte count for people, binary units: "512 B", "1.5 GiB"
pub fn size_text(b: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", b)
    } else if v >= 100.0 {
        format!("{:.0} {}", v, U[i])
    } else {
        format!("{:.1} {}", v, U[i])
    }
}

/// a byte count as people write it: "8G", "512M", "1.5GiB", "200000" (bytes), binary units
pub fn size_parse(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.trim_end_matches("ib").trim_end_matches('b');
    let (num, mul) = match t.chars().last()? {
        'k' => (&t[..t.len() - 1], 1u64 << 10),
        'm' => (&t[..t.len() - 1], 1u64 << 20),
        'g' => (&t[..t.len() - 1], 1u64 << 30),
        't' => (&t[..t.len() - 1], 1u64 << 40),
        _ => (t, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if v.is_nan() || v < 0.0 || v * mul as f64 > 1e18 {
        return None;
    }
    Some((v * mul as f64) as u64)
}

/// a share of something: "50%" of `total`, or an absolute amount through `abs` ("8G", "6")
pub fn share_parse(s: &str, total: u64, abs: impl Fn(&str) -> Option<u64>) -> Option<u64> {
    match s.trim().strip_suffix('%') {
        Some(p) => {
            let p: f64 = p.trim().parse().ok()?;
            (0.0..=100.0).contains(&p).then(|| (total as f64 * p / 100.0) as u64)
        }
        None => abs(s),
    }
}

/// seconds of a duration as people write it: "90" or "90s", "15m", "2h", "1d"
pub fn dur_parse(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mul) = match t.chars().last()? {
        's' => (&t[..t.len() - 1], 1),
        'm' => (&t[..t.len() - 1], 60),
        'h' => (&t[..t.len() - 1], 3600),
        'd' => (&t[..t.len() - 1], 86_400),
        _ => (t.as_str(), 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mul)
}

/// milliseconds for people: "850 ms", "42.3 s", "3m 05s", "2h 07m"
pub fn dur_text(ms: u64) -> String {
    let s = ms / 1000;
    if ms < 1000 {
        format!("{} ms", ms)
    } else if s < 60 {
        format!("{}.{} s", s, ms % 1000 / 100)
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    }
}

/// how long ago a time was: "now", "12 s", "5 min", "3 h", "2 d", "never" for 0
pub fn ago_text(t: i64) -> String {
    if t <= 0 {
        return String::from("never");
    }
    let d = (now() - t).max(0);
    match d {
        0..=2 => String::from("now"),
        3..=119 => format!("{} s", d),
        120..=7199 => format!("{} min", d / 60),
        7200..=172_799 => format!("{} h", d / 3600),
        _ => format!("{} d", d / 86_400),
    }
}

/// the alphabet of ids and pairing codes, five bits a character: no 0 / O and no 1 / I, so a
/// code survives being read aloud and typed
pub const B32: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// `n` characters of the id alphabet from random bytes (five random bits each)
pub fn random_chars(n: usize) -> String {
    let mut raw = alloc::vec![0u8; n];
    plat::fill_random(&mut raw);
    raw.iter().map(|b| B32[(b & 31) as usize] as char).collect()
}

/// a new id: the prefix, a dash and ten random characters, lower case ("j-k7m2x9pq4d")
pub fn new_id(prefix: &str) -> String {
    format!("{}-{}", prefix, random_chars(10).to_ascii_lowercase())
}

/// whether a name is an id of ours: a prefix letter, a dash, lower-case letters and digits
pub fn id_ok(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 4 && b.len() <= 40 && b[0].is_ascii_lowercase() && b[1] == b'-' && b[2..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// a name for files and channels from any text: lower case, letters, digits, '-' and '_', at most 40
pub fn slug(name: &str) -> String {
    let mut s: String = name.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    String::from(&s[..s.len().min(40)])
}

/// Whether a path from another machine may be used below a folder here: relative with '/'
/// between its parts, none of them empty, "." or "..", nothing a file system reads specially
/// (a drive letter, a backslash, a NUL, a name Windows reserves or cuts).
pub fn rel_ok(p: &str) -> bool {
    if p.is_empty() || p.len() > 1024 || p.starts_with('/') {
        return false;
    }
    p.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part.chars().any(|c| matches!(c, '\\' | ':' | '\0' | '<' | '>' | '"' | '|' | '?' | '*') || (c as u32) < 32)
            && !part.ends_with(' ')
            && !part.ends_with('.')
            && !windows_device(part)
    })
}

/// a name Windows keeps for a device whatever its extension: "NUL", "aux.txt", "COM1"
fn windows_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0')
}

/// the path of a relative name ('/' between its parts) below a folder, in this system's form
pub fn below(root: &str, rel: &str) -> String {
    let mut p = String::from(root);
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p = plat::fs::join(&p, part);
    }
    p
}

/// whether a path names a place by itself ("/x", "C:\x", "\\server\share")
pub fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (cfg!(windows) && b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// a path as given on the command line, made absolute against the current folder
pub fn abs_path(p: &str) -> String {
    if absolute(p) { String::from(p) } else { plat::fs::join(&plat::cwd(), p) }
}

/// Write a file in one step (readers see the old content or all of the new), creating the
/// folders on the way to it when they are missing.
pub fn put_file(path: &str, data: &[u8]) -> bool {
    if plat::write_file(path, data) {
        return true;
    }
    let parent = plat::fs::parent(path);
    !parent.is_empty() && plat::file_exists(parent) == 0 && plat::mkdir(parent) && plat::write_file(path, data)
}

/// remove a folder with everything in it (links are removed, never followed) -> whether it is gone
pub fn remove_tree(path: &str) -> bool {
    let Some(m) = plat::fs::lstat(path) else { return true };
    if m.is_dir() && !m.is_symlink() {
        if let Some(list) = plat::fs::read_dir(path) {
            for e in list {
                remove_tree(&plat::fs::join(path, &e.name));
            }
        }
    } else if cfg!(windows) && m.attrs & 1 != 0 {
        // a read-only file (a checked-out git object, say) cannot be deleted as it is
        plat::fs::set_attrs(path, m.attrs & !1);
    }
    plat::delete(path)
}

/// the bytes below a folder (links not followed), stopping early once `stop_above` is exceeded
pub fn tree_size(path: &str, stop_above: u64) -> u64 {
    let mut total = 0u64;
    let mut todo = alloc::vec![String::from(path)];
    while let Some(dir) = todo.pop() {
        let Some(list) = plat::fs::read_dir(&dir) else { continue };
        for e in list {
            let p = plat::fs::join(&dir, &e.name);
            match e.kind {
                plat::fs::FileKind::Dir => todo.push(p),
                plat::fs::FileKind::File => total += plat::fs::lstat(&p).map_or(0, |m| m.size),
                _ => {}
            }
        }
        if total > stop_above {
            break;
        }
    }
    total
}

/// A command line after the program (and its command) were taken off.
///
/// ```text
/// --name VALUE / --name=VALUE   an option (its name is in `valued`)
/// --flag                        a flag
/// -x                            a short form (`short` maps it to its long name)
/// --                            everything after it is `rest`
/// ```
/// With `command` the first plain word ends the options too: it and everything after it is
/// `rest`, the way `bb run -w laptop cargo build --release` is meant.
#[derive(Default, Debug)]
pub struct Cli {
    pub pos: Vec<String>,
    pub opts: Vec<(String, String)>,
    pub flags: Vec<String>,
    pub rest: Vec<String>,
}

impl Cli {
    pub fn parse(args: &[String], valued: &[&str], short: &[(&str, &str)], command: bool) -> Result<Cli, String> {
        let mut c = Cli::default();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            i += 1;
            if a == "--" {
                c.rest.extend(args[i..].iter().cloned());
                break;
            }
            if !a.starts_with('-') || a == "-" {
                if command {
                    c.rest.extend(args[i - 1..].iter().cloned());
                    break;
                }
                c.pos.push(String::from(a));
                continue;
            }
            let (name, inline) = match a.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (a, None),
            };
            let name = short.iter().find(|(s, _)| *s == name).map_or(name, |(_, l)| l);
            if valued.contains(&name) {
                let v = match inline {
                    Some(v) => String::from(v),
                    None => {
                        let v = args.get(i).ok_or_else(|| format!("{} needs a value", name))?.clone();
                        i += 1;
                        v
                    }
                };
                c.opts.push((String::from(name), v));
            } else if inline.is_some() {
                return Err(format!("{} takes no value", name));
            } else {
                c.flags.push(String::from(name));
            }
        }
        Ok(c)
    }

    /// the last value of an option
    pub fn value(&self, name: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// every value of a repeated option, in order
    pub fn values(&self, name: &str) -> Vec<String> {
        self.opts.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// Err for a flag or option the command does not know (a typo must not pass silently)
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        for n in self.flags.iter().chain(self.opts.iter().map(|(n, _)| n)) {
            if !known.contains(&n.as_str()) {
                return Err(format!("unknown option {}", n));
            }
        }
        Ok(())
    }
}

/// Windows, run as administrator: a file or folder that only its owner may open is owned by
/// "the administrators" then -- and the same person in a window without administrator rights
/// can no longer read their own settings.  So what such a run writes is given to that person,
/// too (`icacls PATH /grant DOMAIN\\USER:F`).  Nothing happens elsewhere, without
/// administrator rights, or for a service's account (it writes for itself).
pub fn share_with_user(path: &str) {
    if !cfg!(windows) || !plat::process::elevated() {
        return;
    }
    let Some(user) = plat::env("USERNAME").filter(|u| !u.is_empty() && !u.ends_with('$') && !u.eq_ignore_ascii_case("SYSTEM")) else { return };
    let who = match plat::env("USERDOMAIN").filter(|d| !d.is_empty()) {
        Some(d) => format!("{}\\{}", d, user),
        None => user,
    };
    let grant = if plat::file_exists(path) == 2 { format!("{}:(OI)(CI)F", who) } else { format!("{}:F", who) };
    let args = [path, "/grant", grant.as_str(), "/Q"];
    let cmd = plat::process::Command { program: "icacls", args: &args, cwd: None, env: &[] };
    let Ok(mut child) = cmd.spawn() else { return };
    child.close_stdin();
    let end = tick_ms() + 10_000;
    while tick_ms() < end {
        if child.try_wait().map_or(true, |c| c.is_some()) {
            return;
        }
        plat::sleep_ms(20);
    }
}

/// Whether a file that should be there cannot be read: it is there (or whether it is cannot be
/// told) and does not open.  -> what to say about it, with the way out
pub fn unreadable(path: &str) -> Option<String> {
    let there = plat::file_exists_checked(path);
    if there == Some(0) || (there.is_some() && plat::read_file(path).is_some()) {
        return None;
    }
    let dir = plat::fs::parent(path);
    Some(if cfg!(windows) {
        format!(
            "{} cannot be read.  On Windows that happens after a Build Balancer program ran as administrator: in a terminal started with \"Run as administrator\",  icacls \"{}\" /reset /T /C  gives the files back (then run the programs without administrator rights)",
            path, dir
        )
    } else {
        format!("{} cannot be read by this user (was it written with sudo?  sudo chown -R $USER \"{}\" gives it back)", path, dir)
    })
}
