// bb_update -- a newer version of the programs, from the project's releases
//
//   check()                      the newest release and whether it is newer than this program
//   install(version, dir, say)   fetch that release's archive for this system, check it against
//                                the release's SHA256SUMS, and put its programs in the place of
//                                the ones in `dir` -- also of the one that is running
//   own_dir()                    the folder the running program lies in
//   restart(args)                start the program file that is there now, with these arguments
//   tidy(dir)                    remove what an update left behind (the files it replaced)
//
// Where it asks: REPOSITORY/releases/latest/download/VERSION names the newest version (a
// release from before that file existed is found through the address of "the latest release"),
// REPOSITORY/releases/download/vVERSION/ has SHA256SUMS and one archive per system.  The
// connection is https, checked against the system's certificate authorities.  BB_RELEASE_URL
// names another place with VERSION, SHA256SUMS and the archives side by side (a mirror, a test).
//
// A program that runs can be replaced: Linux and macOS keep the old file open under no name,
// Windows lets a running program be renamed, so the old one is moved aside ("bb.exe.old") and
// removed at a later start.  What runs keeps running the old version until it is started again.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use netio::{Client, Headers, Url};

/// the programs of a release
pub const PROGRAMS: [&str; 4] = ["bb", "bb-worker", "bb-relay", "BuildBalancer"];

const EXT: &str = if cfg!(windows) { ".exe" } else { "" };

/// the file name of a program on this system
pub fn file_name(program: &str) -> String {
    format!("{}{}", program, EXT)
}

/// the archive of this system in a release
pub fn archive_name() -> String {
    format!("buildbalancer-{}-{}.{}", bb_core::os_name(), bb_core::arch_name(), if cfg!(windows) { "zip" } else { "tar.gz" })
}

/// another place to fetch from than the project's releases
fn other_place() -> Option<String> {
    plat::env("BB_RELEASE_URL").map(|b| String::from(b.trim().trim_end_matches('/'))).filter(|b| !b.is_empty())
}

/// where a version's files are (None: the newest release)
fn place(version: Option<&str>) -> String {
    if let Some(b) = other_place() {
        return b;
    }
    match version {
        Some(v) => format!("{}/releases/download/v{}", bb_core::REPOSITORY, v),
        None => format!("{}/releases/latest/download", bb_core::REPOSITORY),
    }
}

fn client(max: usize) -> Client {
    let mut c = Client::new().with_connector(netio::trust::system_connector(Vec::new()));
    c.connect_ms = 10_000;
    c.timeout_ms = 60_000;
    c.max_body = max;
    c.user_agent = format!("build-balancer/{}", bb_core::VERSION);
    c
}

/// one GET, not following anything -> (status, where it sends on, body)
fn get_once(url: &str, max: usize) -> Result<(u16, Option<String>, Vec<u8>), String> {
    let u = Url::parse(url).ok_or_else(|| format!("{} is no address", url))?;
    let r = client(max).get(&u, &Headers::new()).map_err(|e| format!("{} cannot be reached: {}", u.host, e))?;
    let on = r.headers.get("Location").map(|l| if l.starts_with('/') { format!("{}{}", u.origin(), l) } else { String::from(l) });
    Ok((r.status, on, r.body_bytes().to_vec()))
}

/// a GET that follows where it is sent on (a release's files lie on another server than its page)
fn get(url: &str, max: usize) -> Result<(u16, Vec<u8>), String> {
    let mut url = String::from(url);
    for _ in 0..6 {
        let (status, on, body) = get_once(&url, max)?;
        match on {
            Some(next) if matches!(status, 301 | 302 | 303 | 307 | 308) => url = next,
            _ => return Ok((status, body)),
        }
    }
    Err(format!("{} sends on and on", url))
}

/// "0.1.2" (also "v0.1.2", "0.1.2-rc1") as numbers
fn numbers(v: &str) -> Option<[u32; 3]> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let out = [it.next()?.parse().ok()?, it.next().unwrap_or("0").parse().ok()?, it.next().unwrap_or("0").parse().ok()?];
    it.next().is_none().then_some(out)
}

/// whether version `a` is newer than version `b`
pub fn newer(a: &str, b: &str) -> bool {
    matches!((numbers(a), numbers(b)), (Some(x), Some(y)) if x > y)
}

/// what a look at the releases found
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Check {
    /// the version of this program
    pub current: String,
    /// the newest released version
    pub latest: String,
    /// the newest release is newer than this program
    pub newer: bool,
}

