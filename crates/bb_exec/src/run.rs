// run -- a job's process: found, started on its processors, read, measured, ended
//
//   resolve(program, cwd)       the file a command's first word stands for: a path inside the
//                               workspace, a program of PATH, on Windows also a .cmd / .bat there
//   script(dir, shell, text)    a script for a shell as a file, and the command that runs it
//   spawn(&Spec) -> Proc        start it: no shell, no console window, pipes for its output, its
//                               processors limited to Spec::affinity
//   proc.pump(on)               hand over what it wrote since the last call (never blocks)
//   proc.exited()               its exit code once it ended
//   proc.memory()               resident memory of it and everything it started, in bytes
//   proc.kill()                 end it with everything it started
//
// The limit on processors is real: the process and its children can run on those and no
// others.  Memory is measured here and judged by the worker (bb-worker's watchdog).

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use plat::process::{Child, Command};
use plat::sync::Mutex;

use crate::sys;

/// what to start
#[derive(Clone, Debug, Default)]
pub struct Spec {
    /// the program's file (resolve)
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    /// variables to set (Some) or to remove (None) on top of this process's environment
    pub env: Vec<(String, Option<String>)>,
    /// the processors it may use, bit n = processor n (0: all this process may use)
    pub affinity: u64,
    /// run it (and what it starts) below the normal priority, so the machine stays usable
    pub low_priority: bool,
}

/// starting a child changes this process's processors for a moment, and the child is told
/// apart from the others by being new: one start at a time
static SPAWN: Mutex<()> = Mutex::new(());

pub struct Proc {
    child: Child,
    /// the process id (0 when it could not be told apart)
    pub pid: u32,
    /// the pids of the process and what it started, as far as they were seen alive
    tree: BTreeSet<u32>,
    out_end: bool,
    err_end: bool,
    exit: Option<i32>,
}

fn exists_file(p: &str) -> bool {
    plat::file_exists(p) == 1
}

