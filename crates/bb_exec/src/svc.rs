// svc -- the programs as system services, run as the right user
//
//   install(&Service) / uninstall(name) / start(name) / stop(name) / state_text(name)
//   sudo_user() / user_named(name) / user_of(uid)    who a service should run as
//   login_path(&User)                               the PATH that user's login shell has
//   user_data_dir(&User, "worker")                  that user's data folder of a program
//
// Windows and macOS: JFLib's plat::service as it is (the service runs as LocalSystem / root).
// Linux: JFLib's systemd unit plus what it has no field for -- the user the service runs as,
// that user's PATH and home.  A worker runs the main machine's commands: as root they would
// have every right on the machine and none of the tools of the person who installed rustup or
// nvm for themselves.  So a service runs as the user who installs it (the one who called sudo)
// on that user's data folder, and as root only when root itself asks for it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use plat::service::ServiceState;

/// a user of this machine
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub home: String,
}

/// what to install
pub struct Service<'a> {
    /// Windows: the service name; Linux: the unit name; macOS: the label
    pub name: &'a str,
    pub display: &'a str,
    pub description: &'a str,
    /// the program (absolute) and its arguments
    pub exe: &'a str,
    pub args: &'a [String],
    /// Linux: the user it runs as (None: root).  Elsewhere the system's service account.
    pub user: Option<&'a User>,
    /// Linux: the PATH it gets (None: systemd's default)
    pub path: Option<&'a str>,
}

/// a line of /etc/passwd as a user
fn passwd_user(line: &str) -> Option<User> {
    let f: Vec<&str> = line.trim().split(':').collect();
    if f.len() < 6 || f[0].is_empty() || f[0].starts_with('#') {
        return None;
    }
    Some(User { name: String::from(f[0]), uid: f[2].parse().ok()?, home: String::from(f[5]) })
}

fn passwd_find(pick: &dyn Fn(&User) -> bool) -> Option<User> {
    if !cfg!(unix) {
        return None;
    }
    let text = plat::read_file("/etc/passwd").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    text.lines().filter_map(passwd_user).find(|u| pick(u))
}

/// a user by name (the machine's own list; a directory service's users through getent)
pub fn user_named(name: &str) -> Option<User> {
    if name.is_empty() || name.contains([':', '/', ' ', '\n']) {
        return None;
    }
    passwd_find(&|u| u.name == name).or_else(|| crate::tools::run_brief("getent", &["passwd", name], 5000).filter(|(code, _)| *code == 0).and_then(|(_, out)| out.lines().find_map(passwd_user)).filter(|u| u.name == name))
}

/// a user by number
pub fn user_of(uid: u32) -> Option<User> {
    passwd_find(&|u| u.uid == uid)
}

/// the person behind `sudo`: who called it (None: not under sudo, or it was root)
pub fn sudo_user() -> Option<User> {
    if !cfg!(unix) || !plat::process::elevated() {
        return None;
    }
    let name = plat::env("SUDO_USER").filter(|n| !n.is_empty() && n != "root")?;
    user_named(&name).filter(|u| u.uid != 0)
}

/// a user's data folder of one of the programs ("relay", "worker"), as that user gets it
pub fn user_data_dir(u: &User, program: &str) -> String {
    let base = if cfg!(target_os = "macos") { "Library/Application Support" } else { ".local/share" };
    bb_core::util::below(&u.home, &format!("{}/buildbalancer/{}", base, program))
}

/// The PATH a user's login shell has, so a service finds what that person installed for
/// themselves (rustup, pipx ...).  ~/.cargo/bin and ~/.local/bin are always part of it: tools
/// installed there later are found without installing the service again.
pub fn login_path(u: &User) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut add = |p: &str| {
        let p = p.trim();
        if p.starts_with('/') && !p.contains(['"', '\n', '\r', '%', '$', '\\']) && !parts.iter().any(|x| x == p) {
            parts.push(String::from(p));
        }
    };
    add(&format!("{}/.cargo/bin", u.home));
    add(&format!("{}/.local/bin", u.home));
    // the login shell's own answer (its profile files are read); the last line that is a PATH
    let asked = crate::tools::run_brief("su", &["-", &u.name, "-c", "printenv PATH"], 8000).filter(|(code, _)| *code == 0).map(|(_, out)| out).unwrap_or_default();
    if let Some(line) = asked.lines().rev().find(|l| l.trim().starts_with('/')) {
        line.split(':').for_each(&mut add);
    }
    if let Some(own) = plat::env("PATH") {
        own.split(':').for_each(&mut add);
    }
    for p in ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"] {
        add(p);
    }
    parts.join(":")
}

fn need_admin() -> Result<(), String> {
    if plat::process::elevated() {
        return Ok(());
    }
    Err(String::from(if cfg!(windows) { "this needs administrator rights: start the terminal with \"Run as administrator\" and run the command again" } else { "this needs root rights: run the command again with sudo" }))
}

