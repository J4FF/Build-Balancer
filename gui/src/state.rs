// state -- what the window knows and what it has running in the background
//
// Everything that waits (the relay, a pairing, a scan and upload, removing a large folder)
// runs as a task on JFLib's blocking pool; the frame only looks at the finished results
// (poll_tasks).  A tick every second refreshes what the open page shows.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bb_core::proto::{self, JobBrief, Offer, Tool, WorkerStatus, Workspace};
use bb_core::util::{now, size_text};
use bb_main::conf::{Ctx, JobRec, MainConfig, PairedWorker, Project};
use bb_main::fleet::WorkerView;
use bb_main::jobs::Request;
use bb_main::ops::{self, Pairing};
use bb_worker::cleanup;
use bb_worker::config::{self, Config};
use bb_worker::{Engine, Worker};
use plat::task::{spawn_blocking, spawn_cancellable, Task};
use ui::*;

use crate::tr::{t, tf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Connect,
    Worker,
    Fleet,
    Jobs,
    Project,
    Settings,
}

type Outcome<T> = Option<Task<Result<T, String>>>;

pub struct Gui {
    pub demo: bool,
    pub main_dir: String,
    pub worker_dir: String,
    /// --config and --data as this window was started with them (a restart passes them on)
    arg_config: Option<String>,
    arg_data: Option<String>,

    // settings: a newer version
    /// this window was started by an update of the app
    pub updated: bool,
    /// look for a newer version when the app starts
    pub upd_auto: i32,
    pub upd_check: Outcome<bb_update::Check>,
    pub upd_found: Option<bb_update::Check>,
    pub upd_error: String,
    /// the update runs: fetch, check, install -> the version (None: there was none after all)
    pub upd_install: Outcome<Option<String>>,
    /// the update waits until the jobs on this machine are done
    pub upd_waiting: bool,
    /// the main machine asked for the update that runs (nobody may be at this machine: an
    /// update that fails is tried again by itself a few times)
    upd_ordered: bool,
    upd_tries: u32,
    upd_retry_at: u64,
    pub nav: NavState,
    pub page: Page,
    ticks: u32,

    // connect: 0 = this machine lends power, 1 = this is the main machine
    pub role: i32,
    pub f_addr: String,
    pub f_secret: String,
    pub f_name: String,
    pub connect_task: Outcome<String>,

    // this machine as a worker
    pub wcfg: Option<Config>,
    pub engine: Option<Engine>,
    /// an engine that was switched off and still ends its jobs
    winding: Option<Arc<Worker>>,
    pub lend: i32,
    /// a slider moved and the settings file does not say so yet
    pub offer_dirty: bool,
    pub engine_error: String,
    pub wstatus: Option<WorkerStatus>,
    pub projects: Vec<cleanup::Project>,
    pub clean_task: Outcome<String>,
    pub ask_leave: Shared<i32>,
    pub ask_close: Shared<i32>,
    close_ok: bool,

    // this machine as the main machine
    pub main: Option<MainConfig>,
    pub fleet: Vec<WorkerView>,
    pub fleet_error: String,
    pub fleet_loaded: bool,
    fleet_task: Outcome<Vec<WorkerView>>,
    pub pairing: Option<Pairing>,
    pub pair_begin: Outcome<Pairing>,
    pub pair_wait: Outcome<PairedWorker>,
    pub act_task: Outcome<String>,
    pub ask_unpair: Shared<i32>,
    pub unpair_name: String,

    pub jobs: Vec<JobRec>,
    jobs_task: Outcome<(Vec<JobRec>, String)>,
    pub job_sel: String,
    pub job_log: String,

    pub project_dir: String,
    pub project: Option<Project>,
    pub project_error: String,
    pub fd_open: Shared<i32>,
    pub f_cmd: String,
    pub f_out: String,
    /// 0 = the least busy worker that fits, 1.. = fleet[i - 1]
    pub run_on: i32,
    pub run_task: Outcome<JobRec>,
}

