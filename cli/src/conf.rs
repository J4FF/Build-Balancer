// conf -- what the main machine keeps, and a project's settings
//
// In the user's config folder (Windows %APPDATA%\buildbalancer, Linux ~/.config/buildbalancer;
// --config DIR or the variable BB_CONFIG names another):
//
//   main.json          this machine's identity: relay, token, secret key, the fleet's data key
//   workers.json       the paired workers with their public keys
//   jobs/<id>.json     a job's record;  jobs/<id>.log  its output
//   projects/<id>/known   what the last scan of a project hashed (so unchanged files are not read again)
//
// In a project, one file -- buildbalancer.ini in its root:
//
//   [project]  name, id (made by `bb init`: it names the project's workspace on every worker)
//   [sync]     gitignore = true | false, git = false | true, exclude = ..., include = ...
//   [task.NAME]  run = COMMAND ARGS   or   sh = SCRIPT LINE
//                needs, worker, cpu, ram, timeout, outputs, slot, cwd, env
//
// Patterns for many files belong into .bbignore (the syntax of .gitignore).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use agent_core::record;
use base::Ini;
use bb_core::client::Relay;
use bb_core::crypt::{DataKey, Keys};
use bb_core::proto::{EnvVar, TreeRef};
use bb_core::util::below;

pub const PROJECT_FILE: &str = "buildbalancer.ini";

record! {
    pub struct MainConfig {
        pub id: String,
        pub name: String,
        pub relay: String,
        pub relay_fp: String,
        pub relay_id: String,
        pub token: String,
        /// this machine's X25519 secret (hex)
        pub secret: String,
        /// the fleet's data key (hex)
        pub data_key: String,
    }
}

record! {
    pub struct PairedWorker {
        pub id: String,
        pub name: String,
        /// its X25519 public key (hex)
        pub public: String,
        pub os: String,
        pub arch: String,
        pub host: String,
        pub added: i64,
    }
}

record! {
    pub struct WorkersFile {
        pub workers: Vec<PairedWorker>,
    }
}

record! {
    /// what the main machine knows of a job
    pub struct JobRec {
        pub id: String,
        pub worker_id: String,
        pub worker: String,
        pub project_id: String,
        pub project: String,
        /// the project's folder on this machine (outputs come back into it)
        pub project_dir: String,
        pub slot: String,
        pub label: String,
        pub command: String,
        pub outputs: Vec<String>,
        pub state: String,
        pub detail: String,
        /// the number of the last event read from the job's channel
        pub after: u64,
        pub created: i64,
        pub started: i64,
        pub ended: i64,
        pub exit: i64,
        pub error: String,
        pub duration_ms: u64,
        pub peak_ram: u64,
        pub sync_files: u64,
        pub sync_bytes: u64,
        pub artifacts: TreeRef,
        /// the outputs were written to this folder ("" = not fetched yet)
        pub fetched_to: String,
        pub fetched_files: u64,
        pub log_bytes: u64,
    }
}

/// the config folder of this run
pub fn config_dir(cli_value: Option<&str>) -> String {
    if let Some(d) = cli_value.map(String::from).or_else(|| plat::env("BB_CONFIG")).filter(|d| !d.is_empty()) {
        return bb_core::util::abs_path(&d);
    }
    let base = plat::config_dir().unwrap_or_else(|| String::from("."));
    below(&base, "buildbalancer")
}

/// everything a command needs to talk to the fleet
pub struct Ctx {
    pub dir: String,
    pub main: MainConfig,
    pub keys: Keys,
    pub data: DataKey,
    pub relay: Relay,
    pub workers: Vec<PairedWorker>,
}

impl Ctx {
    pub fn load(dir: &str) -> Result<Ctx, String> {
        let main = bb_core::j::load::<MainConfig>(&plat::fs::join(dir, "main.json")).filter(|m| !m.id.is_empty()).ok_or_else(|| {
            String::from("this machine is not set up yet.  Start bb-relay on a machine every device can reach; it prints the line to run here:  bb setup RELAY KEY")
        })?;
        let keys = Keys::from_hex(&main.secret).ok_or("main.json: the secret key is damaged")?;
        let data = DataKey::new(&bb_core::crypt::key32(&main.data_key).ok_or("main.json: the data key is damaged")?);
        let relay = Relay::open(&main.relay, &main.relay_fp, &main.id, &main.token)?;
        let workers = bb_core::j::load::<WorkersFile>(&plat::fs::join(dir, "workers.json")).unwrap_or_default().workers;
        Ok(Ctx { dir: String::from(dir), main, keys, data, relay, workers })
    }

