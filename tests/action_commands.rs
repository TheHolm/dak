//! Tests for `run_action_command`, which runs an `Action::Command` program to
//! completion without a timeout or output capture.

use std::time::Duration;

use dak::actions::{run_action_command, CommandSpec};

/// A command that exits successfully returns Ok.
#[tokio::test]
async fn run_action_command_runs_program() {
    let command = CommandSpec {
        program: "/bin/echo".to_string(),
        args: vec!["action".to_string(), "ran".to_string()],
    };
    run_action_command(command).await.expect("expected success");
}

/// A command exiting with a non-zero status is reported as an error mentioning the exit.
#[tokio::test]
async fn run_action_command_reports_nonzero_exit() {
    let command = CommandSpec {
        program: "false".to_string(),
        args: vec![],
    };
    let error = run_action_command(command)
        .await
        .expect_err("expected failure");
    assert!(error.contains("exited with"), "unexpected error: {error}");
}

/// A program that cannot be spawned (does not exist) is reported as a start failure.
#[tokio::test]
async fn run_action_command_reports_missing_program() {
    let command = CommandSpec {
        program: "/does/not/exist".to_string(),
        args: vec![],
    };
    let error = run_action_command(command)
        .await
        .expect_err("expected failure");
    assert!(
        error.contains("failed to start"),
        "unexpected error: {error}"
    );
}

/// Long-running commands are not cut short by a time limit: a program sleeping past a
/// timeout scale still completes successfully.
#[tokio::test]
async fn run_action_command_allows_slow_program() {
    let command = CommandSpec {
        program: "sleep".to_string(),
        args: vec!["1".to_string()],
    };
    let result = tokio::time::timeout(Duration::from_secs(5), run_action_command(command)).await;
    assert!(result.is_ok(), "expected the slow command to complete");
}
