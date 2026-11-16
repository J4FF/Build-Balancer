// bb-relay -- the Build Balancer relay: the one machine everything else can reach
//
//   bb-relay [run] [--listen ADDR:PORT] [--public HOST[:PORT]] [--data DIR]
//                  [--store-limit SIZE] [--keep-days N] [--threads N] [--plain]
//   bb-relay setup-key [--data DIR]        a new setup key (for a new or another main machine)
//   bb-relay status [--data DIR]           what the data folder holds
//   bb-relay service install [the options of run] | uninstall | start | stop | status
//   bb-relay --version | --help
//
// Copy the program to a server and start it: it makes its data folder, its certificate and a
// setup key, and prints the two commands for the main machine and for the workers.  It only
// passes data on: messages and blobs are sealed between the machines, the relay has no key
// (docs/ARCHITECTURE.md).  Devices reach it over TLS 1.3 with its own certificate, which they pin
// by its fingerprint; --plain serves http for a relay on the same machine or a trusted network.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

mod api;
mod store;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bb_core::util::{out, size_text, Cli};
use crypto::ed25519::Keypair;
use netio::server::Wrap;
use netio::tls::{self, Identity};
use netio::x509::{Cert, CertBuilder};
use netio::{Server, ServerConfig, Stream};
use plat::net::{IpAddr, SocketAddr, TcpStream};

use crate::store::Store;

const SERVICE: &str = if cfg!(windows) {
    "BuildBalancerRelay"
} else if cfg!(target_os = "macos") {
    "org.buildbalancer.relay"
} else {
    "buildbalancer-relay"
};
const VALUED: &[&str] = &["--listen", "--public", "--data", "--store-limit", "--keep-days", "--threads"];

fn fail(s: &str) -> i32 {
    plat::log_error("relay", s);
    1
}

fn usage() -> i32 {
    out("bb-relay [run] [--listen ADDR:PORT] [--public HOST[:PORT]] [--data DIR]\n\
         \x20              [--store-limit SIZE] [--keep-days N] [--threads N] [--plain]\n\
         bb-relay setup-key [--data DIR]     a new setup key for a main machine\n\
         bb-relay status [--data DIR]        devices and storage of the data folder\n\
         bb-relay service install [options of run] | uninstall | start | stop | status\n\
         bb-relay --version\n\
         \n\
         defaults: --listen 0.0.0.0:7700, --store-limit 20G, --keep-days 7, TLS with the relay's\n\
         own certificate (--plain: http).  Start it once and it prints what to do next.\n");
    2
}

fn default_dir() -> String {
    // run by root (a service): a place of the system's, not root's home
    if cfg!(target_os = "macos") && plat::process::elevated() {
        return String::from("/Library/Application Support/BuildBalancer/relay");
    }
    if !cfg!(windows) && plat::process::elevated() {
        return String::from("/var/lib/buildbalancer/relay");
    }
    let base = plat::data_dir().unwrap_or_else(|| String::from("."));
    bb_core::util::below(&base, "buildbalancer/relay")
}

fn data_dir(cli: &Cli) -> String {
    bb_core::util::abs_path(&cli.value("--data").map(String::from).unwrap_or_else(default_dir))
}