/// the newest released version
pub fn latest() -> Result<String, String> {
    // (room for more than a version: where the file is missing, a whole page says so)
    let (status, body) = get(&format!("{}/VERSION", place(None)), 4 << 20)?;
    let named = String::from_utf8_lossy(&body).lines().next().map(|l| String::from(l.trim().trim_start_matches('v'))).unwrap_or_default();
    if status == 200 && numbers(&named).is_some() {
        return Ok(named);
    }
    if other_place().is_none() && status == 404 {
        // a release from before VERSION was part of it: "the latest release" leads to its tag
        let (status, on, _) = get_once(&format!("{}/releases/latest", bb_core::REPOSITORY), 8 << 20)?;
        if let Some(tag) = on.as_deref().and_then(|l| l.rsplit_once("/tag/")).map(|(_, t)| t.trim_start_matches('v'))
            && numbers(tag).is_some()
        {
            return Ok(String::from(tag));
        }
        if status == 404 {
            return Err(String::from("no release has been published yet"));
        }
    }
    Err(format!("the releases did not say which version is the newest (answer {})", status))
}

/// look for a newer version than this program
pub fn check() -> Result<Check, String> {
    let latest = latest()?;
    Ok(Check { current: String::from(bb_core::VERSION), newer: newer(&latest, bb_core::VERSION), latest })
}

/// the folder the running program lies in
pub fn own_dir() -> Option<String> {
    let exe = plat::current_exe()?;
    // Linux names a program that was replaced while it runs "PATH (deleted)"
    let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
    Some(String::from(plat::fs::parent(exe))).filter(|d| !d.is_empty())
}

/// the base name of a path inside an archive
fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// the files of a .tar: (name without folders, content)
fn untar(t: &[u8]) -> Vec<(String, Vec<u8>)> {
    let text = |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|c| *c == 0).unwrap_or(b.len())]).into_owned();
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 512 <= t.len() {
        let h = &t[p..p + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let size = usize::from_str_radix(text(&h[124..136]).trim(), 8).unwrap_or(0);
        p += 512;
        // '0' or NUL: a file (anything else -- folders, links, extended headers -- is passed over)
        if (h[156] == b'0' || h[156] == 0) && p + size <= t.len() {
            out.push((String::from(base_name(&text(&h[0..100]))), t[p..p + size].to_vec()));
        }
        p = p.saturating_add(size.div_ceil(512) * 512);
    }
    out
}

/// the files of a release's archive: (name without folders, content)
fn unpack(name: &str, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = || format!("{} cannot be unpacked", name);
    if name.ends_with(".zip") {
        let limits = archive::Limits { entries: 64, part_bytes: 96 << 20, total_bytes: 384 << 20, archive_bytes: 192 << 20 };
        let entries = archive::read(data, limits).map_err(|_| bad())?;
        return Ok(entries.into_iter().map(|e| (String::from(base_name(&e.name)), e.data)).collect());
    }
    let size = img_core::gzip::gzip_isize(data).map_err(|_| bad())? as usize;
    if size > 384 << 20 {
        return Err(bad());
    }
    let mut tar = alloc::vec![0u8; size];
    let (n, _) = img_core::gzip::gzip_decode(data, &mut tar).map_err(|_| bad())?;
    tar.truncate(n);
    Ok(untar(&tar))
}

/// The names a program was moved aside to while a newer one took its place (Windows):
/// "bb.exe.old", and "bb.exe.old.2" ... when a program still ran from the one before.
fn asides(dir: &str, file: &str) -> Vec<String> {
    let first = format!("{}.old", file);
    let more = format!("{}.old.", file);
    plat::fs::read_dir(dir).unwrap_or_default().into_iter().filter(|e| e.name == first || e.name.starts_with(&more)).map(|e| plat::fs::join(dir, &e.name)).collect()
}

/// a name to move a program aside to that is free
fn aside(dir: &str, file: &str) -> String {
    let taken = asides(dir, file);
    let mut name = plat::fs::join(dir, &format!("{}.old", file));
    let mut n = 2;
    while taken.contains(&name) {
        name = plat::fs::join(dir, &format!("{}.old.{}", file, n));
        n += 1;
    }
    name
}

fn fresh(dir: &str, file: &str) -> String {
    plat::fs::join(dir, &format!(".{}.new", file))
}

