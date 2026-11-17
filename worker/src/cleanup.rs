// cleanup -- nothing is left behind on a machine that lends its power
//
//   remove_project(dir, project, slot)   everything of a project here: its workspaces (the files
//                                        and the build output that grew in them) and the folders
//                                        of its finished jobs -> bytes freed
//   remove_idle(dir, project, slot)      the same when no worker runs on the data folder
//   remove_all(dir, remove)              every project, each through `remove`
//   expire(dir, days, remove)            workspaces nobody used for `days` days go by themselves
//                                        (the worker's setting keep_days)
//   give_up(dir, config, remove)         a place the worker is to keep nothing in any more (a
//                                        drive switched off, or the data folder with --disk 0):
//                                        what lies there goes, and the worker's folder on a drive
//   request(dir, project, slot)          ask the worker that runs on this data folder to remove
//   take_requests(dir, f)                a project (from another program: bb-worker clean)
//   purge(dir)                           the pairing and everything else: the folder is empty
//
// A workspace is a cache: the main machine has the sources, and what a build made of them can
// be made again.  So removing is always safe, except while a job runs in it.  A running worker
// therefore removes through Worker::remove_project, which holds the workspace free of jobs
// meanwhile; remove_project itself is for a data folder nobody works on.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j;
use bb_core::proto::{JobSpec, Workspace};
use bb_core::util::{below, id_ok, now, remove_tree, slug};

use crate::config;

/// a project on this machine: its slots together
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slots: Vec<String>,
    pub bytes: u64,
    /// the last use of any slot
    pub used: i64,
}

/// the projects of a data folder, the largest first
pub fn projects(dir: &str) -> Vec<Project> {
    group(&config::workspaces(dir))
}