/// the unit of a service: JFLib's, with the user, home and PATH added
pub fn unit_text(s: &Service) -> String {
    let spec = plat::service::ServiceSpec { name: s.name, display: s.display, description: s.description, exe: s.exe, args: s.args };
    let mut extra = String::new();
    if let Some(u) = s.user {
        extra += &format!("User={}\nWorkingDirectory={}\n", u.name, u.home);
    }
    if let Some(p) = s.path {
        extra += &format!("Environment=\"PATH={}\"\n", p);
    }
    plat::service::unit_text(&spec).replacen("Type=simple\n", &format!("Type=simple\n{}", extra), 1)
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<(), String> {
    match crate::tools::run_brief("systemctl", args, 30_000) {
        Some((0, _)) => Ok(()),
        Some((_, out)) => Err(format!("systemctl {}: {}", args.join(" "), out.trim())),
        None => Err(format!("systemctl {} gave no answer", args.join(" "))),
    }
}

/// install a service so that it starts with the machine (it is not started yet)
#[cfg(target_os = "linux")]
pub fn install(s: &Service) -> Result<(), String> {
    need_admin()?;
    if plat::fs::stat("/run/systemd/system").is_none() {
        return Err(String::from("this machine does not run systemd, so there is no service to install: start the program from the system's own start-up scripts instead"));
    }
    let path = format!("/etc/systemd/system/{}.service", s.name);
    if plat::fs::stat(&path).is_some() {
        return Err(format!("the service {} is installed already (`service uninstall` removes it, `service start` starts it)", s.name));
    }
    if !plat::write_file(&path, unit_text(s).as_bytes()) {
        return Err(format!("{} cannot be written", path));
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", s.name])
}

#[cfg(not(target_os = "linux"))]
pub fn install(s: &Service) -> Result<(), String> {
    need_admin()?;
    let spec = plat::service::ServiceSpec { name: s.name, display: s.display, description: s.description, exe: s.exe, args: s.args };
    plat::service::install(&spec)
}

pub fn start(name: &str) -> Result<(), String> {
    need_admin()?;
    plat::service::start(name)
}

pub fn stop(name: &str) -> Result<(), String> {
    need_admin()?;
    plat::service::stop(name)
}

pub fn uninstall(name: &str) -> Result<(), String> {
    need_admin()?;
    if plat::service::state(name) == ServiceState::NotInstalled {
        return Err(format!("the service {} is not installed", name));
    }
    plat::service::uninstall(name)
}

pub fn installed(name: &str) -> bool {
    plat::service::state(name) != ServiceState::NotInstalled
}

/// The file a service's manager starts for the program `exe`.  Windows: a copy of its own in
/// Program Files, when the program lies anywhere else (JFLib makes it at `install`: a program
/// in a folder of the user's could be replaced by any program of that user, and the service
/// has the system's rights).  On the other systems it is the program itself.
pub fn program_file(name: &str, exe: &str) -> Option<String> {
    plat::service::program_path(&plat::service::ServiceSpec { name, display: "", description: "", exe, args: &[] })
}

/// Whether the file a service starts is a copy of the program (Windows) that is not the
/// program as it is now: an update replaced the one and not the other.
pub fn program_stale(name: &str, exe: &str) -> bool {
    let Some(place) = program_file(name, exe) else { return false };
    place != exe && plat::file_exists(&place) != 0 && plat::read_file(exe).is_some_and(|new| plat::read_file(&place).is_none_or(|old| old != new))
}

/// After the programs were replaced: make the file the service starts the new program, too.
/// Only Windows has anything to do here (program_file) -- there an update of the programs
/// would leave the service with the copy from before.  A copy that is running cannot be
/// written over, but it can be moved aside: it goes at a later start (it is named as an
/// update names what it replaced), so this works on a service that runs.  A service is
/// never left without its program: if the new one cannot be put there, the old one is back.
pub fn refresh_program(name: &str, exe: &str) -> Result<(), String> {
    let Some(place) = program_file(name, exe) else { return Ok(()) };
    if place == exe {
        return Ok(());
    }
    let aside = format!("{}.old", place);
    // (a replacing that was cut short left the copy under its other name: back with it first)
    if plat::file_exists(&place) == 0 && plat::file_exists(&aside) != 0 {
        plat::rename(&aside, &place);
    }
    if plat::file_exists(&place) == 0 {
        return Ok(());
    }
    let new = plat::read_file(exe).ok_or_else(|| format!("{} cannot be read", exe))?;
    if plat::read_file(&place).is_some_and(|old| old == new) {
        return Ok(());
    }
    need_admin()?;
    let failed = || format!("the service's own copy of the program, {}, could not be replaced by the new one", place);
    if !plat::write_file(&place, &new) {
        plat::delete(&aside);
        if !plat::rename(&place, &aside) {
            return Err(failed());
        }
        if !plat::write_file(&place, &new) {
            plat::delete(&place);
            plat::rename(&aside, &place);
            return Err(failed());
        }
    }
    Ok(())
}

/// a service's state in words
pub fn state_text(name: &str) -> &'static str {
    match plat::service::state(name) {
        ServiceState::NotInstalled => "not installed",
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Running => "running",
        ServiceState::Stopping => "stopping",
        _ => "unknown",
    }
}