/// The relay's certificate: one self-signed Ed25519 certificate, made at the first start.
/// Devices pin its SHA-256, so it needs no authority and no name that fits.
fn identity(dir: &str) -> Result<(Identity, [u8; 32]), String> {
    let (cp, kp) = (plat::fs::join(dir, "tls.pem"), plat::fs::join(dir, "tls.key"));
    let read = || -> Option<(Vec<u8>, Keypair)> {
        let der = netio::pem::decode(&String::from_utf8_lossy(&plat::read_file(&cp)?), "CERTIFICATE")?;
        let key = Keypair::from_seed(&bb_core::crypt::key32(&String::from_utf8_lossy(&plat::read_file(&kp)?))?);
        (Cert::parse(&der)?.public_key == key.public).then_some((der, key))
    };
    let (der, key) = match read() {
        Some(x) => x,
        None if plat::file_exists(&cp) != 0 || plat::file_exists(&kp) != 0 => return Err(format!("{} and tls.key do not fit together; remove both for a new certificate (every device then has to pair again)", cp)),
        None => {
            let key = Keypair::from_seed(&plat::random_bytes());
            let t = bb_core::util::now();
            let der = CertBuilder {
                serial: plat::random_bytes(),
                cn: format!("Build Balancer relay {}", plat::net::hostname()),
                org: String::from("Build Balancer"),
                not_before: t - 86_400,
                not_after: t + 30 * 365 * 86_400,
                public_key: key.public,
                dns: alloc::vec![String::from("localhost"), plat::net::hostname()],
                ips: alloc::vec![IpAddr::LOCALHOST_V4, IpAddr::LOCALHOST_V6],
                server_auth: true,
                ..CertBuilder::default()
            }
            .sign(None, &key);
            if !plat::fs::write_file_private(&cp, netio::pem::encode("CERTIFICATE", &der).as_bytes()) || !plat::fs::write_file_private(&kp, crypto::hex(key.seed()).as_bytes()) {
                return Err(format!("{} cannot be written", cp));
            }
            (der, key)
        }
    };
    let fp = crypto::sha256(&der);
    Ok((Identity { chain: alloc::vec![der], key }, fp))
}

/// a new setup key: its hash into the data folder, the key itself to the caller
fn new_setup_key(dir: &str, fp: Option<&[u8; 32]>) -> Result<String, String> {
    let token: [u8; 16] = plat::random_bytes();
    let hash = bb_core::crypt::token_hash(&bb_core::crypt::setup_token_text(&token));
    if !plat::fs::write_file_private(&plat::fs::join(dir, "setup.hash"), hash.as_bytes()) {
        return Err(format!("{} cannot be written", dir));
    }
    Ok(bb_core::crypt::setup_key_encode(&token, fp))
}

/// the address the other machines are told: --public, the listen address, or this machine's
/// first address that is not loopback
fn public_addr(cli: &Cli, listen: &SocketAddr) -> (String, bool) {
    if let Some(p) = cli.value("--public") {
        let p = p.trim_start_matches("https://").trim_start_matches("http://");
        let with_port = if p.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) { String::from(p) } else { plat::net::host_port(p, listen.port) };
        return (with_port, true);
    }
    if !listen.ip.is_unspecified() {
        return (plat::net::host_port(&format!("{}", listen.ip), listen.port), true);
    }
    let own = plat::net::local_addrs().into_iter().map(|a| a.ip).find(|ip| matches!(ip, IpAddr::V4(b) if b[0] != 127 && !(b[0] == 169 && b[1] == 254)));
    match own {
        Some(ip) => (plat::net::host_port(&format!("{}", ip), listen.port), false),
        None => (plat::net::host_port(&plat::net::hostname(), listen.port), false),
    }
}

fn run() -> i32 {
    let args = plat::args();
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    if rest.iter().any(|a| a == "--version" || a == "-V") {
        out(&bb_core::version_line("bb-relay"));
        return 0;
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        return usage();
    }
    let cli = match Cli::parse(&rest, VALUED, &[], false) {
        Ok(c) => c,
        Err(e) => {
            bb_core::util::err(&format!("bb-relay: {}\n", e));
            return 2;
        }
    };
    if let Err(e) = cli.only(&["--listen", "--public", "--data", "--store-limit", "--keep-days", "--threads", "--plain", "--service"]) {
        bb_core::util::err(&format!("bb-relay: {}\n", e));
        return 2;
    }
    plat::log_levels(plat::LOG_INFO, plat::LOG_INFO);
    match cli.pos.first().map(String::as_str).unwrap_or("run") {
        "run" if cli.flag("--service") => plat::service::run_service(SERVICE, || serve_from_args()),
        "run" => serve(&cli),
        "setup-key" => setup_key_cmd(&cli),
        "status" => status(&cli),
        "service" => service_cmd(&cli),
        _ => usage(),
    }
}