impl Gui {
    pub fn new(config: Option<&str>, data: Option<&str>, demo: bool) -> Gui {
        let main_dir = bb_main::conf::config_dir(config);
        let worker_dir = bb_core::util::abs_path(&data.map(String::from).unwrap_or_else(config::default_dir));
        Gui {
            demo,
            main_dir,
            worker_dir,
            arg_config: config.map(String::from),
            arg_data: data.map(String::from),
            updated: false,
            upd_auto: 1,
            upd_check: None,
            upd_found: None,
            upd_error: String::new(),
            upd_install: None,
            upd_waiting: false,
            upd_ordered: false,
            upd_tries: 0,
            upd_retry_at: 0,
            nav: NavState::default(),
            page: Page::Connect,
            ticks: 0,
            role: 0,
            f_addr: String::new(),
            f_secret: String::new(),
            f_name: plat::net::hostname(),
            connect_task: None,
            wcfg: None,
            engine: None,
            winding: None,
            lend: 0,
            offer_dirty: false,
            engine_error: String::new(),
            wstatus: None,
            projects: Vec::new(),
            clean_task: None,
            ask_leave: Shared::new(0),
            ask_close: Shared::new(0),
            close_ok: false,
            main: None,
            fleet: Vec::new(),
            fleet_error: String::new(),
            fleet_loaded: false,
            fleet_task: None,
            pairing: None,
            pair_begin: None,
            pair_wait: None,
            act_task: None,
            ask_unpair: Shared::new(0),
            unpair_name: String::new(),
            jobs: Vec::new(),
            jobs_task: None,
            job_sel: String::new(),
            job_log: String::new(),
            project_dir: String::new(),
            project: None,
            project_error: String::new(),
            fd_open: Shared::new(0),
            f_cmd: String::new(),
            f_out: String::new(),
            run_on: 0,
            run_task: None,
        }
    }

    /// before the first frame: who this machine is, and a worker starts lending at once
    pub fn start(&mut self, ui: &mut Ui) {
        if self.demo {
            crate::pages::demo(self);
            return;
        }
        self.wcfg = config::load(&self.worker_dir);
        self.main = ops::main_config(&self.main_dir);
        self.page = match (&self.wcfg, &self.main) {
            (Some(_), _) => Page::Worker,
            (None, Some(_)) => Page::Fleet,
            (None, None) => Page::Connect,
        };
        if self.wcfg.is_some() {
            self.set_lending(true);
        }
        let prefs = base::Ini::load(&plat::fs::join(&self.main_dir, "app.ini"));
        if let Some(d) = prefs.get("project", "dir").filter(|d| plat::file_exists(d) == 2) {
            self.open_project(String::from(d));
        }
        self.upd_auto = (prefs.get("update", "check") != Some("0")) as i32;
        if self.updated {
            ui.toast(IB_SUCCESS, t("Aktualisiert"), &tf("Build Balancer läuft jetzt in Version {}.", &[bb_core::VERSION]), 8000);
        } else if self.upd_auto != 0 {
            self.check_updates(ui);
        }
        self.refresh(ui);
    }

    // ---------------------------------------------------------------- a newer version

    /// look at the project's releases (in the background)
    pub fn check_updates(&mut self, ui: &mut Ui) {
        if self.demo {
            self.upd_found = Some(bb_update::Check { current: String::from(bb_core::VERSION), latest: String::from("0.2.0"), newer: true });
            return;
        }
        if self.upd_check.is_none() {
            self.upd_error.clear();
            self.upd_check = Some(ui.spawn(spawn_blocking(bb_update::check)));
        }
    }

    pub fn set_update_auto(&mut self, on: bool) {
        self.upd_auto = on as i32;
        if !self.demo {
            let path = plat::fs::join(&self.main_dir, "app.ini");
            let mut prefs = base::Ini::load(&path);
            prefs.set("update", "check", if on { "1" } else { "0" });
            prefs.save(&path);
        }
    }

