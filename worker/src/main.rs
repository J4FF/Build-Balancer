// bb-worker -- lends this machine's power to a main machine (the command line; the Build
// Balancer app does the same with a window)
//
//   bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]
//                                           pair with a main machine and start lending right away
//                                           (--no-run: pair only)
//   bb-worker run                           lend, in this terminal
//   bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]
//                    [--keep-days N] [--tags a,b] [--tools x,y] [--name NAME]      (no option: show)
//   bb-worker status
//   bb-worker tools                         the build tools jobs can use on this machine
//   bb-worker clean [PROJECT | --all]       remove a project's files here (no argument: list them)
//   bb-worker leave [--purge]               forget the pairing (--purge: and every file kept here)
//   bb-worker update [--check]              fetch the newest release and put it in this program's
//                                           place; a worker that runs on the data folder starts
//                                           again by itself
//   bb-worker service install [--user NAME | --as-root]    lend whenever the machine runs.  Linux:
//                                           the service runs as you (the one who calls sudo), on
//                                           your pairing and with your PATH -- as root only on request
//   bb-worker service uninstall | start | stop | status
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
use bb_exec::svc;
use bb_worker::engine::not_paired;
use bb_worker::{cleanup, config, engine, join};

const SERVICE: &str = if cfg!(windows) {
    "BuildBalancerWorker"
} else if cfg!(target_os = "macos") {
    "org.buildbalancer.worker"
} else {
    "buildbalancer-worker"
};
const VALUED: &[&str] = &["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--keep-days", "--tags", "--tools", "--user"];
const KNOWN: &[&str] = &["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--keep-days", "--tags", "--tools", "--user", "--force", "--service", "--all", "--purge", "--no-run", "--as-root", "--check"];

fn fail(s: &str) -> i32 {
    plat::log_error("worker", s);
    1
}

