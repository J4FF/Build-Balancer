// bb-worker -- lends this machine's power to a main machine (the command line; the Build
// Balancer app does the same with a window)
//
//   bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]
//   bb-worker run
//   bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]
//                    [--keep-days N] [--tags a,b] [--tools x,y] [--name NAME]      (no option: show)
//   bb-worker status
//   bb-worker tools                         the build tools jobs can use on this machine
//   bb-worker clean [PROJECT | --all]       remove a project's files here (no argument: list them)
//   bb-worker leave [--purge]               forget the pairing (--purge: and every file kept here)
//   bb-worker service install | uninstall | start | stop | status
//   bb-worker --version | --help                                (every command takes --data DIR)
//
// A worker opens no port.  It asks the relay for orders (a long poll), runs jobs of the one
// main machine it paired with, and reports what it offers and does.  Its owner decides how
// much it lends: processors (jobs are pinned to them), memory (a job that takes the jobs
// beyond it is ended), disk space for the projects' workspaces (docs/ARCHITECTURE.md).
//
// The main machine is trusted: its jobs run as the user this program runs as, with that
// user's rights.  Pair only with your own main machine.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::util::{out, size_text, tick_ms, Cli};
use bb_worker::engine::not_paired;
use bb_worker::{cleanup, config, engine, join};

const SERVICE: &str = if cfg!(windows) {
    "BuildBalancerWorker"
} else if cfg!(target_os = "macos") {
    "org.buildbalancer.worker"
} else {
    "buildbalancer-worker"
};
const VALUED: &[&str] = &["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--keep-days", "--tags", "--tools"];
const KNOWN: &[&str] = &["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--keep-days", "--tags", "--tools", "--force", "--service", "--all", "--purge"];

fn fail(s: &str) -> i32 {
    plat::log_error("worker", s);
    1
}

fn usage() -> i32 {
    out("bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]\n\
         bb-worker run                      work until stopped\n\
         bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]\n\
         \x20                [--keep-days N] [--tags a,b] [--tools x,y] [--name NAME]     show or change what is lent\n\
         bb-worker status\n\
         bb-worker tools                    the build tools jobs can use on this machine\n\
         bb-worker clean [PROJECT | --all]  remove a project's files here (no argument: list them)\n\
         bb-worker leave [--purge]          forget the pairing (--purge: and every file kept here)\n\
         bb-worker service install | uninstall | start | stop | status\n\
         bb-worker --version\n\
         (every command: --data DIR)\n\
         \n\
         RELAY and CODE come from `bb pair` on the main machine.\n");
    2
}

fn run() -> i32 {
    let rest: Vec<String> = plat::args().iter().skip(1).cloned().collect();
    if rest.iter().any(|a| a == "--version" || a == "-V") {
        out(&bb_core::version_line("bb-worker"));
        return 0;
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") || rest.is_empty() {
        return usage();
    }
    let cli = match Cli::parse(&rest, VALUED, &[], false).and_then(|c| c.only(KNOWN).map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            bb_core::util::err(&format!("bb-worker: {}\n", e));
            return 2;
        }
    };
    plat::log_levels(plat::LOG_INFO, plat::LOG_INFO);
    let dir = config::data_dir(&cli);
    match cli.pos.first().map(String::as_str).unwrap_or("") {
        "join" => match join::join(&cli, &dir) {
            Ok(m) => {
                out(&m);
                0
            }
            Err(e) => fail(&e),
        },
        "run" if cli.flag("--service") => plat::service::run_service(SERVICE, || engine::run(&config::data_dir(&Cli::parse(&plat::args()[1..], VALUED, &[], false).unwrap_or_default()))),
        "run" => engine::run(&dir),
        "config" => config_cmd(&cli, &dir),
        "status" => status_cmd(&dir),
        "tools" => {
            // what a job on this machine can use, as this user (a service sees its own PATH)
            let t0 = tick_ms();
            let extra = config::load(&dir).map(|c| c.tools).unwrap_or_default();
            for t in bb_exec::tools::detect(&extra) {
                out(&format!("  {:<10} {}\n             {}\n", t.name, t.version, t.path));
            }
            out(&format!("(looked for {} programs in {})\n", bb_exec::tools::DEFAULT.len() + extra.len(), bb_core::util::dur_text(tick_ms() - t0)));
            0
        }
        "clean" => clean_cmd(&cli, &dir),
        "leave" => leave_cmd(&cli, &dir),
        "service" => service_cmd(&cli, &dir),
        _ => usage(),
    }
}

fn config_cmd(cli: &Cli, dir: &str) -> i32 {
    let Some(mut c) = config::load(dir) else { return fail(&not_paired(dir)) };
    match config::apply_options(&mut c, cli, dir) {
        Ok(true) => {
            if let Err(e) = config::save(dir, &c) {
                return fail(&e);
            }
            out(&format!("\"{}\" now lends:\n{}(a running worker takes this over within a few seconds)\n", c.name, config::offer_text(&c)));
            0
        }
        Ok(false) => {
            out(&format!("\"{}\" lends:\n{}", c.name, config::offer_text(&c)));
            0
        }
        Err(e) => fail(&e),
    }
}

fn project_lines(dir: &str) -> String {
    let list = cleanup::projects(dir);
    if list.is_empty() {
        return String::from("  no project has files here\n");
    }
    list.iter().map(|p| format!("  {:<24} {:>10}   used {} ago   {} slot(s)   {}\n", p.name, size_text(p.bytes), bb_core::util::ago_text(p.used), p.slots.len(), p.id)).collect()
}