    /// One click: fetch the newest release, put it in the programs' place and start the app
    /// again.  Jobs that run here are waited for first -- an update is no reason to lose a build.
    pub fn update_now(&mut self, ui: &mut Ui) {
        if self.demo {
            ui.toast(IB_INFO, t("Demo"), t("In der Demo wird nichts verändert."), 0);
            return;
        }
        if self.upd_install.is_some() {
            return;
        }
        // From here on no job starts on this machine (one that arrives waits, and runs after
        // the restart); one that runs is waited for.
        if self.engine.as_ref().is_some_and(|e| !e.hold()) {
            self.upd_waiting = true;
            return;
        }
        self.upd_waiting = false;
        self.upd_install = Some(ui.spawn(spawn_blocking(|| bb_worker::engine::update_programs(&mut |_| {}))));
    }

    /// the update did not happen: jobs may start again
    fn release_jobs(&self) {
        if let Some(e) = &self.engine {
            e.release();
        }
    }

    /// Start the program that is in this one's place now and close this window.  The new
    /// window waits until this one is gone, then lends again and reconnects by itself.
    fn restart_app(&mut self, ui: &mut Ui) {
        self.shutdown();
        let mut args: Vec<String> = Vec::new();
        for (name, value) in [("--config", &self.arg_config), ("--data", &self.arg_data)] {
            if let Some(v) = value {
                args.extend([String::from(name), v.clone()]);
            }
        }
        args.extend([String::from("--after-update"), format!("{}", bb_exec::sys::own_pid())]);
        // for tests: more arguments for the window that starts (a script, so it needs no screen)
        if let Some(more) = plat::env("BB_RESTART_ARGS") {
            args.extend(more.split_whitespace().map(String::from));
        }
        if bb_update::restart(&args) {
            self.close_ok = true;
            ui.quit();
        } else {
            ui.toast(IB_WARNING, t("Update eingespielt"), t("Build Balancer konnte sich nicht selbst neu starten: bitte schließen und wieder öffnen."), 0);
            if self.wcfg.is_some() {
                self.set_lending(true);
            }
        }
    }

    pub fn go(&mut self, page: Page) {
        self.page = page;
        self.ticks = 0;
    }

    /// what the page says where this machine has no role yet
    pub fn is_worker(&self) -> bool {
        self.wcfg.is_some()
    }

    pub fn is_main(&self) -> bool {
        self.main.is_some()
    }

    // ---------------------------------------------------------------- this machine lends

    /// the switch "lend power": start or stop the engine
    pub fn set_lending(&mut self, on: bool) {
        if self.demo {
            self.lend = on as i32;
            return;
        }
        if !on {
            if let Some(e) = self.engine.take() {
                // it ends its jobs on its own threads; the window does not wait for that
                self.winding = Some(e.w.clone());
                drop(e);
            }
            self.lend = 0;
            self.wstatus = None;
            return;
        }
        if self.engine.is_some() {
            return;
        }
        if self.winding.as_ref().is_some_and(|w| !w.sched.lock().running.is_empty()) {
            self.engine_error = String::from(t("Die letzten Aufträge werden noch beendet – gleich noch einmal einschalten."));
            self.lend = 0;
            return;
        }
        self.winding = None;
        match Engine::start(&self.worker_dir) {
            Ok(e) => {
                self.engine = Some(e);
                self.engine_error.clear();
                self.lend = 1;
            }
            Err(e) => {
                self.engine_error = e;
                self.lend = 0;
            }
        }
    }

    /// write what the sliders say; a running engine follows within seconds
    pub fn save_offer(&mut self, ui: &mut Ui) {
        if self.demo {
            return;
        }
        if let Some(c) = &self.wcfg
            && let Err(e) = config::save(&self.worker_dir, c)
        {
            ui.toast(IB_ERROR, t("Nicht gespeichert"), &e, 0);
        }
    }

