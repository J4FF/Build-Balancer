// tools -- which build tools a machine has, so whoever hands out jobs can tell where a job can
// run: every name of a list is looked up on PATH and asked for its version
//
//   detect(extra)   the tools of DEFAULT plus the names in `extra`, each with the first line it
//                   prints for --version (some take another word, see DEFAULT)
//
// A name that is not found is left out.  A program that is found but does not answer within a
// few seconds, or answers with an error, is left out too: Windows keeps a `python.exe` that
// only points to its store, and that is no Python.

use alloc::string::String;
use alloc::vec::Vec;

use bb_core::proto::Tool;

use crate::run::{resolve, spawn, Spec};

/// (name, the argument that makes it print its version)
pub const DEFAULT: &[(&str, &str)] = &[
    ("cargo", "--version"),
    ("rustc", "--version"),
    ("gcc", "--version"),
    ("g++", "--version"),
    ("clang", "--version"),
    ("cl", ""),
    ("msbuild", "-version"),
    ("cmake", "--version"),
    ("make", "--version"),
    ("ninja", "--version"),
    ("meson", "--version"),
    ("python", "--version"),
    ("python3", "--version"),
    ("node", "--version"),
    ("npm", "--version"),
    ("pnpm", "--version"),
    ("yarn", "--version"),
    ("java", "-version"),
    ("javac", "-version"),
    ("gradle", "--version"),
    ("mvn", "--version"),
    ("dotnet", "--version"),
    ("go", "version"),
    ("zig", "version"),
    ("swift", "--version"),
    ("git", "--version"),
    ("docker", "--version"),
    ("bash", "--version"),
];

/// run a program for at most `ms` and return (exit code, what it printed, up to 4 KiB)
pub fn run_brief(program: &str, args: &[&str], ms: u64) -> Option<(i32, String)> {
    let cwd = plat::temp_dir().unwrap_or_else(plat::cwd);
    let spec = Spec { program: String::from(program), args: args.iter().map(|a| String::from(*a)).collect(), cwd, ..Spec::default() };
    let mut p = spawn(&spec).ok()?;
    let mut text: Vec<u8> = Vec::new();
    let end = bb_core::util::tick_ms() + ms;
    loop {
        let any = p.pump(&mut |_, b| {
            if text.len() < 4096 {
                text.extend_from_slice(b);
            }
        });
        if let Some(code) = p.exited() {
            // what it wrote just before it ended
            for _ in 0..20 {
                if p.drained() || !p.pump(&mut |_, b| text.extend_from_slice(&b[..b.len().min(4096usize.saturating_sub(text.len()))])) {
                    break;
                }
            }
            return Some((code, String::from_utf8_lossy(&text).into_owned()));
        }
        if bb_core::util::tick_ms() > end {
            p.kill();
            return None;
        }
        if !any {
            plat::sleep_ms(15);
        }
    }
}

fn version_of(path: &str, arg: &str) -> Option<String> {
    let args: Vec<&str> = if arg.is_empty() { Vec::new() } else { alloc::vec![arg] };
    let (code, text) = run_brief(path, &args, 12_000)?;
    // cl without arguments prints its banner and usage and ends with 0; everything else must succeed
    if code != 0 {
        return None;
    }
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut s: String = line.chars().filter(|c| !c.is_control()).take(120).collect();
    if s.is_empty() {
        s.push('?');
    }
    Some(s)
}

/// the tools of this machine
pub fn detect(extra: &[String]) -> Vec<Tool> {
    detect_with(extra, &mut |_| {})
}

/// detect(), handing each tool to `found` as soon as it answered (asking them all takes a while)
pub fn detect_with(extra: &[String], found: &mut dyn FnMut(&Tool)) -> Vec<Tool> {
    let mut out: Vec<Tool> = Vec::new();
    let cwd = plat::cwd();
    let names = DEFAULT.iter().map(|(n, a)| (String::from(*n), *a)).chain(extra.iter().map(|n| (n.clone(), "--version")));
    for (name, arg) in names {
        if name.is_empty() || name.contains(['/', '\\']) || out.iter().any(|t| t.name == name) {
            continue;
        }
        let Some(path) = resolve(&name, &cwd) else { continue };
        // Windows' own bash.exe starts a Linux subsystem: not a shell for jobs in a Windows folder
        if cfg!(windows) && name == "bash" && path.to_ascii_lowercase().contains("system32") {
            continue;
        }
        if let Some(version) = version_of(&path, arg) {
            let t = Tool { name, version, path };
            found(&t);
            out.push(t);
        }
    }
    out
}
