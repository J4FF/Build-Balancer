// bb_update -- a newer version of the programs, from the project's releases
//
//   check()                      the newest release and whether it is newer than this program
//   install(version, dir, say)   fetch that release's archive for this system, check it against
//                                the release's SHA256SUMS, and put its programs in the place of
//                                the ones in `dir` -- also of the one that is running
//   own_dir()                    the folder the running program lies in
//   restart(args)                start the program file that is there now, with these arguments
//   tidy(dir)                    remove what an update left behind (the files it replaced)
//   at_start()                   for every program's start: tidy its folder, and remember the
//                                file it runs from
//   replaced()                   whether an update has put another file in that one's place
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
//
// The four programs of a folder belong together.  One update at a time works in a folder (a
// lock file says so, and the tidying of a program that starts meanwhile leaves its files
// alone).  An update writes down what it is about to do before it does it, keeps the old
// programs within reach until all four new ones are in place, and puts them back when one
// cannot be.  One that was cut short -- its note is still there -- is taken back whole by
// the next program that starts (tidy).
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use netio::{Client, Headers, Url};
use plat::sync::Mutex;

/// the first version that can update itself: older programs do not know the order to
pub const FIRST: &str = "0.1.2";

/// whether a program of this version can update itself (when asked to, or on a click)
pub fn can_update_itself(version: &str) -> bool {
    !newer(FIRST, version) && numbers(version).is_some()
}

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

/// Whether an address may be asked.  What is fetched becomes the programs of this machine, so
/// it travels encrypted and from a server that proved who it is: https, also at every address
/// a server sends on to.  Plain http only at a place someone named on purpose with
/// BB_RELEASE_URL (a mirror in the own network, the tests).
fn allowed(url: &str) -> Result<(), String> {
    let chosen = other_place().is_some_and(|b| b.starts_with("http://") && url.starts_with(b.as_str()));
    if url.starts_with("https://") || chosen {
        Ok(())
    } else {
        Err(format!("{} is not an https address: nothing is fetched from there", url.split('?').next().unwrap_or(url)))
    }
}

/// one GET at an address that is allowed -> (status, where it sends on, body)
fn get_step(url: &str, max: usize) -> Result<(u16, Option<String>, Vec<u8>), String> {
    allowed(url)?;
    get_once(url, max)
}