/// the service's entry: run_service takes a plain function
fn serve_from_args() -> i32 {
    let rest: Vec<String> = plat::args().iter().skip(1).cloned().collect();
    match Cli::parse(&rest, VALUED, &[], false) {
        Ok(c) => serve(&c),
        Err(e) => fail(&e),
    }
}

fn serve(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    let plain = cli.flag("--plain");
    let limit = match cli.value("--store-limit").map(bb_core::util::size_parse) {
        Some(None) => return fail("--store-limit takes a size like 20G"),
        Some(Some(v)) => v.max(64 << 20),
        None => 20 << 30,
    };
    let keep = cli.value("--keep-days").and_then(|v| v.parse::<i64>().ok()).unwrap_or(7).clamp(1, 3650);
    let Some(listen) = SocketAddr::parse(cli.value("--listen").unwrap_or("0.0.0.0:7700")) else { return fail("--listen takes ADDRESS:PORT, like 0.0.0.0:7700") };
    let store = match Store::open(&dir, limit, keep) {
        Ok(s) => Arc::new(s),
        Err(e) => return fail(&e),
    };
    plat::log_file(Some(&plat::fs::join(&plat::fs::join(&dir, "logs"), "relay.log")), 10 << 20, 3);
    let (wrap, fp): (Option<Wrap>, Option<[u8; 32]>) = if plain {
        (None, None)
    } else {
        let (id, fp) = match identity(&dir) {
            Ok(x) => x,
            Err(e) => return fail(&e),
        };
        let cfg = tls::ServerConfig { identity: id, client_auth: None, client_auth_optional: false };
        let w: Wrap = Arc::new(move |tcp: TcpStream| {
            let s = tls::accept(Box::new(tcp), cfg.clone()).ok()?;
            Some((Box::new(s) as Box<dyn Stream>, None))
        });
        (Some(w), Some(fp))
    };
    let (claimed, relay_id, devices) = {
        let st = store.st.lock();
        (st.file.devices.iter().any(|d| d.role == bb_core::proto::ROLE_MAIN), st.file.relay_id.clone(), st.file.devices.len())
    };
    // a relay nobody took yet shows a setup key at every start: the old one is replaced
    let setup = if claimed {
        None
    } else {
        match new_setup_key(&dir, fp.as_ref()) {
            Ok(k) => Some(k),
            Err(e) => return fail(&e),
        }
    };
    let threads = cli.value("--threads").and_then(|v| v.parse::<usize>().ok()).unwrap_or(64).clamp(8, 512);
    // a connection that only waits gives its thread back after a while; devices reconnect by themselves
    let cfg = ServerConfig { threads, max_body: 40 << 20, idle_ms: 20_000, read_ms: 30_000, write_ms: 60_000, max_requests: 100_000 };
    let s2 = store.clone();
    let server = match Server::start_with(&listen, cfg, Arc::new(move |req| api::handle(&s2, req)), wrap) {
        Ok(s) => s,
        Err(e) => return fail(&format!("{} cannot be opened: {}", listen, e)),
    };
    let (addr, sure) = public_addr(cli, &server.local_addr());
    let scheme = if plain { "http://" } else { "" };
    let mut b = format!("\n{} relay {}  (id {})\n", bb_core::PRODUCT, bb_core::VERSION, relay_id);
    b += &format!("  data folder   {}\n  listening on  {} ({})\n", dir, server.local_addr(), if plain { "http, no TLS" } else { "TLS 1.3" });
    if let Some(f) = &fp {
        b += &format!("  certificate   SHA-256 {}\n", bb_core::crypt::fingerprint_text(f));
    }
    b += &format!("  blob store    up to {}, channels kept {} days\n\n", size_text(limit), keep);
    match &setup {
        Some(key) => {
            b += "This relay has no main machine yet.\n\n  1. On your MAIN machine (the one you start builds from):\n\n";
            b += &format!("       bb setup {}{} {}\n\n", scheme, addr, key);
            b += "  2. For every machine that should lend its power, run `bb pair` on the main machine.\n     It prints the command to run on that machine, with a one-time code:\n\n";
            b += &format!("       bb-worker join {}{} CODE\n\n", scheme, addr);
        }
        None => {
            b += &format!("{} device(s) admitted.  To add a worker, run `bb pair` on the main machine and then on the worker:\n\n", devices);
            b += &format!("       bb-worker join {}{} CODE\n\n", scheme, addr);
            b += "A new or second main machine needs a setup key: `bb-relay setup-key`.\n\n";
        }
    }
    if !sure {
        b += &format!("({} is this machine's address as it sees itself.  Behind a router or with a DNS name,\n use the address the other machines reach it by -- or start with --public HOST.)\n\n", addr);
    }
    if plain && !listen.ip.is_loopback() {
        b += "(--plain: access tokens travel unencrypted.  Jobs, sources and results stay sealed\n end to end, but use this only inside a network you trust.)\n\n";
    }
    b += "Keep it running: `bb-relay service install` makes it a system service.\n\n";
    out(&b);
    plat::log_info("relay", &format!("started on {}", server.local_addr()));
    let mut ticks = 0u32;
    while !plat::service::stop_requested() {
        plat::sleep_ms(500);
        ticks += 1;
        if ticks % 120 == 0 {
            store.sweep(&mut store.st.lock());
        }
    }
    plat::log_info("relay", "stopping");
    store.changed.notify_all();
    server.stop();
    0
}

