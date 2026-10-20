//! The bank process from its start to its loop: signals, the link to the front (PCDS_IPC_FD), the
//! tables, the plugins prepared, HELLO, then START (the world from the save) or RESTORE (the
//! snapshot of the bank before it), READY with what the restore took, and the loop (serve.rs).
//! The PCDS_DEV=bank-fail=WHAT switches (mod.rs) are tests of the front's side of this.

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{info, plugins_glue, serve, start};
use crate::config;
use crate::ipc::{self, Link};

/// The page faults this process (all its threads) has had without reading a disk: a restore into
/// a new process touches every page of the world for the first time.
fn minor_faults() -> u64 {
    // SAFETY: getrusage fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_minflt as u64
}

fn fail_for_test(what: &str) -> bool {
    config::dev("bank-fail").is_some_and(|v| v == what)
}

pub fn run(args: &[String]) {
    let [dir, save_path] = args else {
        eprintln!("usage: pcds bank DATA_DIR SAVE.json (the front starts it)");
        std::process::exit(2);
    };
    // Ctrl-C and SIGTERM are the front's: it tells this bank to save and stop.
    // SAFETY: plain signal dispositions, set before any other thread exists.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let fd: i32 = crate::env("PCDS_IPC_FD").and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("pcds bank: no PCDS_IPC_FD (the front starts banks)");
        std::process::exit(2)
    });
    // SAFETY: the front put our end of the socket at this descriptor; nothing else owns it.
    let mut link = Link::new(unsafe { UnixStream::from_raw_fd(fd) });
    // The tables load before the hello: the front stops the old bank only once they are in.
    let (net_data, data, spec) = crate::tables(dir);
    let scenes = crate::scene_tables(dir, &net_data, &data);
    let build_id = std::fs::read_to_string(format!("{dir}/buildid")).ok().map(|b| b.trim().to_string());
    // The plugins are read and compiled now, with the tables, while the old bank still plays: after a
    // restore only their start is left (a hot update stops the world for that, not for this).
    let prepared = plugins_glue::prepare(save_path);
    let mut hello = info();
    hello["gameVersion"] = net_data.game_version.clone().into();
    let protocol = if fail_for_test("protocol") { 999 } else { ipc::PROTOCOL };
    if fail_for_test("slow-hello") {
        println!("bank: holding its hello for 4 s (PCDS_DEV=bank-fail=slow-hello)");
        std::thread::sleep(Duration::from_secs(4));
    }
    link.send(ipc::HELLO, protocol, hello.to_string().as_bytes());
    let (mut verified, mut verify_ms) = (Value::Null, 0.0);
    let mut started = Instant::now();
    let (mut got_at, mut faults_before) = (0, 0);
    let game = match link.recv() {
        Some(f) if f.kind == ipc::START => start::load(dir, save_path, net_data, data, spec, scenes),
        Some(f) if f.kind == ipc::RESTORE => {
            started = Instant::now();
            got_at = ipc::now_ns();
            faults_before = minor_faults();
            if fail_for_test("restore") {
                eprintln!("bank: dying on the restore (PCDS_DEV=bank-fail=restore)");
                std::process::exit(3);
            }
            let (restored, checked) = start::restore(&f, net_data, data, spec, scenes, save_path);
            (verified, verify_ms) = (checked.verified, checked.ms);
            restored
        }
        _ => return,
    };
    let mut game = game;
    plugins_glue::attach(&mut game, prepared);
    // What the start freed (the save's JSON, a snapshot's text, the tables' text) goes back to
    // the system, not to sit in the process.
    // SAFETY: malloc_trim only returns free memory to the system.
    unsafe { libc::malloc_trim(0) };
    // gotAt and readyAt (the monotonic clock, ns) put the restore on the flip's timeline in the front.
    let ready = json!({"restoreMs": started.elapsed().as_secs_f64() * 1000.0 - verify_ms, "verifyMs": verify_ms, "verified": verified, "gotAt": got_at, "readyAt": ipc::now_ns(), "faults": minor_faults() - faults_before});
    if fail_for_test("slow-ready") {
        println!("bank: holding its ready for 2 s (PCDS_DEV=bank-fail=slow-ready)");
        std::thread::sleep(Duration::from_secs(2));
    }
    link.send(ipc::READY, 0, ready.to_string().as_bytes());
    if fail_for_test("die-after-ready") {
        println!("bank: ending 1 s after its ready (PCDS_DEV=bank-fail=die-after-ready)");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(1));
            std::process::exit(3)
        });
    }
    serve::Bank { game, link, save_path: save_path.clone(), build_id, data_dir: dir.clone() }.serve();
}
