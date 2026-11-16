// BuildBalancer -- the desktop app: one window for a machine that lends its power, for the
// main machine, or both.  It is a window around the same code the command-line programs use
// (bb_worker's engine runs inside it, bb_main does the main machine's work).
//
//   BuildBalancer                       the window
//   BuildBalancer --demo                with made-up machines and jobs, nothing is contacted
//   BuildBalancer --config DIR          the main machine's config folder (as `bb --config`)
//   BuildBalancer --data DIR            the worker's data folder (as `bb-worker --data`)
//   BuildBalancer --shot FILE.png, --script FILE.uis ...   JFLib's test options (headless)
//
// Pages: "This machine" (what it lends, what runs, the projects kept here with one click to
// remove them), "Machines" (the workers of the main machine, adding one with a code), "Jobs",
// "Project" (run a project's tasks), "Connect" (pair this machine or make it the main machine),
// "Settings" (look for a newer version, install it with one click: the app starts again by
// itself and the worker in it reconnects).
// Texts are German in the source and English through locales/ui.tsv, as JFLib's apps do it.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

mod pages;
mod state;
mod tr;

use ui::*;

use crate::state::{Gui, Page};
use crate::tr::t;

const DESC: AppDesc = AppDesc { title: "Build Balancer", flags: AF_RESIZABLE | AF_SAVE_STATE | AF_SINGLE_INSTANCE, w: 1040, h: 720, min_w: 760, min_h: 520, ..AppDesc::NONE };

/// widget ids (repeated items take the ids after their base)
pub mod id {
    pub const NAV: u32 = 10;
    pub const NAV_ITEM: u32 = 20;
    pub const PAGE_SCROLL: u32 = 40;
    pub const START: u32 = 100;
    pub const WORKER: u32 = 200;
    pub const PROJECT_ROW: u32 = 400; // + 4 per project
    pub const FLEET: u32 = 1000;
    pub const FLEET_ROW: u32 = 1100; // + 64 per worker
    pub const JOBS: u32 = 4000;
    pub const JOB_ROW: u32 = 4100; // one per job
    pub const PROJECT: u32 = 6000;
    pub const TASK_ROW: u32 = 6100; // + 2 per task
    pub const DIALOG: u32 = 7000;
    pub const SETTINGS: u32 = 8000;
    pub const FILEDLG: u32 = 0x10000;
    /// the tick that refreshes what the pages show
    pub const TIMER: u32 = 1;
}

impl App for Gui {
    fn init(&mut self, ui: &mut Ui) -> bool {
        self.start(ui);
        ui.timer(id::TIMER, 1000);
        true
    }

    fn frame(&mut self, ui: &mut Ui) {
        self.poll_tasks(ui);
        ui.next_size(-1, -1);
        ui.box_begin(0, BOX_H, 0);
        nav(self, ui);
        ui.next_size(-1, -1);
        ui.scroll_begin(id::PAGE_SCROLL, SC_V | BOX_V | pad(24));
        match self.page {
            Page::Connect => pages::connect(self, ui),
            Page::Worker => pages::worker(self, ui),
            Page::Fleet => pages::fleet(self, ui),
            Page::Jobs => pages::jobs(self, ui),
            Page::Project => pages::project(self, ui),
            Page::Settings => pages::settings(self, ui),
        }
        ui.scroll_end();
        ui.box_end();
        pages::dialogs(self, ui);
        ui.toast_host();
    }

    fn command(&mut self, ui: &mut Ui, id: u32, arg: u32) -> bool {
        match id {
            UIC_TIMER if arg == id::TIMER => {
                self.tick(ui);
                ui.timer(id::TIMER, 1000);
                true
            }
            // a worker with running jobs is not closed by a slip of the hand
            UIC_CLOSE => self.close_asked(),
            UIC_EXIT => {
                self.shutdown();
                false
            }
            _ => false,
        }
    }
}

fn nav(g: &mut Gui, ui: &mut Ui) {
    ui.next_size(if g.nav.compact { 56 } else { 212 }, -1);
    ui.nav_begin(id::NAV, &mut g.nav);
    let mut nav = g.nav;
    ui.box_begin(0, BOX_H | BOX_XCENTER | pad(4), 6);
    if ui.nav_toggle(id::NAV + 1, t("Navigation umschalten"), &mut nav) & EV_ACT != 0 {
        g.nav = nav;
    }
    ui.box_end();
    let running = g.wstatus.as_ref().map_or(0, |s| s.running.len());
    let open_jobs = g.jobs.iter().filter(|j| !bb_core::proto::is_final(&j.state)).count();
    let badge = |n: usize| if n > 0 { Some(alloc::format!("{}", n)) } else { None };
    // a newer version shows at the settings, wherever the person is
    let news = g.upd_found.as_ref().filter(|c| c.newer).map(|_| alloc::string::String::from("1"));
    let items: [(Option<&str>, Page, u32, &str, Option<alloc::string::String>); 6] = [
        (None, Page::Worker, IC_POWER, t("Dieses Gerät"), badge(running)),
        (Some(t("Hauptgerät")), Page::Fleet, IC_GRID, t("Geräte"), None),
        (None, Page::Jobs, IC_HISTORY, t("Aufträge"), badge(open_jobs)),
        (None, Page::Project, IC_FOLDER, t("Projekt"), None),
        (None, Page::Connect, IC_LINK, t("Verbinden"), None),
        (None, Page::Settings, IC_SETTINGS, t("Einstellungen"), news),
    ];
    for (i, (section, page, icon, label, badge)) in items.iter().enumerate() {
        if *page == Page::Connect {
            ui.nav_footer();
        } else if let Some(s) = section {
            ui.nav_section(s);
        }
        if ui.nav_item(id::NAV_ITEM + i as u32, *icon, label, g.page == *page, badge.as_deref()) & EV_ACT != 0 {
            g.go(*page);
        }
    }
    ui.nav_end();
}

fn run() -> i32 {
    tr::install();
    let args = base::Args(plat::args());
    // Started by the app itself after an update: the window that started this one is closing.
    // It holds the worker's data folder (and the claim to be the only window) until it is gone.
    let updated = args.value("--after-update").and_then(|p| p.parse::<u32>().ok());
    if let Some(pid) = updated {
        let end = bb_core::util::tick_ms() + 20_000;
        while bb_core::util::tick_ms() < end && bb_exec::sys::processes().iter().any(|p| p.pid == pid) {
            plat::sleep_ms(100);
        }
    }
    // what an update left behind: the files it replaced
    bb_update::at_start();
    let mut g = Gui::new(args.value("--config"), args.value("--data"), args.flag("--demo"));
    g.updated = updated.is_some();
    ui::launch(&DESC.translated(), g, &style::STYLE)
}

ui::main!(run());