    pub fn save_workers(&self) -> Result<(), String> {
        let f = WorkersFile { workers: self.workers.clone() };
        if bb_core::j::save(&plat::fs::join(&self.dir, "workers.json"), &f) { Ok(()) } else { Err(format!("{} cannot be written", plat::fs::join(&self.dir, "workers.json"))) }
    }

    /// the key shared with a worker
    pub fn pair_key(&self, w: &PairedWorker) -> Option<[u8; 32]> {
        bb_core::crypt::pair_key(&self.keys, &bb_core::crypt::key32(&w.public)?)
    }

    /// a paired worker by name or id (names compare without case)
    pub fn worker(&self, name: &str) -> Result<&PairedWorker, String> {
        self.workers.iter().find(|w| w.id == name || w.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
            let names: Vec<&str> = self.workers.iter().map(|w| w.name.as_str()).collect();
            if names.is_empty() { String::from("no worker is paired yet: bb pair") } else { format!("no worker \"{}\"; paired: {}", name, names.join(", ")) }
        })
    }

    pub fn jobs_dir(&self) -> String {
        plat::fs::join(&self.dir, "jobs")
    }

    pub fn job_path(&self, id: &str, ext: &str) -> String {
        plat::fs::join(&self.jobs_dir(), &format!("{}.{}", id, ext))
    }

    pub fn save_job(&self, r: &JobRec) -> bool {
        bb_core::util::put_file(&self.job_path(&r.id, "json"), bb_core::j::pretty(&bb_core::j::Json::to_json(r)).as_bytes())
    }

    pub fn load_job(&self, id: &str) -> Option<JobRec> {
        bb_core::j::load::<JobRec>(&self.job_path(id, "json")).filter(|r| !r.id.is_empty())
    }

    /// every job record, newest first
    pub fn jobs(&self) -> Vec<JobRec> {
        let mut v: Vec<JobRec> = plat::fs::read_dir(&self.jobs_dir()).unwrap_or_default().iter().filter_map(|e| e.name.strip_suffix(".json")).filter_map(|id| self.load_job(id)).collect();
        v.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        v
    }

    /// A job by its id, the start of its id, or "last" for the newest.
    pub fn find_job(&self, name: &str) -> Result<JobRec, String> {
        if let Some(r) = self.load_job(name) {
            return Ok(r);
        }
        let all = self.jobs();
        if name == "last" {
            return all.into_iter().next().ok_or_else(|| String::from("there is no job yet"));
        }
        let hits: Vec<JobRec> = all.into_iter().filter(|r| r.id.starts_with(name) || r.id.trim_start_matches("j-").starts_with(name)).collect();
        match hits.len() {
            1 => Ok(hits.into_iter().next().unwrap_or_default()),
            0 => Err(format!("no job \"{}\" (bb jobs lists them)", name)),
            _ => Err(format!("\"{}\" fits several jobs: {}", name, hits.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }
}

/// a task of a project file
#[derive(Clone, Debug, Default)]
pub struct Task {
    pub name: String,
    pub argv: Vec<String>,
    pub script: String,
    pub shell: String,
    pub needs: String,
    pub worker: String,
    pub cpu: u32,
    pub ram: u64,
    pub timeout: u64,
    pub outputs: Vec<String>,
    pub slot: String,
    pub cwd: String,
    pub env: Vec<EnvVar>,
}

/// a project: its folder and what buildbalancer.ini says
#[derive(Clone, Debug, Default)]
pub struct Project {
    pub dir: String,
    pub id: String,
    pub name: String,
    pub sync: bb_sync::Options,
    pub tasks: Vec<Task>,
}

/// a command line as one text -> its words; "double quotes" keep blanks together
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(core::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// a list in one settings value: parts between ';', or between blanks when there is no ';'
pub fn split_list(s: &str) -> Vec<String> {
    let parts: Vec<&str> = if s.contains(';') { s.split(';').collect() } else { s.split_whitespace().collect() };
    parts.iter().map(|p| String::from(p.trim())).filter(|p| !p.is_empty()).collect()
}

/// "NAME=value" pairs
pub fn parse_env(items: &[String]) -> Result<Vec<EnvVar>, String> {
    items.iter().map(|e| e.split_once('=').filter(|(k, _)| !k.trim().is_empty()).map(|(k, v)| EnvVar { name: String::from(k.trim()), value: String::from(v) }).ok_or_else(|| format!("\"{}\": a variable is NAME=value", e))).collect()
}

impl Project {
    /// the project whose folder is `start` or lies above it
    pub fn find(start: &str) -> Option<Project> {
        let mut dir = String::from(start);
        loop {
            if plat::file_exists(&plat::fs::join(&dir, PROJECT_FILE)) == 1 {
                return Project::load(&dir).ok();
            }
            let parent = String::from(plat::fs::parent(&dir));
            if parent.is_empty() || parent == dir {
                return None;
            }
            dir = parent;
        }
    }

    pub fn load(dir: &str) -> Result<Project, String> {
        let path = plat::fs::join(dir, PROJECT_FILE);
        let ini = Ini::load_checked(&path).ok_or_else(|| format!("{} cannot be read", path))?;
        let id = String::from(ini.get("project", "id").unwrap_or(""));
        if !bb_core::util::id_ok(&id) {
            return Err(format!("{}: [project] id is missing or not an id; `bb init` writes one", path));
        }
        let name = ini.get("project", "name").map(String::from).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
        let flag = |key: &str, def: bool| ini.get_int("sync", key, def as i64) != 0;
        let sync = bb_sync::Options {
            gitignore: flag("gitignore", true),
            git_dir: flag("git", false),
            exclude: split_list(ini.get("sync", "exclude").unwrap_or("")),
            include: split_list(ini.get("sync", "include").unwrap_or("")),
        };
        let mut tasks = Vec::new();
        for sec in ini.sections() {
            let Some(tname) = sec.strip_prefix("task.").or_else(|| sec.strip_prefix("Task.")) else { continue };
            let get = |k: &str| ini.get(sec, k).unwrap_or("").trim();
            let t = Task {
                name: String::from(tname),
                argv: split_args(get("run")),
                script: String::from(get("sh")),
                shell: String::from(get("shell")),
                needs: String::from(get("needs")),
                worker: String::from(get("worker")),
                cpu: get("cpu").parse().unwrap_or(0),
                ram: bb_core::util::size_parse(get("ram")).unwrap_or(0),
                timeout: bb_core::util::dur_parse(get("timeout")).unwrap_or(0),
                outputs: split_list(get("outputs")),
                slot: String::from(get("slot")),
                cwd: String::from(get("cwd")),
                env: parse_env(&split_list(get("env"))).map_err(|e| format!("{}: [{}] env: {}", path, sec, e))?,
            };
            if t.argv.is_empty() && t.script.is_empty() {
                return Err(format!("{}: [{}] needs  run = COMMAND  or  sh = SCRIPT", path, sec));
            }
            tasks.push(t);
        }
        Ok(Project { dir: String::from(dir), id, name, sync, tasks })
    }

    pub fn task(&self, name: &str) -> Result<&Task, String> {
        self.tasks.iter().find(|t| t.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
            let names: Vec<&str> = self.tasks.iter().map(|t| t.name.as_str()).collect();
            if names.is_empty() { format!("{} defines no task yet: add a section [task.NAME] with run = COMMAND", PROJECT_FILE) } else { format!("no task \"{}\"; there are: {}", name, names.join(", ")) }
        })
    }
}

/// the text `bb init` writes
pub fn project_template(name: &str, id: &str) -> String {
    format!(
        "# Build Balancer: this folder is a project that can be built on other machines.\n\
         # `bb run -- COMMAND` sends its files to a worker and runs COMMAND there;\n\
         # `bb task NAME` runs a task defined below.  `bb guide` explains everything.\n\
         \n\
         [project]\n\
         name = {name}\n\
         # names this project's workspace on every worker: keep it\n\
         id = {id}\n\
         \n\
         [sync]\n\
         # what the workers get: every file here except what .gitignore and .bbignore leave out\n\
         # (so build output stays at home and each worker keeps its own).\n\
         gitignore = true\n\
         # the .git folder itself (only for builds that read the commit from it)\n\
         git = false\n\
         # more to leave out / to take along after all (.gitignore syntax, ';' between patterns)\n\
         exclude =\n\
         include =\n\
         \n\
         # A task is a command with what it needs and what comes back.  Examples:\n\
         #\n\
         # [task.build-linux]\n\
         # run = cargo build --release\n\
         # needs = os=linux tool=cargo cpu>=4\n\
         # outputs = target/release/myapp\n\
         #\n\
         # [task.tests]\n\
         # sh = make -j$BB_CPUS && make test\n\
         # needs = os=linux ram>=8G\n\
         # cpu = 8\n\
         # ram = 8G\n\
         # timeout = 1h\n\
         # outputs = reports/*.xml\n"
    )
}