    /// one click: everything of a project on this machine goes (`id` "": of every project)
    pub fn remove_project(&mut self, ui: &mut Ui, id: &str) {
        if self.clean_task.is_some() {
            return;
        }
        if self.demo {
            let before: u64 = self.projects.iter().map(|p| p.bytes).sum();
            self.projects.retain(|p| !id.is_empty() && p.id != id);
            let after: u64 = self.projects.iter().map(|p| p.bytes).sum();
            ui.toast(IB_SUCCESS, t("Entfernt"), &tf("{} frei", &[&size_text(before - after)]), 0);
            return;
        }
        let (dir, id) = (self.worker_dir.clone(), String::from(id));
        let worker = self.engine.as_ref().map(|e| e.w.clone());
        self.clean_task = Some(ui.spawn(spawn_blocking(move || {
            // the running worker keeps jobs out of a workspace while it goes
            let remove = |p: &str, s: &str| match &worker {
                Some(w) => w.remove_project(p, s),
                None => cleanup::remove_idle(&dir, p, s),
            };
            if id.is_empty() {
                let (gone, bytes, kept) = cleanup::remove_all(&dir, &remove);
                Ok(if kept == 0 { tf("{} Projekte entfernt, {} frei", &[&format!("{}", gone), &size_text(bytes)]) } else { tf("{} Projekte entfernt, {} frei; {} bleiben, weil dort ein Auftrag läuft", &[&format!("{}", gone), &size_text(bytes), &format!("{}", kept)]) })
            } else {
                Ok(tf("{} frei", &[&size_text(remove(&id, "")?)]))
            }
        })));
    }

    /// forget the pairing and remove everything this machine kept for it
    pub fn leave(&mut self, ui: &mut Ui) {
        self.set_lending(false);
        if !self.demo && !cleanup::purge(&self.worker_dir) {
            ui.toast(IB_WARNING, t("Nicht alles entfernt"), t("Eine Datei ist noch in einem anderen Programm geöffnet."), 0);
        }
        self.wcfg = None;
        self.projects.clear();
        self.go(Page::Connect);
        ui.toast(IB_SUCCESS, t("Kopplung aufgehoben"), t("Auf diesem Gerät ist nichts mehr von Build Balancer übrig."), 0);
    }

    // ---------------------------------------------------------------- connect

    /// the button of the page "Connect": pair this machine, or make it the main machine
    pub fn connect(&mut self, ui: &mut Ui) {
        if self.connect_task.is_some() {
            return;
        }
        let (mut addr, mut secret) = (String::from(self.f_addr.trim()), String::from(self.f_secret.trim()));
        // a whole line pasted into the first field
        let line = format!("{} {}", addr, secret);
        if let Some((a, s)) = bb_worker::join::parse_line(&line).or_else(|| ops::parse_setup_line(&line)) {
            (addr, secret) = (a, s);
        }
        if addr.is_empty() || secret.is_empty() {
            ui.toast(IB_WARNING, t("Es fehlt noch etwas"), t("Adresse und Code (oder Schlüssel) eintragen – oder die ganze Zeile einfügen."), 0);
            return;
        }
        let name = String::from(self.f_name.trim());
        if self.demo {
            ui.toast(IB_INFO, t("Demo"), t("In der Demo wird nichts verbunden."), 0);
            return;
        }
        if self.role == 0 {
            let dir = self.worker_dir.clone();
            self.connect_task = Some(ui.spawn(spawn_blocking(move || {
                let mut cfg = bb_worker::join::fresh_config(&dir);
                if !name.is_empty() {
                    cfg.name = name;
                }
                let cfg = bb_worker::join::pair(&dir, &addr, &secret, cfg, &mut |_| {})?;
                Ok(tf("Gekoppelt mit {} als {}", &[&cfg.main_name, &cfg.name]))
            })));
        } else {
            let dir = self.main_dir.clone();
            self.connect_task = Some(ui.spawn(spawn_blocking(move || {
                let m = ops::setup(&dir, &addr, &secret, (!name.is_empty()).then_some(name.as_str()), true)?;
                Ok(tf("{} ist jetzt das Hauptgerät von {}", &[&m.name, &m.relay]))
            })));
        }
    }