fn status_cmd(dir: &str) -> i32 {
    let Some(c) = config::load(dir) else { return fail(&not_paired(dir)) };
    out(&format!("worker \"{}\" ({})\n  main machine {}\n  relay        {}\n  data folder  {}\n  service      {}\n{}projects here:\n{}", c.name, c.id, c.main_name, c.relay, dir, plat::service::state(SERVICE).text(), config::offer_text(&c), project_lines(dir)));
    0
}

/// bb-worker clean [PROJECT | --all]: directly when no worker runs on the data folder, else
/// through the worker that does (it knows which projects are in use)
fn clean_cmd(cli: &Cli, dir: &str) -> i32 {
    let all = cleanup::projects(dir);
    let picked: Vec<&cleanup::Project> = if cli.flag("--all") {
        all.iter().collect()
    } else {
        match cli.pos.get(1) {
            None => {
                out(&format!("projects with files on this machine:\n{}\nremove one with  bb-worker clean NAME  -- or all with  bb-worker clean --all\n", project_lines(dir)));
                return 0;
            }
            Some(name) => match all.iter().find(|p| p.id == *name || p.name.eq_ignore_ascii_case(name)) {
                Some(p) => alloc::vec![p],
                None => return fail(&format!("no project \"{}\" has files here (bb-worker clean lists them)", name)),
            },
        }
    };
    if picked.is_empty() {
        out("nothing to remove: no project has files here\n");
        return 0;
    }
    match plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")) {
        // nobody works here: remove at once
        Some(_lock) => {
            let mut freed = 0u64;
            for p in &picked {
                match cleanup::remove_project(dir, &p.id, "") {
                    Ok(b) => freed += b,
                    Err(e) => return fail(&e),
                }
            }
            out(&format!("{} project(s) removed, {} freed\n", picked.len(), size_text(freed)));
            0
        }
        // a worker runs: it removes them, and refuses those a job runs in
        None => {
            let files: Vec<String> = picked.iter().filter_map(|p| cleanup::request(dir, &p.id, "")).collect();
            let end = tick_ms() + 15_000;
            while tick_ms() < end && files.iter().any(|f| plat::file_exists(f) != 0) {
                plat::sleep_ms(200);
            }
            let left: Vec<&str> = picked.iter().filter(|p| cleanup::projects(dir).iter().any(|q| q.id == p.id)).map(|p| p.name.as_str()).collect();
            if left.is_empty() {
                out(&format!("{} project(s) removed by the running worker\n", picked.len()));
                0
            } else {
                fail(&format!("still here: {} (a job is running in it, or the worker has not answered yet)", left.join(", ")))
            }
        }
    }
}

fn leave_cmd(cli: &Cli, dir: &str) -> i32 {
    if plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")).is_none() {
        return fail("a worker runs on this data folder: stop it first (its service, its terminal, or the app)");
    }
    if cli.flag("--purge") {
        return if cleanup::purge(dir) {
            out("The pairing and every file of this worker are gone.  On the main machine, `bb unpair NAME` removes it from the relay.\n");
            0
        } else {
            fail(&format!("not everything in {} could be removed (a file is open in another program)", dir))
        };
    }
    if plat::delete(&plat::fs::join(dir, "worker.json")) {
        out("The pairing is forgotten (the projects' files stay; --purge removes them too).  On the main machine, `bb unpair NAME` removes this worker from the relay.\n");
        0
    } else {
        fail("this machine is not paired")
    }
}

fn service_cmd(cli: &Cli, dir: &str) -> i32 {
    let r = match cli.pos.get(1).map(String::as_str).unwrap_or("status") {
        "install" => (|| {
            config::load(dir).ok_or_else(|| not_paired(dir))?;
            let exe = plat::current_exe().ok_or("the program's own path is unknown")?;
            let args = alloc::vec![String::from("run"), String::from("--service"), String::from("--data"), String::from(dir)];
            let spec = plat::service::ServiceSpec { name: SERVICE, display: "Build Balancer Worker", description: "Build Balancer: runs build jobs of the paired main machine within the limits set with bb-worker config", exe: &exe, args: &args };
            plat::service::install(&spec).and_then(|_| plat::service::start(SERVICE))?;
            Ok(format!(
                "service {} installed and started (data: {})\nJobs now run as the service's user ({}): tools that live only in your own\nuser profile (rustup, nvm ...) are not on its PATH -- `bb workers` on the main machine shows what it finds.",
                SERVICE,
                dir,
                if cfg!(windows) { "LocalSystem" } else { "root" }
            ))
        })(),
        "uninstall" => plat::service::uninstall(SERVICE).map(|_| format!("service {} removed (pairing and workspaces stay)", SERVICE)),
        "start" => plat::service::start(SERVICE).map(|_| format!("service {} started", SERVICE)),
        "stop" => plat::service::stop(SERVICE).map(|_| format!("service {} stopped", SERVICE)),
        "status" => Ok(format!("service {}: {}", SERVICE, plat::service::state(SERVICE).text())),
        _ => return usage(),
    };
    match r {
        Ok(m) => {
            out(&format!("{}\n", m));
            0
        }
        Err(e) => fail(&e),
    }
}

rt::main!(run());