fn setup_key_cmd(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    if plat::file_exists(&plat::fs::join(&dir, "relay.json")) != 1 {
        return fail(&format!("{} holds no relay yet: start bb-relay once", dir));
    }
    // a relay with TLS has its certificate by now; without one it runs with --plain
    let fp = plat::read_file(&plat::fs::join(&dir, "tls.pem")).and_then(|b| netio::pem::decode(&String::from_utf8_lossy(&b), "CERTIFICATE")).map(|der| crypto::sha256(&der));
    match new_setup_key(&dir, fp.as_ref()) {
        Ok(k) => {
            out(&format!("A new setup key (valid once; an older unused one is void now):\n\n    bb setup <relay address> {}\n\n", k));
            0
        }
        Err(e) => fail(&e),
    }
}

fn status(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    let Some(f) = bb_core::j::load::<store::RelayFile>(&plat::fs::join(&dir, "relay.json")) else { return fail(&format!("{} holds no relay", dir)) };
    out(&format!("relay {} in {}\nservice {}: {}\n", f.relay_id, dir, SERVICE, plat::service::state(SERVICE).text()));
    for d in &f.devices {
        out(&format!("  {:<7} {:<24} {}\n", d.role, d.name, d.id));
    }
    let blobs = bb_core::util::tree_size(&plat::fs::join(&dir, "blobs"), u64::MAX);
    let ch = bb_core::util::tree_size(&plat::fs::join(&dir, "channels"), u64::MAX);
    out(&format!("blobs {}, channels {}\n", size_text(blobs), size_text(ch)));
    0
}

fn service_cmd(cli: &Cli) -> i32 {
    let r = match cli.pos.get(1).map(String::as_str).unwrap_or("status") {
        "install" => install_service(cli),
        "uninstall" => plat::service::uninstall(SERVICE).map(|_| format!("service {} removed (the data folder stays)", SERVICE)),
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

fn install_service(cli: &Cli) -> Result<String, String> {
    let exe = plat::current_exe().ok_or("the program's own path is unknown")?;
    let dir = data_dir(cli);
    let mut args = alloc::vec![String::from("run"), String::from("--service"), String::from("--data"), dir.clone()];
    for (n, v) in &cli.opts {
        if n != "--data" {
            args.push(n.clone());
            args.push(v.clone());
        }
    }
    if cli.flag("--plain") {
        args.push(String::from("--plain"));
    }
    let spec = plat::service::ServiceSpec { name: SERVICE, display: "Build Balancer Relay", description: "Build Balancer: passes sealed build data between the machines that paired through it", exe: &exe, args: &args };
    plat::service::install(&spec).and_then(|_| plat::service::start(SERVICE))?;
    Ok(format!("service {} installed and started (data: {})\nIts log: {}", SERVICE, dir, plat::fs::join(&plat::fs::join(&dir, "logs"), "relay.log")))
}

rt::main!(run());