    // ---------------------------------------------------------------- the main machine

    pub fn refresh_fleet(&mut self, ui: &mut Ui) {
        if self.demo || !self.is_main() || self.fleet_task.is_some() {
            return;
        }
        let dir = self.main_dir.clone();
        self.fleet_task = Some(ui.spawn(spawn_blocking(move || bb_main::fleet::fleet(&Ctx::load(&dir)?))));
    }

    fn refresh_jobs(&mut self, ui: &mut Ui) {
        if self.demo || !self.is_main() || self.jobs_task.is_some() {
            return;
        }
        let (dir, sel) = (self.main_dir.clone(), self.job_sel.clone());
        self.jobs_task = Some(ui.spawn(spawn_blocking(move || {
            let ctx = Ctx::load(&dir)?;
            let mut all = ctx.jobs();
            all.truncate(60);
            for r in all.iter_mut().filter(|r| !proto::is_final(&r.state)) {
                let _ = bb_main::jobs::pump(&ctx, r, 0);
            }
            let log = all.iter().find(|r| r.id == sel).map(|r| bb_main::jobs::log_tail(&ctx, r, 400)).unwrap_or_default();
            Ok((all, log))
        })));
    }

    /// open a pairing: the code shows as soon as the relay has the slot
    pub fn pair(&mut self, ui: &mut Ui) {
        if self.pair_begin.is_some() || self.pairing.is_some() {
            return;
        }
        if self.demo {
            self.pairing = Some(crate::pages::demo_pairing());
            return;
        }
        let dir = self.main_dir.clone();
        self.pair_begin = Some(ui.spawn(spawn_blocking(move || ops::pair_begin(&Ctx::load(&dir)?, ops::PAIR_MINUTES))));
    }

    pub fn pair_cancel(&mut self) {
        self.pairing = None;
        self.pair_wait = None; // dropping the task tells its loop to stop
    }

    /// something the main machine does that ends in one line: unpair, clean, cancel, fetch
    pub fn act(&mut self, ui: &mut Ui, f: impl FnOnce(&mut Ctx) -> Result<String, String> + Send + 'static) {
        if self.demo {
            ui.toast(IB_INFO, t("Demo"), t("In der Demo wird nichts verändert."), 0);
            return;
        }
        if self.act_task.is_some() {
            return;
        }
        let dir = self.main_dir.clone();
        self.act_task = Some(ui.spawn(spawn_blocking(move || f(&mut Ctx::load(&dir)?))));
    }

    pub fn select_job(&mut self, ui: &mut Ui, id: &str) {
        self.job_sel = String::from(id);
        self.job_log.clear();
        if self.demo {
            self.job_log = String::from(crate::pages::DEMO_LOG);
        }
        self.jobs_task = None;
        self.refresh_jobs(ui);
    }

