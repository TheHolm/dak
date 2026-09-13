//! Tests for `spawn_detached`: programs are started fully separated from this
//! process — their own process group and null stdio, with the child handle dropped
//! without waiting or killing — so they keep running on their own after the spawner.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dak::actions::{spawn_detached, CommandSpec};
use dak::log::Log;

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Parses the process group id (field 5) of `pid` from `/proc/<pid>/stat`.
///
/// The parenthesised comm in field 2 may contain spaces, so the stat line is split
/// at the *last* closing parenthesis; after that the whitespace-separated fields
/// restart at field 3 (state), making field 5 the third token.
fn read_pgrp(pid: i32) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_else(|error| panic!("failed reading stat for pid {pid}: {error}"));
    let rest = stat
        .rsplit_once(')')
        .expect("malformed stat line, no closing paren")
        .1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields[2].parse().unwrap_or_else(|error| {
        panic!("could not parse process group from stat of pid {pid}: {error}")
    })
}

/// A detached program starts running immediately, gets its own process group
/// distinct from this process's, and is not killed just because its handle was
/// dropped — it runs to completion on its own.
#[test]
fn spawn_detached_starts_program_in_its_own_process_group() {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid_file = format!("/tmp/dak_spawn_detached_{}_{n}.pid", std::process::id());
    let done_file = format!("/tmp/dak_spawn_detached_{}_{n}.done", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&done_file);

    // The program writes its pid straight away, then keeps running for a while and
    // finally marks its own completion. Checking the completion marker instead of
    // the pid's absence avoids pid-reuse noise from concurrent tests.
    let command = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!("echo $$ > {pid_file}; sleep 2; echo done > {done_file}"),
        ],
    };
    spawn_detached(&command, Log::default());

    let pid = {
        let mut pid = None;
        for _ in 0..200 {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(parsed) = content.trim().parse::<i32>() {
                    pid = Some(parsed);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        pid.expect("detached program did not write its pid file")
    };
    assert_ne!(
        pid,
        std::process::id() as i32,
        "spawned child, not ourselves"
    );

    assert!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "program {pid} should be running after the handle was dropped"
    );

    // Detachment means the child leads its own process group (`process_group(0)`),
    // which must differ from this process's group.
    let child_pgrp = read_pgrp(pid);
    let own_pgrp = read_pgrp(std::process::id() as i32);
    assert_eq!(child_pgrp, pid, "child should lead its own process group");
    assert_ne!(
        child_pgrp, own_pgrp,
        "child must not share our process group"
    );

    // The program finishes on its own: dropping the handle neither killed it nor
    // blocked on it — the completion marker still gets written.
    let mut completed = false;
    for _ in 0..500 {
        if Path::new(&done_file).exists() {
            completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        completed,
        "detached program {pid} did not run to completion"
    );

    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&done_file);
}
