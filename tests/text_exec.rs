//! Tests for the asynchronous `exec` machinery: running programs with a timeout,
//! reporting their raw stdout through a channel, and killing them when cancelled.

use std::time::Duration;

use dak::actions::{
    run_command_with_timeout, spawn_exec, CommandSpec, ExecEvent, ExecOutputKind,
    MAX_EXEC_OUTPUT_BYTES,
};

/// Whether a process with the given pid is still running, checked the portable POSIX
/// way (`kill -0`) rather than via `/proc`, which Linux mounts by default but FreeBSD
/// does not. Stdio is silenced: a gone pid isn't an error worth narrating here.
fn is_process_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A program that prints to stdout and exits immediately yields its full output.
#[tokio::test]
async fn run_command_with_timeout_captures_output() {
    let command = CommandSpec {
        program: "/bin/echo".to_string(),
        args: vec!["good".to_string(), "morning".to_string()],
    };
    let output = run_command_with_timeout(&command, Duration::from_secs(5))
        .await
        .expect("expected success");
    assert_eq!(output, b"good morning\n");
}

/// A program exiting with a non-zero status is reported as an error mentioning the exit.
#[tokio::test]
async fn run_command_with_timeout_reports_nonzero_exit() {
    let command = CommandSpec {
        program: "false".to_string(),
        args: vec![],
    };
    let error = run_command_with_timeout(&command, Duration::from_secs(5))
        .await
        .expect_err("expected failure");
    assert!(error.contains("exited with"), "unexpected error: {error}");
}

/// A program running longer than the timeout is killed and reported as such.
#[tokio::test]
async fn run_command_with_timeout_kills_slow_program() {
    let command = CommandSpec {
        program: "sleep".to_string(),
        args: vec!["10".to_string()],
    };
    let error = run_command_with_timeout(&command, Duration::from_millis(200))
        .await
        .expect_err("expected timeout");
    assert!(error.contains("killed"), "unexpected error: {error}");
}

/// A program producing more output than one OS pipe buffer's worth (but well under
/// `MAX_EXEC_OUTPUT_BYTES`) still completes successfully within the timeout: stdout is
/// drained concurrently with waiting for exit, so the program never blocks on its own
/// `write()` waiting for a reader that only shows up after it exits.
#[tokio::test]
async fn run_command_with_timeout_completes_with_output_past_one_pipe_buffer() {
    // 256 KiB comfortably exceeds a typical 64 KiB Linux pipe buffer, while staying
    // far under MAX_EXEC_OUTPUT_BYTES (10 MiB).
    let size = 256 * 1024;
    let command = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), format!("head -c {size} /dev/zero")],
    };
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        run_command_with_timeout(&command, Duration::from_secs(5)),
    )
    .await
    .expect("should not need the outer test timeout")
    .expect("expected success");
    assert_eq!(output.len(), size);
}

/// A program producing more output than `MAX_EXEC_OUTPUT_BYTES` is killed as soon as
/// the cap is exceeded and reported with a distinct error, rather than being silently
/// truncated or left to run until the (much longer) timeout elapses.
#[tokio::test]
async fn run_command_with_timeout_reports_output_past_the_cap() {
    let size = MAX_EXEC_OUTPUT_BYTES + 1024;
    let command = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), format!("head -c {size} /dev/zero")],
    };
    // The timeout given here is far longer than this should ever take: exceeding the
    // cap must be detected and the program killed well before it, not merely before
    // this outer bound.
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        run_command_with_timeout(&command, Duration::from_secs(30)),
    )
    .await
    .expect("output past the cap should be detected quickly, not after the full timeout")
    .expect_err("expected the output-too-large error");
    assert!(
        error.contains("output exceeded"),
        "unexpected error: {error}"
    );
}

/// A program that cannot be spawned (does not exist) is reported as a start failure.
#[tokio::test]
async fn run_command_with_timeout_reports_missing_program() {
    let command = CommandSpec {
        program: "/does/not/exist".to_string(),
        args: vec![],
    };
    let error = run_command_with_timeout(&command, Duration::from_secs(5))
        .await
        .expect_err("expected failure");
    assert!(
        error.contains("failed to start"),
        "unexpected error: {error}"
    );
}

/// Non-UTF-8 output is preserved verbatim; deciding how to render it is the caller's job.
#[tokio::test]
async fn run_command_with_timeout_preserves_raw_bytes() {
    let command = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "printf '\\377\\376'".to_string()],
    };
    let output = run_command_with_timeout(&command, Duration::from_secs(5))
        .await
        .expect("expected success");
    assert_eq!(output, b"\xff\xfe");
}

/// Spawning an `exec` task delivers its raw stdout as an Output event on success.
#[tokio::test]
async fn spawn_exec_reports_output() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let handle = spawn_exec(
        tx,
        3,
        ExecOutputKind::Text,
        CommandSpec {
            program: "/bin/echo".to_string(),
            args: vec!["sync".to_string()],
        },
        7,
    );
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the task")
        .expect("channel closed");
    assert_eq!(
        event,
        ExecEvent::Output {
            key: 3,
            generation: 7,
            kind: ExecOutputKind::Text,
            stdout: b"sync\n".to_vec(),
        }
    );
    handle.await.expect("task should not panic");
}

/// Spawning an `exec` task delivers an Error event when the program fails.
#[tokio::test]
async fn spawn_exec_reports_failure() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let handle = spawn_exec(
        tx,
        5,
        ExecOutputKind::Image,
        CommandSpec {
            program: "false".to_string(),
            args: vec![],
        },
        9,
    );
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for the task")
        .expect("channel closed");
    match event {
        ExecEvent::Error {
            key,
            generation,
            error,
        } => {
            assert_eq!((key, generation), (5, 9));
            assert!(error.contains("exited with"), "unexpected error: {error}");
        }
        other => panic!("expected an Error event, got {other:?}"),
    }
    handle.await.expect("task should not panic");
}

/// Aborting a spawned `exec` task kills the process it started.
#[tokio::test]
async fn aborting_spawned_task_kills_the_process() {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let pid_file = format!("/tmp/dak_text_exec_pid_{}", std::process::id());
    let _ = std::fs::remove_file(&pid_file);
    let command = CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!("echo $$ > {pid_file}; exec sleep 60"),
        ],
    };
    let handle = spawn_exec(tx, 2, ExecOutputKind::Text, command, 1);

    // Wait until the program has written its own pid, then abort the task.
    let pid = {
        let mut pid = None;
        for _ in 0..200 {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if let Ok(parsed) = content.trim().parse::<i32>() {
                    pid = Some(parsed);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pid.expect("program did not write its pid file")
    };
    assert!(
        is_process_alive(pid),
        "sanity check: process {pid} should be running"
    );

    handle.abort();
    let _ = handle.await;

    let mut gone = false;
    for _ in 0..100 {
        if !is_process_alive(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "process {pid} still alive after abort");
    let _ = std::fs::remove_file(&pid_file);
}