    pub fn open_project(&mut self, dir: String) {
        self.project_error.clear();
        self.project = if plat::file_exists(&plat::fs::join(&dir, bb_main::conf::PROJECT_FILE)) == 1 {
            match Project::load(&dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.project_error = e;
                    None
                }
            }
        } else {
            None
        };
        self.project_dir = dir;
        if !self.demo {
            let path = plat::fs::join(&self.main_dir, "app.ini");
            let mut prefs = base::Ini::load(&path);
            prefs.set("project", "dir", &self.project_dir);
            prefs.save(&path);
        }
    }

    /// send a job: a task of the project (`task` its name) or the command of the text field
    pub fn run(&mut self, ui: &mut Ui, task: Option<String>) {
        if self.run_task.is_some() {
            return;
        }
        if self.demo {
            ui.toast(IB_INFO, t("Demo"), t("In der Demo wird nichts gestartet."), 0);
            return;
        }
        let Some(project) = self.project.clone() else { return };
        let worker = if self.run_on > 0 { self.fleet.get(self.run_on as usize - 1).map(|v| v.paired.name.clone()).unwrap_or_default() } else { String::new() };
        let req = match &task {
            Some(name) => {
                let Ok(t) = project.task(name) else { return };
                Request {
                    worker: if worker.is_empty() { t.worker.clone() } else { worker },
                    needs: t.needs.clone(),
                    cpu: t.cpu,
                    ram: t.ram,
                    timeout: t.timeout,
                    argv: t.argv.clone(),
                    script: t.script.clone(),
                    shell: if t.script.is_empty() { String::new() } else if t.shell.is_empty() { String::from("default") } else { t.shell.clone() },
                    cwd: t.cwd.clone(),
                    env: t.env.clone(),
                    outputs: t.outputs.clone(),
                    slot: t.slot.clone(),
                    label: t.name.clone(),
                    sync: true,
                }
            }
            None => {
                let argv = bb_main::conf::split_args(&self.f_cmd);
                if argv.is_empty() {
                    ui.toast(IB_WARNING, t("Kein Befehl"), t("Einen Befehl eintragen, zum Beispiel: cargo build --release"), 0);
                    return;
                }
                Request { worker, argv, outputs: bb_main::conf::split_list(&self.f_out), sync: true, ..Request::default() }
            }
        };
        let dir = self.main_dir.clone();
        self.run_task = Some(ui.spawn(spawn_blocking(move || {
            let ctx = Ctx::load(&dir)?;
            let mut views = bb_main::fleet::fleet(&ctx)?;
            bb_main::jobs::submit(&ctx, Some(&project), &req, &mut views, &mut |_| {})
        })));
    }

    // ---------------------------------------------------------------- the clock

    /// once a second: what the open page shows is brought up to date
    pub fn tick(&mut self, ui: &mut Ui) {
        self.ticks += 1;
        if self.demo {
            return;
        }
        // an update that waited for the jobs here, or one that was asked for from outside: the
        // main machine's word, or `bb-worker update` that replaced the programs already
        if self.upd_waiting {
            self.update_now(ui);
        }
        let wanted = self.engine.as_ref().map_or(0, |e| e.w.restart.load(core::sync::atomic::Ordering::Relaxed));
        if wanted == bb_worker::engine::RESTART_ONLY {
            // the programs were replaced already: start again as soon as no job runs
            if self.engine.as_ref().is_some_and(|e| e.hold()) {
                self.restart_app(ui);
                return;
            }
        } else if wanted != 0 && self.upd_install.is_none() && !self.upd_waiting && bb_core::util::tick_ms() >= self.upd_retry_at {
            // the main machine's word: taken from the engine now, and tried again from here if it fails
            if let Some(e) = &self.engine {
                e.w.restart.swap(0, core::sync::atomic::Ordering::Relaxed);
            }
            self.upd_ordered = true;
            self.update_now(ui);
        }
        // this machine: cheap, and the navigation shows its running jobs on every page
        if let Some(e) = &self.engine {
            self.wstatus = Some(e.status());
        }
        if self.is_worker() && (self.page == Page::Worker || self.ticks % 5 == 0) {
            self.projects = cleanup::projects(&self.worker_dir);
        }
        if self.wcfg.is_some() && self.ticks % 3 == 0 && !self.offer_dirty {
            // settings changed from outside (bb-worker config)
            if let Some(c) = config::load(&self.worker_dir) {
                self.wcfg = Some(c);
            }
        }
        match self.page {
            Page::Fleet | Page::Project if self.ticks % 4 == 1 => self.refresh_fleet(ui),
            Page::Jobs if self.ticks % 2 == 1 => self.refresh_jobs(ui),
            _ => {}
        }
        if self.ticks % 6 == 0 {
            self.refresh_jobs(ui);
        }
        ui.invalidate();
    }

    pub fn refresh(&mut self, ui: &mut Ui) {
        self.refresh_fleet(ui);
        self.refresh_jobs(ui);
        if self.is_worker() {
            self.projects = cleanup::projects(&self.worker_dir);
        }
    }

    /// take what the background tasks finished
    pub fn poll_tasks(&mut self, ui: &mut Ui) {
        if let Some(r) = done(&mut self.upd_check) {
            match r {
                Ok(c) => self.upd_found = Some(c),
                Err(e) => self.upd_error = e,
            }
        }
        if let Some(r) = done(&mut self.upd_install) {
            match r {
                Ok(Some(_)) => {
                    self.restart_app(ui);
                    return;
                }
                Ok(None) => {
                    self.release_jobs();
                    (self.upd_ordered, self.upd_tries) = (false, 0);
                    self.upd_found = Some(bb_update::Check { current: String::from(bb_core::VERSION), latest: String::from(bb_core::VERSION), newer: false });
                }
                Err(e) => {
                    self.release_jobs();
                    self.upd_tries += 1;
                    if self.upd_ordered && self.upd_tries < 4 {
                        // asked for from afar: a download can fail, so once more in a minute
                        self.upd_retry_at = bb_core::util::tick_ms() + 60_000;
                        if let Some(en) = &self.engine {
                            en.w.restart.fetch_max(bb_worker::engine::RESTART_UPDATE, core::sync::atomic::Ordering::Relaxed);
                        }
                    } else {
                        (self.upd_ordered, self.upd_tries) = (false, 0);
                        ui.toast(IB_ERROR, t("Update fehlgeschlagen"), &e, 15_000);
                    }
                }
            }
        }
        if let Some(r) = done(&mut self.connect_task) {
            match r {
                Ok(msg) => {
                    ui.toast(IB_SUCCESS, t("Verbunden"), &msg, 0);
                    self.f_secret.clear();
                    let was_worker = self.is_worker();
                    self.wcfg = config::load(&self.worker_dir);
                    self.main = ops::main_config(&self.main_dir);
                    if self.role == 0 && self.wcfg.is_some() {
                        if !was_worker || self.engine.is_none() {
                            self.set_lending(false);
                            self.set_lending(true);
                        }
                        self.go(Page::Worker);
                    } else if self.main.is_some() {
                        self.go(Page::Fleet);
                        self.refresh_fleet(ui);
                    }
                }
                Err(e) => {
                    ui.toast(IB_ERROR, t("Das hat nicht geklappt"), &e, 12_000);
                }
            }
        }
        if let Some(r) = done(&mut self.clean_task) {
            match r {
                Ok(msg) => ui.toast(IB_SUCCESS, t("Entfernt"), &msg, 0),
                Err(e) => ui.toast(IB_WARNING, t("Nicht entfernt"), &e, 10_000),
            };
            self.projects = cleanup::projects(&self.worker_dir);
        }
        if let Some(r) = done(&mut self.fleet_task) {
            self.fleet_loaded = true;
            match r {
                Ok(v) => {
                    self.fleet = v;
                    self.fleet_error.clear();
                }
                Err(e) => self.fleet_error = e,
            }
        }
        if let Some(r) = done(&mut self.pair_begin) {
            match r {
                Ok(p) => {
                    // wait for the other machine until the code runs out or the card is closed
                    let (dir, p2) = (self.main_dir.clone(), p.clone());
                    self.pair_wait = Some(ui.spawn(spawn_cancellable(move |stop| {
                        let mut ctx = Ctx::load(&dir)?;
                        loop {
                            if stop.is_cancelled() {
                                return Err(String::new());
                            }
                            if let Some(w) = ops::pair_poll(&mut ctx, &p2, 5)? {
                                return Ok(w);
                            }
                        }
                    })));
                    self.pairing = Some(p);
                }
                Err(e) => {
                    ui.toast(IB_ERROR, t("Kein Code"), &e, 10_000);
                }
            }
        }
        if let Some(r) = done(&mut self.pair_wait) {
            self.pairing = None;
            match r {
                Ok(w) => {
                    ui.toast(IB_SUCCESS, t("Gerät gekoppelt"), &tf("{} ({}) gehört jetzt dazu.", &[&w.name, &w.os]), 0);
                    self.refresh_fleet(ui);
                }
                Err(e) if !e.is_empty() => {
                    ui.toast(IB_WARNING, t("Nicht gekoppelt"), &e, 10_000);
                }
                Err(_) => {}
            }
        }
        if let Some(r) = done(&mut self.act_task) {
            match r {
                Ok(msg) => ui.toast(IB_SUCCESS, t("Erledigt"), &msg, 0),
                Err(e) => ui.toast(IB_ERROR, t("Das hat nicht geklappt"), &e, 12_000),
            };
            self.refresh_fleet(ui);
            self.jobs_task = None;
            self.refresh_jobs(ui);
        }
        if let Some(r) = done(&mut self.jobs_task) {
            if let Ok((jobs, log)) = r {
                self.jobs = jobs;
                if !self.job_sel.is_empty() {
                    self.job_log = log;
                }
            }
        }
        if let Some(r) = done(&mut self.run_task) {
            match r {
                Ok(rec) => {
                    ui.toast(IB_SUCCESS, t("Auftrag unterwegs"), &tf("{} läuft auf {}.", &[&rec.id, &rec.worker]), 0);
                    self.job_sel = rec.id.clone();
                    self.job_log.clear();
                    self.go(Page::Jobs);
                    self.refresh_jobs(ui);
                }
                Err(e) => {
                    ui.toast(IB_ERROR, t("Nicht gestartet"), &e, 15_000);
                }
            }
        }
    }

    // ---------------------------------------------------------------- the end

    /// the window is to close: true keeps it open (a question is on the screen)
    pub fn close_asked(&mut self) -> bool {
        let running = self.engine.as_ref().map_or(0, |e| e.w.sched.lock().running.len());
        if running > 0 && !self.close_ok {
            self.ask_close.set(1);
            return true;
        }
        false
    }

    pub fn close_anyway(&mut self, ui: &mut Ui) {
        self.close_ok = true;
        ui.quit();
    }

    pub fn shutdown(&mut self) {
        if let Some(e) = self.engine.take() {
            e.stop();
        }
    }
}

/// the result of a task that finished (the slot is free again)
fn done<T>(slot: &mut Outcome<T>) -> Option<Result<T, String>> {
    if !slot.as_ref().is_some_and(|t| t.is_done()) {
        return None;
    }
    slot.take().and_then(|t| t.take())
}

// what the demo and the pages build records from
pub fn offer(cpu: u32, ram_gib: u64, disk_gib: u64) -> Offer {
    Offer { cpu, ram: ram_gib << 30, disk: disk_gib << 30, jobs: 4 }
}

pub fn brief(id: &str, project: &str, state: &str, cpu: u32, command: &str) -> JobBrief {
    JobBrief { id: String::from(id), project: String::from(project), slot: String::from("main"), state: String::from(state), cpu, ram: 0, started: now() - 95, command: String::from(command) }
}

pub fn tools(names: &[&str]) -> Vec<Tool> {
    names.iter().map(|n| Tool { name: String::from(*n), version: String::new(), path: String::new() }).collect()
}

pub fn workspace(id: &str, name: &str, gib10: u64, days: i64) -> Workspace {
    Workspace { project_id: String::from(id), project: String::from(name), slot: String::from("main"), bytes: (gib10 << 30) / 10, used: now() - days * 86_400 - 3600 }
}