/// remove what an update left behind in a folder: the programs it replaced, files of one that
/// did not finish.  A program that still runs from a replaced file keeps it until it ends.
pub fn tidy(dir: &str) {
    for p in PROGRAMS {
        let file = file_name(p);
        let mut leftovers = asides(dir, &file);
        leftovers.push(fresh(dir, &file));
        for path in leftovers {
            if plat::file_exists(&path) != 0 {
                plat::delete(&path);
            }
        }
    }
}

/// Put the programs of release `version` into `dir` -> the programs that were put there.
/// `say` gets a word on what happens.  Nothing is changed unless the archive matches the
/// release's checksum and all of its programs could be written.
pub fn install(version: &str, dir: &str, say: &mut dyn FnMut(&str)) -> Result<Vec<String>, String> {
    if numbers(version).is_none() {
        return Err(format!("\"{}\" is no version", version));
    }
    let (name, from) = (archive_name(), place(Some(version)));
    say("fetching the checksums");
    let (status, sums) = get(&format!("{}/SHA256SUMS", from), 1 << 20)?;
    if status != 200 {
        return Err(format!("release {} has no SHA256SUMS (answer {})", version, status));
    }
    let sums = String::from_utf8_lossy(&sums).into_owned();
    let want = sums
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?, f.next()?.trim_start_matches('*')))
        })
        .find(|(_, n)| *n == name)
        .map(|(h, _)| h.to_ascii_lowercase())
        .ok_or_else(|| format!("release {} has no programs for this system ({})", version, name))?;
    say("fetching the programs");
    let (status, data) = get(&format!("{}/{}", from, name), 192 << 20)?;
    if status != 200 {
        return Err(format!("{} of release {} could not be fetched (answer {})", name, version, status));
    }
    if crypto::hex(&crypto::sha256(&data)) != want {
        return Err(format!("{} does not match the release's checksum: nothing was installed", name));
    }
    say("installing");
    let files = unpack(&name, &data)?;
    let mut programs: Vec<(String, &[u8])> = Vec::new();
    for p in PROGRAMS {
        let file = file_name(p);
        if let Some((_, content)) = files.iter().find(|(n, _)| *n == file) {
            programs.push((file, content.as_slice()));
        }
    }
    if programs.len() != PROGRAMS.len() {
        return Err(format!("{} does not hold all of the programs: nothing was installed", name));
    }
    put(dir, &programs)?;
    Ok(programs.into_iter().map(|(f, _)| f).collect())
}

/// write the new programs beside the old ones, then let them take their places
fn put(dir: &str, programs: &[(String, &[u8])]) -> Result<(), String> {
    let denied = |what: &str| {
        format!(
            "{} in {} cannot be replaced by this user: run the update {}",
            what,
            dir,
            if cfg!(windows) { "in a terminal started with \"Run as administrator\"" } else { "with sudo" }
        )
    };
    tidy(dir);
    for (file, content) in programs {
        let new = fresh(dir, file);
        if !plat::write_file(&new, content) || (!cfg!(windows) && !plat::fs::set_mode(&new, 0o755)) {
            tidy(dir);
            return Err(denied(file));
        }
    }
    // (the program, where the one before it went)
    let mut moved: Vec<(&String, Option<String>)> = Vec::new();
    let undo = |moved: &[(&String, Option<String>)]| {
        // back to what was there: the programs of one folder belong together
        for (file, old) in moved.iter().rev() {
            let target = plat::fs::join(dir, file);
            if let Some(old) = old {
                plat::delete(&target);
                plat::rename(old, &target);
            }
        }
    };
    for (file, _) in programs {
        let (target, new) = (plat::fs::join(dir, file), fresh(dir, file));
        // Windows: a program that runs cannot be written over, but it can be moved aside
        let old = (cfg!(windows) && plat::file_exists(&target) != 0).then(|| aside(dir, file));
        let ok = old.as_ref().is_none_or(|o| plat::rename(&target, o));
        if !ok || !plat::rename(&new, &target) {
            if ok {
                moved.push((file, old));
            }
            undo(&moved);
            tidy(dir);
            return Err(denied(file));
        }
        moved.push((file, old));
    }
    Ok(())
}

/// start the program file that is at this program's path now -- after an update the new one --
/// with these arguments -> whether it started
pub fn restart(args: &[String]) -> bool {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    plat::restart_self(&refs) == 0
}