fn usage() -> i32 {
    out("bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]\n\
         \x20                                  pair with a main machine and start lending (--no-run: pair only)\n\
         bb-worker run                      lend, in this terminal, until stopped\n\
         bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]\n\
         \x20                [--keep-days N] [--tags a,b] [--tools x,y] [--name NAME]     show or change what is lent\n\
         bb-worker status\n\
         bb-worker tools                    the build tools jobs can use on this machine\n\
         bb-worker clean [PROJECT | --all]  remove a project's files here (no argument: list them)\n\
         bb-worker leave [--purge]          forget the pairing (--purge: and every file kept here)\n\
         bb-worker update [--check]         fetch the newest release; a running worker starts again by itself\n\
         bb-worker service install [--user NAME | --as-root]   lend whenever the machine runs\n\
         bb-worker service uninstall | start | stop | status\n\
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
    // what an update left behind: the files it replaced
    bb_update::at_start();
    let dir = config::data_dir(&cli);
    match cli.pos.first().map(String::as_str).unwrap_or("") {
        "join" => match join::join(&cli, &dir) {
            Ok(c) => joined(&cli, &dir, &c),
            Err(e) => fail(&e),
        },
        // A service that is to start again ends with an error: its manager starts it anew.
        // Windows does that only for a service whose program ends without having said that
        // it stopped -- so there it ends right here, with nothing said.
        "run" if cli.flag("--service") => plat::service::run_service(SERVICE, || {
            let code = engine::run(&config::data_dir(&Cli::parse(&plat::args()[1..], VALUED, &[], false).unwrap_or_default()));
            if cfg!(windows) && code == engine::EXIT_RESTART {
                plat::exit(code);
            }
            code
        }),
        "run" => again(&dir, &rest),
        "update" => update_cmd(&cli, &dir),
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

/// After a pairing: say what was agreed, and start lending -- a machine that joined and then
/// does nothing looks broken from the main machine ("never seen online").
fn joined(cli: &Cli, dir: &str, c: &config::Config) -> i32 {
    out(&format!(
        "\nPaired with \"{}\" as worker \"{}\" ({}).\n\nThis machine lends:\n{}  change it any time:  bb-worker config --cpu 6 --ram 12G --disk 40G\n\n",
        c.main_name,
        c.name,
        c.id,
        config::offer_text(c)
    ));
    if !cfg!(windows) && plat::process::elevated() {
        out("Note: this pairing was made as root, so jobs of the main machine run as root here.  Pair without\nsudo (`bb-worker leave`, then `bb-worker join ...`) to run them as yourself.\n\n");
    }
    // by its full path: sudo has a PATH of its own, without ~/.local/bin
    let exe = plat::current_exe().unwrap_or_else(|| String::from("bb-worker"));
    let service = if cfg!(windows) { String::from("bb-worker service install   (in a terminal run as administrator)") } else { format!("sudo {} service install", exe) };
    if svc::installed(SERVICE) {
        out(&format!("The service {} is installed: restart it so that it works with this pairing\n(`bb-worker service stop`, then `bb-worker service start`).\n", SERVICE));
        return 0;
    }
    if cli.flag("--no-run") {
        out(&format!("Nothing runs yet.  Start lending:\n  bb-worker run                 in this terminal, or\n  {}\n", service));
        return 0;
    }
    out(&format!("This machine lends its power now, for as long as this terminal stays open (Ctrl+C ends it).\nTo lend whenever the machine runs:  {}\n\n", service));
    // started again after an update, it is a plain `run`: the pairing is done
    let mut args = alloc::vec![String::from("run")];
    if let Some(d) = cli.value("--data") {
        args.extend([String::from("--data"), String::from(d)]);
    }
    again(dir, &args)
}

/// A worker in a terminal: lend until the system asks the program to stop.  When it is to
/// start again with new programs, it starts them and ends -- once the new worker has taken
/// the data folder over.  Nobody watches a worker in a terminal: a new one that does not come
/// up (it could not be started, or ended at once) must not be the end of the lending.  Then
/// this one, old as it is, works on, and tries again later.
fn again(dir: &str, args: &[String]) -> i32 {
    loop {
        let code = engine::run(dir);
        if code != engine::EXIT_RESTART {
            return code;
        }
        if bb_update::restart(args) && taken_over(dir) {
            return 0;
        }
        // (said where it is seen: the log file is closed between two runs)
        bb_core::util::err("bb-worker: the new version did not start (or ended at once): this one works on and tries again in ten minutes\n");
        engine::defer_restarts(600_000);
    }
}

/// whether another worker took the data folder over (holds its lock) within a few seconds
fn taken_over(dir: &str) -> bool {
    let lock = plat::fs::join(dir, "worker.lock");
    let end = tick_ms() + 15_000;
    while tick_ms() < end {
        plat::sleep_ms(250);
        // (a look takes the lock for an instant; the new worker tries more than once -- engine::run)
        if plat::FileLock::acquire(&lock).is_none() {
            return true;
        }
    }
    false
}

/// bb-worker update [--check]
fn update_cmd(cli: &Cli, dir: &str) -> i32 {
    let c = match bb_update::check() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if cli.flag("--check") {
        out(&if c.newer { format!("Version {} is available (this is {}): bb-worker update\n", c.latest, c.current) } else { format!("bb-worker {} is the newest version.\n", c.current) });
        return 0;
    }
    if c.newer {
        let Some(own) = bb_update::own_dir() else { return fail("the program's own folder is unknown") };
        if let Err(e) = bb_update::install(&c.latest, &own, &mut |m| out(&format!("{} ...\n", m))) {
            return fail(&e);
        }
        out(&format!("Version {} is installed in {} (bb, bb-worker, bb-relay, BuildBalancer).\n", c.latest, own));
    } else {
        out(&format!("bb-worker {} is the newest version.\n", c.current));
    }
    // Whoever lends on this data folder right now -- `bb-worker run`, the service, the app --
    // holds its lock and may still run an older program: the files could have been replaced
    // by `bb update` or the app before.  It is asked to start again -- at once when this call
    // replaced the programs, else only if it is older than what is there.
    if config::load(dir).is_some() && plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")).is_none() {
        let word = if c.newer { String::from("now") } else { String::from(bb_core::VERSION) };
        if !bb_core::util::put_file(&engine::restart_request(dir), word.as_bytes()) {
            return fail("the worker that runs here could not be asked to start again: start it again yourself (the app, the service, or bb-worker run)");
        }
        out(if c.newer { "The worker that runs here starts again with the new version as soon as no job runs.\n" } else { "A worker that still runs an older version here starts again as soon as no job runs.\n" });
    } else if c.newer && svc::installed(SERVICE) && plat::service::state(SERVICE) == plat::service::ServiceState::Running {
        // The service lends on another data folder (another user's pairing).  It is not
        // stopped -- that would cut its jobs off --, and needs no word: a worker sees that its
        // program was replaced.
        out(&format!("The service {} sees the new programs by itself and starts again with them as soon as no job runs.\n", SERVICE));
    }
    0
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
    out(&format!("worker \"{}\" ({})\n  main machine {}\n  relay        {}\n  data folder  {}\n  service      {}\n{}projects here:\n{}", c.name, c.id, c.main_name, c.relay, dir, svc::state_text(SERVICE), config::offer_text(&c), project_lines(dir)));
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

/// Who the service runs as, on which data folder and with which PATH.  Linux: as the person
/// who installs it -- the one behind sudo, or --user NAME -- on that person's pairing, so jobs
/// have that person's rights and tools and not root's; as root only with --as-root.  Windows
/// and macOS: the system's service account (LocalSystem, root), as JFLib installs it.
fn service_place(cli: &Cli, dir: &str) -> Result<(String, Option<svc::User>, Option<String>), String> {
    if !cfg!(target_os = "linux") || cli.flag("--as-root") {
        config::load(dir).ok_or_else(|| not_paired(dir))?;
        return Ok((String::from(dir), None, None));
    }
    let user = match cli.value("--user") {
        Some(name) => svc::user_named(name).ok_or_else(|| format!("this machine has no user \"{}\"", name))?,
        None => match svc::sudo_user() {
            Some(u) => u,
            None if plat::process::elevated() => {
                return Err(String::from(
                    "as root the service would run every job of the main machine as root.  Name the user the jobs should run as:\n\n    bb-worker service install --user NAME\n\n(the pairing has to be that user's: `bb-worker join` run as NAME) -- or pass --as-root if root is what you want.",
                ));
            }
            None => return Err(String::from("this needs root rights: run it again with sudo (the service will run as you, not as root)")),
        },
    };
    let dir = cli.value("--data").map(bb_core::util::abs_path).unwrap_or_else(|| svc::user_data_dir(&user, "worker"));
    if config::load(&dir).is_none() {
        return Err(if config::load(config::system_dir()).is_some() {
            format!(
                "this machine was paired as root (the pairing lies in {}), so its jobs would run as root.\nPair it as {} instead -- `bb-worker join RELAY CODE` without sudo -- or pass --as-root.",
                config::system_dir(),
                user.name
            )
        } else {
            format!("{} has no pairing in {}: run `bb-worker join RELAY CODE` as {} first (without sudo)", user.name, dir, user.name)
        });
    }
    let path = svc::login_path(&user);
    Ok((dir, Some(user), Some(path)))
}

fn install_service(cli: &Cli, dir: &str) -> Result<String, String> {
    let exe = plat::current_exe().ok_or("the program's own path is unknown")?;
    let (dir, user, path) = service_place(cli, dir)?;
    let args = alloc::vec![String::from("run"), String::from("--service"), String::from("--data"), dir.clone()];
    let service = svc::Service {
        name: SERVICE,
        display: "Build Balancer Worker",
        description: "Build Balancer: runs build jobs of the paired main machine within the limits set with bb-worker config",
        exe: &exe,
        args: &args,
        user: user.as_ref(),
        path: path.as_deref(),
    };
    svc::install(&service).and_then(|_| svc::start(SERVICE))?;
    let who = match &user {
        Some(u) => format!(
            "Jobs run as {}, with the PATH of {}'s login shell (so rustup, pipx and the like are found):\n  {}\nTools installed later into ~/.cargo/bin or ~/.local/bin are found as well; for anything else,\ninstall the service again.",
            u.name,
            u.name,
            path.as_deref().unwrap_or("")
        ),
        None => format!(
            "Jobs run as the service's account ({}): tools that live only in your own user profile\n(rustup, nvm ...) are not on its PATH.",
            if cfg!(windows) { "LocalSystem" } else { "root" }
        ),
    };
    Ok(format!("service {} installed and started: this machine lends its power whenever it runs (data: {})\n{}\n`bb workers` on the main machine shows the machine and the tools it found.", SERVICE, dir, who))
}

fn service_cmd(cli: &Cli, dir: &str) -> i32 {
    let r = match cli.pos.get(1).map(String::as_str).unwrap_or("status") {
        "install" => install_service(cli, dir),
        "uninstall" => svc::uninstall(SERVICE).map(|_| format!("service {} removed (pairing and workspaces stay)", SERVICE)),
        "start" => svc::start(SERVICE).map(|_| format!("service {} started", SERVICE)),
        "stop" => svc::stop(SERVICE).map(|_| format!("service {} stopped", SERVICE)),
        "status" => Ok(format!("service {}: {}", SERVICE, svc::state_text(SERVICE))),
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