/// The file a command's first word stands for; None when there is none.
pub fn resolve(program: &str, cwd: &str) -> Option<String> {
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    if program.contains('/') || program.contains('\\') {
        let base = if bb_core::util::absolute(program) { String::from(program) } else { plat::fs::join(cwd, &program.replace('/', if cfg!(windows) { "\\" } else { "/" })) };
        return exts.iter().map(|e| format!("{}{}", base, e)).find(|p| exists_file(p));
    }
    if let Some(p) = plat::process::find(program) {
        return Some(p);
    }
    if cfg!(windows) {
        // npm, yarn, gradle ... are .cmd files there
        for dir in plat::env("PATH").unwrap_or_default().split(';').filter(|d| !d.is_empty()) {
            for e in [".cmd", ".bat"] {
                let p = plat::fs::join(dir.trim_matches('"'), &format!("{}{}", program, e));
                if exists_file(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// the shell a script runs in when the job names none
pub fn default_shell() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

/// Write a script for `shell` into `dir` -> the program and arguments that run it.
pub fn script(dir: &str, shell: &str, text: &str) -> Result<(String, Vec<String>), String> {
    let shell = if shell.is_empty() || shell == "default" { default_shell() } else { shell };
    let (name, body) = match shell {
        "cmd" => ("job.cmd", format!("@echo off\r\n{}\r\n", text.replace("\r\n", "\n").replace('\n', "\r\n"))),
        "powershell" | "pwsh" => ("job.ps1", format!("$ErrorActionPreference = 'Stop'\r\n{}\r\nexit $LASTEXITCODE\r\n", text)),
        "sh" | "bash" => ("job.sh", format!("{}\n", text.replace("\r\n", "\n"))),
        _ => return Err(format!("unknown shell \"{}\": sh, bash, cmd, powershell or pwsh", shell)),
    };
    let path = plat::fs::join(dir, name);
    if !bb_core::util::put_file(&path, body.as_bytes()) {
        return Err(format!("{} cannot be written", path));
    }
    let s = |x: &str| String::from(x);
    Ok(match shell {
        "cmd" => {
            if !cfg!(windows) {
                return Err(String::from("the shell cmd exists on Windows only"));
            }
            (path, Vec::new())
        }
        "powershell" | "pwsh" => (s(shell), alloc::vec![s("-NoProfile"), s("-NonInteractive"), s("-ExecutionPolicy"), s("Bypass"), s("-File"), path]),
        // -e: a failing line ends the script, as a build script should
        _ => (s(shell), alloc::vec![s("-e"), path]),
    })
}

fn spawn_error(code: u32, program: &str) -> String {
    match code {
        2 | 3 => format!("{}: no such program", program),
        5 | 13 => format!("{}: not allowed to run it (not executable?)", program),
        87 if cfg!(windows) => format!("{}: a .cmd / .bat file cannot take arguments with \" % ! ^ & | < >; put the command into a script (--sh)", program),
        8 if !cfg!(windows) => format!("{}: not a program this system runs (a script without #! line, or built for another system)", program),
        _ => format!("{}: could not be started (system error {})", program, code),
    }
}

pub fn spawn(spec: &Spec) -> Result<Proc, String> {
    let mut args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
    let env: Vec<(&str, Option<&str>)> = spec.env.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
    let mut program = spec.program.as_str();
    // macOS: a process cannot lower the priority of what it starts and keep its own, so the
    // job starts through nice (sys.rs)
    if cfg!(target_os = "macos") && spec.low_priority {
        let mut niced = alloc::vec!["-n", "10", program];
        niced.append(&mut args);
        args = niced;
        program = "/usr/bin/nice";
    }
    let cmd = Command { program, args: &args, cwd: Some(&spec.cwd), env: &env };
    let guard = SPAWN.lock();
    let before = sys::children();
    let old = sys::affinity();
    let pinned = spec.affinity != 0 && old != 0 && sys::set_affinity(spec.affinity & old);
    let priority = if spec.low_priority { sys::lower_priority() } else { 0 };
    let r = cmd.spawn();
    sys::restore_priority(priority);
    if pinned {
        sys::set_affinity(old);
    }
    let mut child = r.map_err(|code| spawn_error(code, &spec.program))?;
    let pid = sys::children().into_iter().find(|p| !before.contains(p)).unwrap_or(0);
    drop(guard);
    // a build asks nothing: whatever reads its input sees the end at once
    child.close_stdin();
    let mut tree = BTreeSet::new();
    if pid != 0 {
        tree.insert(pid);
    }
    Ok(Proc { child, pid, tree, out_end: false, err_end: false, exit: None })
}

impl Proc {
    /// Hand what the process wrote since the last call to `on(is_stderr, bytes)` -> whether
    /// there was anything.
    pub fn pump(&mut self, on: &mut dyn FnMut(bool, &[u8])) -> bool {
        let mut buf = [0u8; 16 * 1024];
        let mut any = false;
        for err in [false, true] {
            // a bounded number of rounds, so one chatty stream does not starve the other
            for _ in 0..8 {
                if if err { self.err_end } else { self.out_end } {
                    break;
                }
                let r = if err { self.child.read_stderr(&mut buf) } else { self.child.read_stdout(&mut buf) };
                match r {
                    Ok(Some(0)) | Err(_) => {
                        if err {
                            self.err_end = true
                        } else {
                            self.out_end = true
                        }
                    }
                    Ok(Some(n)) => {
                        on(err, &buf[..n]);
                        any = true;
                    }
                    Ok(None) => break,
                }
            }
        }
        any
    }

    /// both output streams are at their end
    pub fn drained(&self) -> bool {
        self.out_end && self.err_end
    }

    /// the exit code once the process ended
    pub fn exited(&mut self) -> Option<i32> {
        if self.exit.is_none() {
            self.exit = match self.child.try_wait() {
                Ok(code) => code,
                Err(_) => Some(-1),
            };
        }
        self.exit
    }

    /// Resident memory of the process and everything below it, in bytes (0: its id is unknown).
    pub fn memory(&mut self) -> u64 {
        if self.pid == 0 {
            return 0;
        }
        let list = sys::processes();
        let alive: BTreeSet<u32> = list.iter().map(|p| p.pid).collect();
        self.tree.retain(|p| alive.contains(p));
        loop {
            let before = self.tree.len();
            for p in &list {
                if self.tree.contains(&p.parent) {
                    self.tree.insert(p.pid);
                }
            }
            if self.tree.len() == before {
                break;
            }
        }
        list.iter().filter(|p| self.tree.contains(&p.pid)).map(|p| if p.rss != 0 { p.rss } else { sys::rss(p.pid) }).sum()
    }

    /// end the process and everything it started
    pub fn kill(&mut self) {
        self.child.terminate();
    }
}