/// a GET that follows where it is sent on (a release's files lie on another server than its page)
fn get(url: &str, max: usize) -> Result<(u16, Vec<u8>), String> {
    let mut url = String::from(url);
    for _ in 0..6 {
        let (status, on, body) = get_step(&url, max)?;
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
        let (status, on, _) = get_step(&format!("{}/releases/latest", bb_core::REPOSITORY), 8 << 20)?;
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

/// The names the program before an update is kept under until the update is through:
/// "bb.exe.old", and "bb.exe.old.2" ... when a program still runs from the one before
/// (Windows, where the running file itself is what is moved aside).
fn asides(dir: &str, file: &str) -> Vec<String> {
    let first = format!("{}.old", file);
    let more = format!("{}.old.", file);
    plat::fs::read_dir(dir).unwrap_or_default().into_iter().filter(|e| e.name == first || e.name.starts_with(&more)).map(|e| plat::fs::join(dir, &e.name)).collect()
}

/// a name to keep the program before an update under that is free
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

/// The file whose lock says: an update is at work in this folder.  It stays where it is,
/// also between updates: a lock is on the file, not on its name, and a name that went away
/// and came back would give two updates a lock each.
fn lock_path(dir: &str) -> String {
    plat::fs::join(dir, ".buildbalancer-update.lock")
}

/// Take the folder's lock -> None: another update holds it, or the file is not this user's
/// to open.  The file is handed to the folder's owner, writable for those the folder is
/// writable for: who may replace the programs may take the lock, also after root was here
/// once (`sudo bb-relay update` with the programs in a folder of the user's).
fn lock(dir: &str) -> Option<plat::FileLock> {
    let path = lock_path(dir);
    let plain = |m: &plat::fs::Meta| m.kind == Some(plat::fs::FileKind::File);
    // (anything but a plain file under this name is not ours, and is not followed anywhere)
    if plat::fs::lstat(&path).is_some_and(|m| !plain(&m)) {
        return None;
    }
    let lock = plat::FileLock::acquire(&path)?;
    if cfg!(windows) {
        // (no dot hides a file there)
        plat::fs::set_attrs(&path, 0x2);
    } else if let (Some(folder), Some(file)) = (plat::fs::stat(dir), plat::fs::lstat(&path))
        && plain(&file)
    {
        if (file.uid, file.gid) != (folder.uid, folder.gid) {
            plat::fs::set_owner(&path, folder.uid, folder.gid);
        }
        let mode = 0o600 | (folder.mode & 0o066);
        if file.mode & 0o7777 != mode {
            plat::fs::set_mode(&path, mode);
        }
    }
    Some(lock)
}

/// The file that says what an update is about to do, written before it does it and removed
/// when all of it is done: one line per program, "FILE<tab>THE NAME ITS OLD ONE IS KEPT UNDER"
/// (nothing after the tab: there was no such program).  As long as it is there, the update
/// did not get through, and everything it names can be taken back.
fn journal_path(dir: &str) -> String {
    plat::fs::join(dir, ".buildbalancer-update.journal")
}

/// (program file, the name its old one is kept under or "") for each line of a journal that
/// names one of the programs and a name of the kind an update gives -> None: it cannot be read
fn journal(dir: &str) -> Option<Vec<(String, String)>> {
    let text = plat::read_file(&journal_path(dir))?;
    let text = String::from_utf8_lossy(&text).into_owned();
    let kept_name = |file: &str, old: &str| {
        let Some(rest) = old.strip_prefix(file).and_then(|r| r.strip_prefix(".old")) else { return false };
        rest.is_empty() || rest.strip_prefix('.').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    Some(
        text.lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(file, old)| PROGRAMS.iter().any(|p| file_name(p) == *file) && (old.is_empty() || kept_name(file, old)))
            .map(|(file, old)| (String::from(file), String::from(old)))
            .collect(),
    )
}

/// Take back an update that did not get through: every program it names as it was before
/// -> the programs that could not be put back (none: all is as it was).  What it kept of
/// those stays where it is -- it is all there is to go back to.
fn roll_back(dir: &str, plan: &[(String, String)]) -> Vec<String> {
    let mut left = Vec::new();
    for (file, old) in plan {
        let target = plat::fs::join(dir, file);
        if old.is_empty() {
            // there was no such program before: none stays
            if plat::file_exists(&target) != 0 && !plat::delete(&target) {
                left.push(file.clone());
            }
            continue;
        }
        let kept = plat::fs::join(dir, old);
        // (no such file: the update had not come to this program yet)
        if plat::file_exists(&kept) == 0 {
            continue;
        }
        let mut back = plat::rename(&kept, &target);
        if !back && cfg!(windows) {
            // Windows writes over no program that runs -- but that one can be moved out of
            // the way (and goes at a later start)
            plat::rename(&target, &aside(dir, file));
            back = plat::rename(&kept, &target);
        }
        if !back {
            left.push(file.clone());
            continue;
        }
        // Elsewhere the two can be names of one file (the update had not put the new one
        // there yet), and renaming the one onto the other leaves both.
        if !cfg!(windows) {
            plat::delete(&kept);
        }
    }
    left
}

/// what an update left in a folder: (program file, the programs kept from before, a new one not yet in place)
fn leftovers(dir: &str) -> Vec<(String, Vec<String>, Option<String>)> {
    PROGRAMS
        .iter()
        .map(|p| {
            let file = file_name(p);
            let new = Some(fresh(dir, &file)).filter(|n| plat::file_exists(n) != 0);
            (file.clone(), asides(dir, &file), new)
        })
        .filter(|(_, old, new)| !old.is_empty() || new.is_some())
        .collect()
}

/// Clear away what an update left behind in a folder.  One that got through leaves the
/// programs it replaced (Windows: a program that still runs from one keeps it until it ends).
/// One that was cut short -- its journal is still there -- is taken back whole: the programs
/// of a folder belong together, and two new ones beside two old ones are worse than four
/// old ones.  While an update is at work in the folder, nothing is touched.
pub fn tidy(dir: &str) {
    let cut_short = plat::file_exists(&journal_path(dir)) != 0;
    if !cut_short && leftovers(dir).is_empty() {
        return;
    }
    // (a folder this user may not write to: there is nothing to do here either way)
    let Some(_lock) = lock(dir) else { return };
    if let Err(why) = tidy_locked(dir) {
        plat::log_warn("update", &format!("an update of the programs in {} was cut short and is not taken back yet ({}): the next start of a program tries again", dir, why));
    }
}

/// tidy() under the folder's lock, where no update can be in the middle of anything -> Err(why
/// an update that was cut short is not taken back yet).  Then nothing was cleared away: its
/// note and the old programs it kept stay until all of it could be taken back.
fn tidy_locked(dir: &str) -> Result<(), String> {
    if plat::file_exists(&journal_path(dir)) != 0 {
        // one that cannot be read is another user's, and theirs to take back
        let Some(plan) = journal(dir) else { return Err(String::from("what it was about to do was written down by another user")) };
        let left = roll_back(dir, &plan);
        if !left.is_empty() {
            return Err(format!("{} cannot be replaced right now", left.join(", ")));
        }
    }
    for (file, mut old, new) in leftovers(dir) {
        let target = plat::fs::join(dir, &file);
        // a program that is in no place at all, and one kept from before: back with it
        if plat::file_exists(&target) == 0
            && let Some(last) = old.pop()
        {
            plat::rename(&last, &target);
        }
        for path in old.into_iter().chain(new) {
            plat::delete(&path);
        }
    }
    plat::delete(&journal_path(dir));
    Ok(())
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

/// Write the new programs beside the old ones, then let them take their places.  The old
/// ones stay within reach until all are in place: if one cannot be, all go back.
fn put(dir: &str, programs: &[(String, &[u8])]) -> Result<(), String> {
    let denied = |what: &str| {
        format!(
            "{} in {} cannot be replaced by this user: run the update {}",
            what,
            dir,
            if cfg!(windows) { "in a terminal started with \"Run as administrator\"" } else { "with sudo" }
        )
    };
    let Some(_lock) = lock(dir) else {
        // No lock: another update works here right now -- or the folder, or the lock's file
        // in it, is not this user's to write to, which says nothing about another update.
        let probe = plat::fs::join(dir, ".buildbalancer-update.probe");
        let writable = plat::write_file(&probe, b"");
        plat::delete(&probe);
        // (a lock that is held leaves its file to be opened; Windows holds one by having the
        // file open alone, and there the two cannot be told apart)
        let foreign = !cfg!(windows) && plat::File::open(&lock_path(dir), plat::FO_RW).is_none();
        return Err(if writable && !foreign { format!("another update is at work in {}: try again in a moment", dir) } else { denied("the programs") });
    };
    // What an update before this one left goes first -- and one that was cut short is taken
    // back first: a new one on top of it would bury the old programs it kept.
    if let Err(why) = tidy_locked(dir) {
        return Err(format!("an update that was cut short in {} is not taken back yet ({}): close the Build Balancer programs that run from there, then try again", dir, why));
    }
    let result = replace(dir, programs).map_err(|file| {
        // (its note is still there: not everything could be put back)
        let unsettled = if plat::file_exists(&journal_path(dir)) != 0 { " -- and not every program of before could be put back yet: the next start of a program tries again" } else { "" };
        format!("{}{}", denied(&file), unsettled)
    });
    // what is left over now is this update's own: new files that did not get into place
    for (file, _) in programs {
        plat::delete(&fresh(dir, file));
    }
    result
}

/// the replacing itself, under the folder's lock -> Err(the program that could not be replaced)
fn replace(dir: &str, programs: &[(String, &[u8])]) -> Result<(), String> {
    for (file, content) in programs {
        let new = fresh(dir, file);
        if !plat::write_file(&new, content) || (!cfg!(windows) && !plat::fs::set_mode(&new, 0o755)) {
            return Err(file.clone());
        }
    }
    // What is about to happen, on disk before it happens: (the program, the name its old one
    // is kept under).  An update that ends here or anywhere below -- the power, a kill -- is
    // taken back whole by the next program that starts (tidy).
    let plan: Vec<(String, String)> = programs
        .iter()
        .map(|(file, _)| (file.clone(), if plat::file_exists(&plat::fs::join(dir, file)) != 0 { String::from(base_name(&aside(dir, file))) } else { String::new() }))
        .collect();
    let text: String = plan.iter().map(|(file, old)| format!("{}\t{}\n", file, old)).collect();
    if !plat::write_file(&journal_path(dir), text.as_bytes()) {
        return Err(programs[0].0.clone());
    }
    for (file, old) in &plan {
        let (target, new) = (plat::fs::join(dir, file), fresh(dir, file));
        let kept_as = plat::fs::join(dir, old);
        // Windows: a program that runs cannot be written over, but it can be moved aside.
        // Elsewhere the old program gets a second name and the new one then takes the first:
        // a program that runs must keep its name, or it could not find the new one to start.
        let kept = if old.is_empty() {
            true
        } else if cfg!(windows) {
            plat::rename(&target, &kept_as)
        } else {
            // (no second name for a file of another user's on most systems: a copy then)
            plat::fs::hard_link(&target, &kept_as) || plat::read_file(&target).is_some_and(|b| plat::write_file(&kept_as, &b) && plat::fs::set_mode(&kept_as, 0o755))
        };
        if !kept || !plat::rename(&new, &target) {
            // (what cannot be put back now stays on record -- the note, the old programs --
            // and the next program that starts tries again)
            if roll_back(dir, &plan).is_empty() {
                plat::delete(&journal_path(dir));
            }
            return Err(file.clone());
        }
    }
    // All four are in place: from here on the update counts.
    plat::delete(&journal_path(dir));
    // Where the old programs are only second names (not the files that run), they can go
    // now; on Windows they go at a later start.
    if !cfg!(windows) {
        for (_, old) in plan.iter().filter(|(_, old)| !old.is_empty()) {
            plat::delete(&plat::fs::join(dir, old));
        }
    }
    Ok(())
}

/// the file this program runs from, as at_start() found it: (path, device, file, written)
static SELF: Mutex<Option<(String, u64, u64, i64)>> = Mutex::new(None);
/// For a program's start: remember the file it runs from, and clear away what an update left
/// in its folder.
pub fn at_start() {
    let Some(exe) = plat::current_exe() else { return };
    // Linux names a program that was replaced while it runs "PATH (deleted)"
    let exe = String::from(exe.strip_suffix(" (deleted)").unwrap_or(&exe));
    if let Some(m) = plat::fs::stat(&exe) {
        *SELF.lock() = Some((exe.clone(), m.dev, m.ino, m.mtime_ns));
    }
    let dir = plat::fs::parent(&exe);
    if !dir.is_empty() {
        tidy(dir);
    }
}

/// Whether an update that got through has put another file in this program's place since
/// at_start(): what runs is then not what a new start would run.  Whoever made that update --
/// `bb update`, the app, an installer, another user -- need not know of this program.
pub fn replaced() -> bool {
    let Some((path, dev, ino, written)) = SELF.lock().clone() else { return false };
    // (an update that is still at work, or one that is about to be taken back: not yet)
    if plat::file_exists(&journal_path(plat::fs::parent(&path))) != 0 {
        return false;
    }
    // another file, written at another time (where a file system has no lasting numbers for
    // its files, the time alone says that nothing happened)
    plat::fs::stat(&path).is_some_and(|m| (m.dev, m.ino) != (dev, ino) && m.mtime_ns != written)
}

/// start the program file that is at this program's path now -- after an update the new one --
/// with these arguments -> whether it started
pub fn restart(args: &[String]) -> bool {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    plat::restart_self(&refs) == 0
}
