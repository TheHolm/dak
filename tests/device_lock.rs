//! The per-device lock (`src/lock.rs`) between separate lock holders: in-process ones
//! (two opens of the same lock file conflict just like two processes do, since `flock`
//! locks belong to the open file) and a real second process - this test binary re-run
//! as a helper (see [`lock_holder_helper`]) - for `--replace`.
//!
//! Every test uses its own lock directory, so none of them touches the real one and
//! they can run in parallel.

mod common;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use dak::control::StopSource;
use dak::lock::{self, Conflict, DeviceKey, Holder, LockError};

/// The device key every test locks.
fn key() -> DeviceKey {
    DeviceKey::new(0x0300, 0x3002, Some("TESTSERIAL"), "/dev/hidraw9")
}

/// A fresh lock directory for one test.
fn lock_dir() -> PathBuf {
    common::temp_dir()
}

/// Takes the lock for [`key`] in `dir`, retrying for a moment while it looks held: a
/// child that another test is forking right now briefly holds a copy of every fd until
/// its exec closes the close-on-exec ones (see NOTES.md section 10), so a just-released
/// lock can look taken for an instant.
fn relock(dir: &Path) -> Result<lock::DeviceLock, LockError> {
    let mut result = lock::try_lock(dir, &key());
    for _ in 0..200 {
        if !matches!(result, Err(LockError::Busy(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        result = lock::try_lock(dir, &key());
    }
    result
}

/// A second lock attempt fails as busy and names this process as the holder; once the
/// first lock is dropped the lock can be taken again.
#[test]
fn second_lock_is_busy_until_released() {
    let dir = lock_dir();
    let first = lock::try_lock(&dir, &key()).unwrap();
    assert_eq!(first.path(), dir.join(key().file_name()));
    match lock::try_lock(&dir, &key()) {
        Err(LockError::Busy(Some(holder))) => {
            assert_eq!(holder.pid, std::process::id() as i32);
            assert_eq!(
                holder,
                Holder {
                    since: holder.since.clone(),
                    ..Holder::current()
                }
            );
        }
        other => panic!("expected busy with a holder, got {other:?}"),
    }
    assert_eq!(
        lock::holder_of(first.path()).unwrap().pid,
        std::process::id() as i32
    );
    drop(first);
    relock(&dir).expect("free again after the holder dropped it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Different devices have independent locks.
#[test]
fn different_devices_do_not_conflict() {
    let dir = lock_dir();
    let _one = lock::try_lock(&dir, &key()).unwrap();
    let other = DeviceKey::new(0x0300, 0x3002, Some("OTHER"), "");
    lock::try_lock(&dir, &other).expect("another serial is another lock");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new lock file is world-writable (despite the umask), so another user can record
/// themselves in it later; an existing file with stale content is reused and rewritten.
#[test]
fn lock_files_are_shared_and_reused() {
    let dir = lock_dir();
    let path = dir.join(key().file_name());
    drop(lock::try_lock(&dir, &key()).unwrap());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666);

    std::fs::write(
        &path,
        "garbage that is much longer than any real holder record ".repeat(10),
    )
    .unwrap();
    let _lock = relock(&dir).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("pid="), "{text}");
    assert!(
        !text.contains("garbage"),
        "the old content was truncated: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A symlink planted at the lock path is refused, not followed.
#[test]
fn symlinked_lock_file_is_refused() {
    let dir = lock_dir();
    let target = dir.join("victim");
    std::fs::write(&target, "precious").unwrap();
    std::os::unix::fs::symlink(&target, dir.join(key().file_name())).unwrap();
    assert!(matches!(
        lock::try_lock(&dir, &key()),
        Err(LockError::Io(_))
    ));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A FIFO planted at the lock path is refused without hanging the open.
#[test]
fn fifo_lock_file_is_refused() {
    let dir = lock_dir();
    let path = dir.join(key().file_name());
    let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o666) }, 0);
    assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
    match lock::try_lock(&dir, &key()) {
        Err(LockError::Io(error)) => assert!(error.contains("not a regular file"), "{error}"),
        other => panic!("expected an I/O error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing lock directory is an I/O error naming the path.
#[test]
fn missing_lock_directory_is_an_error() {
    let dir = Path::new("/nonexistent/dak-lock-dir");
    match lock::try_lock(dir, &key()) {
        Err(LockError::Io(error)) => assert!(error.contains("/nonexistent/dak-lock-dir")),
        other => panic!("expected an I/O error, got {other:?}"),
    }
}

/// `DAK_LOCK_DIR` overrides the lock directory (read only: the environment is never
/// modified here, since tests run in parallel).
#[test]
fn lock_dir_honours_the_environment() {
    match std::env::var_os(lock::LOCK_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(dir) => assert_eq!(lock::lock_dir(), PathBuf::from(dir)),
        None => {
            let dir = lock::lock_dir();
            assert!(
                dir == Path::new("/run/lock") || dir == Path::new("/tmp"),
                "{dir:?}"
            );
        }
    }
}

/// The default policy fails at once with the holder.
#[tokio::test]
async fn refuse_fails_at_once() {
    let dir = lock_dir();
    let _held = lock::try_lock(&dir, &key()).unwrap();
    let stop = StopSource::new();
    let result = lock::acquire(&dir, &key(), Conflict::Refuse, &stop.signal(), |_| {
        panic!("refusing never waits")
    })
    .await;
    assert!(
        matches!(result, Err(LockError::Busy(Some(_)))),
        "{result:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--wait` takes the lock once the holder lets go, reporting the wait once.
#[tokio::test]
async fn wait_takes_the_lock_after_release() {
    let dir = lock_dir();
    let held = lock::try_lock(&dir, &key()).unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(held);
    });
    let stop = StopSource::new();
    let mut waited = false;
    let lock = tokio::time::timeout(
        Duration::from_secs(5),
        lock::acquire(&dir, &key(), Conflict::Wait, &stop.signal(), |holder| {
            assert!(holder.is_some());
            waited = true;
        }),
    )
    .await
    .expect("the wait ends once the lock is free");
    assert!(lock.is_ok(), "{lock:?}");
    assert!(waited);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop signal ends `--wait`.
#[tokio::test]
async fn wait_is_cancelled_by_stop() {
    let dir = lock_dir();
    let _held = lock::try_lock(&dir, &key()).unwrap();
    let stop = StopSource::new();
    let signal = stop.signal();
    let key = key();
    let waiter = lock::acquire(&dir, &key, Conflict::Wait, &signal, |_| {});
    let stopper = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.stop();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(waiter, stopper)
    })
    .await
    .expect("stop ends the wait");
    assert_eq!(result.unwrap_err(), LockError::Cancelled);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--replace` refuses to signal another user's instance unless running as root (as
/// root it is allowed, so the case is only checked for ordinary users; the rule itself
/// is unit-tested in `src/lock.rs`).
#[tokio::test]
async fn replace_refuses_another_users_instance() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = lock_dir();
    let held = lock::try_lock(&dir, &key()).unwrap();
    let foreign = Holder {
        pid: std::process::id() as i32,
        uid: unsafe { libc::geteuid() } + 1,
        user: "someone-else".into(),
        since: "t".into(),
    };
    std::fs::write(held.path(), foreign.to_text()).unwrap();
    let stop = StopSource::new();
    let result = lock::acquire(&dir, &key(), Conflict::Replace, &stop.signal(), |_| {}).await;
    assert_eq!(result.unwrap_err(), LockError::NotPermitted(foreign));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Environment variable telling [`lock_holder_helper`] which directory to lock in.
const HELPER_DIR: &str = "DAK_TEST_LOCK_HELPER_DIR";
/// When set, the helper ignores `SIGTERM` (a holder that will not let go).
const HELPER_IGNORE_TERM: &str = "DAK_TEST_LOCK_HELPER_IGNORE_TERM";

/// Not a test of its own: when this test binary is re-run with [`HELPER_DIR`] set, this
/// becomes a second process that takes the lock and holds it until killed. Without the
/// variable (a normal `--ignored` run) it does nothing.
#[test]
#[ignore = "helper process for the --replace tests"]
fn lock_holder_helper() {
    let Some(dir) = std::env::var_os(HELPER_DIR) else {
        return;
    };
    if std::env::var_os(HELPER_IGNORE_TERM).is_some() {
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let _lock = lock::try_lock(Path::new(&dir), &key()).expect("the helper takes the lock");
    std::thread::sleep(Duration::from_secs(30));
}

/// Starts the helper holding the lock in `dir` and waits until its record is written.
fn start_holder(dir: &Path, ignore_term: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "lock_holder_helper",
            "--test-threads=1",
        ])
        .env(HELPER_DIR, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if ignore_term {
        command.env(HELPER_IGNORE_TERM, "1");
    }
    let mut child = command.spawn().unwrap();
    let path = dir.join(key().file_name());
    for _ in 0..500 {
        if lock::holder_of(&path).is_some_and(|holder| holder.pid == child.id() as i32) {
            return child;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the helper did not take the lock");
}

/// `--replace` stops the holding process with `SIGTERM` and takes its lock.
#[tokio::test]
async fn replace_stops_the_holder_and_takes_over() {
    let dir = lock_dir();
    let mut child = start_holder(&dir, false);
    let stop = StopSource::new();
    let lock = lock::acquire(&dir, &key(), Conflict::Replace, &stop.signal(), |holder| {
        assert_eq!(holder.unwrap().pid, child.id() as i32);
    })
    .await;
    assert!(lock.is_ok(), "{lock:?}");
    let status = child.wait().unwrap();
    assert!(!status.success(), "the holder was terminated: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A holder that ignores `SIGTERM` makes `--replace` time out, naming it.
#[tokio::test]
async fn replace_times_out_on_a_stubborn_holder() {
    let dir = lock_dir();
    let mut child = start_holder(&dir, true);
    let stop = StopSource::new();
    let result = lock::acquire_with_timeout(
        &dir,
        &key(),
        Conflict::Replace,
        Duration::from_millis(800),
        &stop.signal(),
        |_| {},
    )
    .await;
    let _ = child.kill();
    let _ = child.wait();
    match result {
        Err(LockError::Timeout(Some(holder))) => assert_eq!(holder.pid, child.id() as i32),
        other => panic!("expected a timeout naming the holder, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A forged holder record naming a live process that is not a `dak` (here a `sleep` of
/// our own user, which `--replace` would otherwise be allowed to signal) is refused:
/// the process is not signalled.
#[tokio::test]
async fn replace_refuses_a_forged_holder_record() {
    let dir = lock_dir();
    let held = lock::try_lock(&dir, &key()).unwrap();
    let mut victim = Command::new("sleep").arg("30").spawn().unwrap();
    let forged = Holder {
        pid: victim.id() as i32,
        uid: unsafe { libc::geteuid() },
        user: "me".into(),
        since: "t".into(),
    };
    std::fs::write(held.path(), forged.to_text()).unwrap();
    let stop = StopSource::new();
    let result = lock::acquire(&dir, &key(), Conflict::Replace, &stop.signal(), |_| {
        panic!("a forged holder is not waited for")
    })
    .await;
    assert_eq!(result.unwrap_err(), LockError::NotVerified(forged));
    assert!(
        victim.try_wait().unwrap().is_none(),
        "the forged pid was signalled"
    );
    let _ = victim.kill();
    let _ = victim.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A holder record naming pid -1, 0 or 1 (every process, the process group, init) is
/// not a record at all, so `--replace` has nobody to signal and reports the device
/// busy - and this process is still alive to check it.
#[tokio::test]
async fn replace_ignores_records_with_impossible_pids() {
    let dir = lock_dir();
    let held = lock::try_lock(&dir, &key()).unwrap();
    for pid in ["-1", "0", "1"] {
        let text = format!("pid={pid}\nuid={}\nuser=x\nsince=t\n", unsafe {
            libc::geteuid()
        });
        std::fs::write(held.path(), text).unwrap();
        assert_eq!(lock::holder_of(held.path()), None, "pid {pid}");
        let stop = StopSource::new();
        let result = lock::acquire(&dir, &key(), Conflict::Replace, &stop.signal(), |_| {}).await;
        assert_eq!(result.unwrap_err(), LockError::Busy(None), "pid {pid}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hard link planted at the lock path is refused, and the file it links to is neither
/// truncated nor overwritten.
#[test]
fn hardlinked_lock_file_is_refused() {
    let dir = lock_dir();
    let target = dir.join("victim");
    std::fs::write(&target, "precious").unwrap();
    std::fs::hard_link(&target, dir.join(key().file_name())).unwrap();
    match lock::try_lock(&dir, &key()) {
        Err(LockError::Io(error)) => assert!(error.contains("hard links"), "{error}"),
        other => panic!("expected an I/O error, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A real holder passes the check `--replace` makes before signalling it, and a record
/// with the wrong uid for that process does not.
#[test]
fn holder_verification_checks_process_and_user() {
    let own = Holder::current();
    assert!(lock::holder_is_genuine(&own));
    let wrong_uid = Holder {
        uid: own.uid.wrapping_add(1),
        ..own.clone()
    };
    assert!(!lock::holder_is_genuine(&wrong_uid));
    let gone = Holder {
        pid: i32::MAX,
        ..own
    };
    assert!(!lock::holder_is_genuine(&gone));
}