/// workspaces (one per slot) as projects
pub fn group(workspaces: &[Workspace]) -> Vec<Project> {
    let mut out: Vec<Project> = Vec::new();
    for w in workspaces {
        let i = match out.iter().position(|p| p.id == w.project_id) {
            Some(i) => i,
            None => {
                out.push(Project { id: w.project_id.clone(), ..Project::default() });
                out.len() - 1
            }
        };
        let p = &mut out[i];
        if p.name.is_empty() {
            p.name = w.project.clone();
        }
        p.slots.push(w.slot.clone());
        p.bytes += w.bytes;
        p.used = p.used.max(w.used);
    }
    for p in out.iter_mut().filter(|p| p.name.is_empty()) {
        p.name = p.id.clone();
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
    out
}

/// what a project cannot be removed for while a job runs in it
pub const BUSY: &str = "a job is running in this project right now; cancel it or wait for it, then remove the project";

/// how a project is removed: (project id, slot) -> bytes freed
pub type Remove<'a> = &'a dyn Fn(&str, &str) -> Result<u64, String>;

/// how one copy is removed from one place: (the place's root, project id, slot) -> bytes freed
pub type RemoveIn<'a> = &'a dyn Fn(&str, &str, &str) -> Result<u64, String>;

/// Remove the copy of a project's slot that lies in one place, and nothing else of it: the
/// copy in another place is the one the next job uses.  The caller makes sure no job runs in
/// it (Worker::remove_in, or the data folder's lock).
pub fn remove_in(root: &str, project_id: &str, slot: &str) -> Result<u64, String> {
    if !id_ok(project_id) || slug(slot) != slot || slot.is_empty() {
        return Err(format!("\"{}\" names no project here", project_id));
    }
    let path = config::ws_dir(root, project_id, slot);
    let bytes = bb_core::util::tree_size(&path, u64::MAX);
    if !remove_tree(&path) {
        return Err(format!("{} could not be removed completely (a file in it is open in another program)", path));
    }
    // the project's folder goes with its last slot there
    plat::delete(&below(root, &format!("ws/{}", project_id)));
    Ok(bytes)
}

/// remove a project's files where no worker runs on the data folder (Err when one does)
pub fn remove_idle(dir: &str, project_id: &str, slot: &str) -> Result<u64, String> {
    let _lock = plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")).ok_or("a worker runs on this data folder (bb-worker or its service): it has to remove the project -- bb-worker clean asks it to")?;
    remove_project(dir, project_id, slot)
}

/// Remove a project's files (`slot` "": every slot).  The caller makes sure no job runs in it.
pub fn remove_project(dir: &str, project_id: &str, slot: &str) -> Result<u64, String> {
    if !id_ok(project_id) || !(slot.is_empty() || slug(slot) == slot) {
        return Err(format!("\"{}\" names no project here", project_id));
    }
    // in every place that has something of it (its slots can lie on different drives)
    let mut bytes = 0u64;
    for root in config::roots(dir) {
        let path = if slot.is_empty() { below(&root, &format!("ws/{}", project_id)) } else { config::ws_dir(&root, project_id, slot) };
        if plat::file_exists(&path) == 0 {
            continue;
        }
        // measured now: the record of the last job may be older than what a build left
        bytes += bb_core::util::tree_size(&path, u64::MAX);
        if !remove_tree(&path) {
            return Err(format!("{} could not be removed completely (a file in it is open in another program)", path));
        }
        if !slot.is_empty() {
            // the project's folder goes with its last slot
            plat::delete(&below(&root, &format!("ws/{}", project_id)));
        }
    }
    // the folders of its jobs that ended: scripts, logs, records
    let jobs = plat::fs::join(dir, "jobs");
    for e in plat::fs::read_dir(&jobs).unwrap_or_default() {
        let jd = plat::fs::join(&jobs, &e.name);
        let Some(spec) = j::load::<JobSpec>(&plat::fs::join(&jd, "spec.json")) else { continue };
        if spec.project_id == project_id && (slot.is_empty() || spec.slot == slot) && plat::file_exists(&plat::fs::join(&jd, "done.json")) == 1 {
            remove_tree(&jd);
        }
    }
    Ok(bytes)
}

/// remove every project nobody works in -> (projects removed, bytes freed, projects kept because a job runs)
pub fn remove_all(dir: &str, remove: Remove) -> (usize, u64, usize) {
    let (mut gone, mut bytes, mut kept) = (0, 0u64, 0);
    for p in projects(dir) {
        match remove(&p.id, "") {
            Ok(b) => {
                gone += 1;
                bytes += b;
            }
            Err(_) => kept += 1,
        }
    }
    (gone, bytes, kept)
}

/// remove the workspaces nobody used for `days` days (0: never) -> how many went
pub fn expire(dir: &str, days: u32, remove: Remove) -> usize {
    if days == 0 {
        return 0;
    }
    let limit = now() - days as i64 * 86_400;
    let mut gone = 0;
    for (root, w) in config::located(dir) {
        // a workspace without a record (a first sync that never finished): its folder's time
        let used = if w.used > 0 { w.used } else { plat::fs::stat(&config::ws_dir(&root, &w.project_id, &w.slot)).map_or(now(), |m| m.mtime_ns.div_euclid(1_000_000_000)) };
        if used < limit && remove(&w.project_id, &w.slot).is_ok() {
            plat::log_info("cleanup", &format!("workspace {} / {} removed: unused for more than {} days", if w.project.is_empty() { &w.project_id } else { &w.project }, w.slot, days));
            gone += 1;
        }
    }
    gone
}

/// The places a worker is to keep nothing in any more -- drives switched off (Store::disk
/// 0) as far as they are there, and the data folder when it lends nothing (Config::disk 0):
/// every copy in them goes through `remove`, which lets no job be cut off and takes only the
/// copy in that place.  Once a drive holds none, the worker's folder on it goes, too (the data
/// folder stays: it holds the pairing).  -> whether all of them are clear (false: a job still
/// runs in one; the next call goes on).
pub fn give_up(dir: &str, c: &config::Config, remove: RemoveIn) -> bool {
    let mut clear = true;
    if c.disk == 0 {
        for w in config::workspaces_in(dir) {
            let _ = remove(dir, &w.project_id, &w.slot);
        }
        clear &= config::workspaces_in(dir).is_empty();
    }
    for s in c.stores.iter().filter(|s| s.disk == 0 && plat::file_exists(&s.path) == 2) {
        let root = config::store_root(&s.path, &c.id);
        if plat::file_exists(&root) == 0 {
            continue;
        }
        for w in config::workspaces_in(&root) {
            // (a job that runs in it keeps it until it ends; the next call goes on)
            let _ = remove(&root, &w.project_id, &w.slot);
        }
        if config::workspaces_in(&root).is_empty() && remove_tree(&root) {
            plat::log_info("cleanup", &format!("{} is used no more: this worker's folder on it is removed", s.path));
            // (the folder all workers share on a drive goes with the last of them: only when empty)
            plat::delete(&plat::fs::join(&s.path, "BuildBalancer"));
        } else {
            clear = false;
        }
    }
    clear
}

/// Forget the drives that were given up and hold nothing of this worker any more.  One that
/// is not there keeps its entry: what lies on it goes when it is plugged in again.
pub fn forget_given_up(c: &mut config::Config) {
    let id = c.id.clone();
    c.stores.retain(|s| s.disk > 0 || plat::file_exists(&s.path) != 2 || plat::file_exists(&config::store_root(&s.path, &id)) != 0);
}

/// leave a request for the worker that runs on this data folder -> the request's file
pub fn request(dir: &str, project_id: &str, slot: &str) -> Option<String> {
    let path = below(dir, &format!("requests/{}.json", bb_core::util::new_id("q")));
    bb_core::util::put_file(&path, j::text(&j::obj([("project_id", j::s(project_id)), ("slot", j::s(slot))])).as_bytes()).then_some(path)
}

/// hand every waiting request to `f(project_id, slot)` and take it away
pub fn take_requests(dir: &str, f: &mut dyn FnMut(&str, &str)) {
    let folder = plat::fs::join(dir, "requests");
    for e in plat::fs::read_dir(&folder).unwrap_or_default() {
        let path = plat::fs::join(&folder, &e.name);
        if let Some(v) = plat::read_file(&path).and_then(|b| j::parse(&b)) {
            f(j::str_of(&v, "project_id"), j::str_of(&v, "slot"));
        }
        plat::delete(&path);
    }
}

/// forget the pairing and remove everything this program keeps here
pub fn purge(dir: &str) -> bool {
    let mut ok = true;
    // what lies on other drives first: afterwards nothing here knows of them any more
    if let Some(c) = config::load(dir) {
        for s in c.stores.iter().filter(|s| plat::file_exists(&s.path) == 2) {
            ok &= remove_tree(&config::store_root(&s.path, &c.id));
            plat::delete(&plat::fs::join(&s.path, "BuildBalancer"));
        }
    }
    for part in ["ws", "jobs", "requests", "logs"] {
        ok &= remove_tree(&plat::fs::join(dir, part));
    }
    for file in ["worker.json", "state.json", "worker.lock"] {
        let p = plat::fs::join(dir, file);
        ok &= plat::file_exists(&p) == 0 || plat::delete(&p);
    }
    ok
}
