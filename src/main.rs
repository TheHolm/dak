use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// See src/lib.rs for why this is needed on FreeBSD only: the binary crate compiles
// separately from the library crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;
// Only the tests build fake device descriptions, which need async-hid's `DeviceId`.
#[cfg(all(test, target_os = "freebsd"))]
extern crate async_hid_freebsd as async_hid;

use clap::Parser;
use mirajazz::{
    device::{list_devices, Device},
    error::MirajazzError,
    types::{DeviceInput, HidDevice, HidDeviceInfo},
};
use serde_json::Value;
use tokio::sync::mpsc;

use dak::actions::{self, Action, ButtonDevice};
use dak::baseplane::Reference;
use dak::cli::Cli;
use dak::color::Color;
use dak::control::{self, Controller, StopSignal, StopSource};
use dak::daemon;
use dak::exit;
use dak::hardware;
use dak::lock::{self, Conflict, DeviceKey, DeviceLock, LockError};
use dak::log::{CliLogging, Environment, Log, LogSettings, LoggingConfig, Subsystem};
use dak::map::{ControlEvent, MapError, Mapping, TwistDirection};
use dak::press::{ClickDetector, ClickEvent, Defaults, PressDecision, ReleaseDecision};
use dak::reconnect::{self, SwappableDevice};
use dak::variables::{VarValue, Variables};

/// Parses the command line, loads the config and drives every configured device until
/// the program is told to stop, returning one of the [`dak::exit`] statuses.
///
/// The config is loaded (and validated) before the async runtime even starts, so a
/// configuration error is reported, with its own exit status, before anything touches
/// a device or forks into the background.
fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // Until the config's `logging` section is read, lines go to the console (or the
    // journal under systemd), filtered by the command-line level and `-d` alone.
    let bootstrap = match LogSettings::resolve(
        &LoggingConfig::default(),
        &CliLogging {
            file: None,
            syslog: false,
            ..CliLogging::from_cli(&cli)
        },
        &Environment::probe(false),
    ) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("error: {error}");
            return std::process::ExitCode::from(exit::FAILURE);
        }
    };
    if let Ok(sinks) = bootstrap.open() {
        dak::log::install(sinks);
    }
    let log = bootstrap.log;
    log.info(format!(
        "DAK (Dynamic Ajazz Keyboard) v{}",
        env!("CARGO_PKG_VERSION")
    ));
    std::process::ExitCode::from(run(cli, log))
}

/// The whole program after argument parsing; returns the exit status.
fn run(cli: Cli, log: Log) -> u8 {
    // The mapping wizard runs standalone: it must not read the config nor
    // execute any actions, and it exits on its own when done.
    if cli.map {
        return match build_runtime(log) {
            Some(runtime) => match runtime.block_on(dak::map::run_map_wizard(log)) {
                Ok(()) => exit::SUCCESS,
                Err(MapError::Device(MirajazzError::DeviceNotFoundError)) => exit::NO_DEVICE,
                Err(MapError::Busy) => exit::DEVICE_BUSY,
                Err(error) => {
                    log.error(error);
                    exit::FAILURE
                }
            },
            None => exit::FAILURE,
        };
    }

    let config_path = absolute_config_path(&actions::resolve_config_path(cli.config.as_deref()));
    log.info(format!("Using config: {}", config_path.display()));
    let config = match actions::load_config_from_path(&config_path.to_string_lossy()) {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors {
                log.error(error);
            }
            return exit::CONFIG;
        }
    };
    // The configured outputs are opened while still attached to the terminal, so a log
    // file that cannot be opened is reported there.
    let (settings, sinks) = match open_logging(&config, &cli, cli.detach) {
        Ok(opened) => opened,
        Err(error) => {
            log.error(error);
            return exit::CONFIG;
        }
    };
    let pid_file = cli.pid_file.as_deref().map(absolute_config_path);

    let readiness = if cli.detach {
        // SAFETY: no runtime (and no other thread) exists yet.
        match unsafe { daemon::detach(pid_file.as_deref()) } {
            Ok(daemon::Detached::Daemon(readiness)) => Some(readiness),
            Ok(daemon::Detached::Parent(status, message)) => {
                report_detached_start(log, status, &message);
                return status;
            }
            Err(error) => {
                log.error(error);
                return exit::FAILURE;
            }
        }
    } else {
        if let Some(path) = &pid_file {
            if let Err(error) = daemon::write_pid_file(path, std::process::id()) {
                log.error(error);
                return exit::FAILURE;
            }
        }
        None
    };

    // From here on every line goes to the configured outputs.
    dak::log::install(sinks);
    let log = settings.log;
    log.info(format!(
        "Loaded config version {} from {}",
        config.version,
        config_path.display()
    ));
    for warning in &config.warnings {
        log.warn(warning);
    }
    for detail in &config.font_details {
        log.debug(Subsystem::Fonts, detail);
    }

    let status = match build_runtime(log) {
        Some(runtime) => {
            let options = ServeOptions {
                cli: &cli,
                config_path: &config_path,
                conflict: Conflict::from_flags(cli.wait, cli.replace),
                detached: cli.detach,
                readiness: readiness.as_ref(),
            };
            runtime.block_on(serve(config, log, options))
        }
        None => exit::FAILURE,
    };
    if let Some(readiness) = &readiness {
        // Ended before startup completed: tell the waiting terminal why.
        let message = dak::log::last_error().unwrap_or_else(|| "stopped during startup".into());
        readiness.report(daemon::StartupReport::Failed(status, message));
    }
    if let Some(path) = &pid_file {
        daemon::remove_pid_file(path);
    }
    status
}

/// In the terminal `dak --detach` was started from, once the daemon reported: what
/// happened, with the daemon's own message.
fn report_detached_start(log: Log, status: u8, message: &str) {
    if status == exit::SUCCESS && message == NO_DEVICE_YET {
        log.warn(format!("dak is running in the background, but {message}"));
    } else if status == exit::SUCCESS {
        log.info(format!("dak is running in the background: {message}"));
    } else {
        log.error(format!(
            "dak failed to start in the background (exit status {status}): {message}"
        ));
    }
}

/// Resolves the logging setup from the config's `logging` section and the command-line
/// overrides (with `auto` resolved as `detached` says) and opens its outputs, ready to
/// be installed. Fails when an output cannot be opened.
fn open_logging(
    config: &actions::LoadedConfig,
    cli: &Cli,
    detached: bool,
) -> Result<(LogSettings, dak::log::Sinks), String> {
    let settings = LogSettings::resolve(
        &config.logging,
        &CliLogging::from_cli(cli),
        &Environment::probe(detached),
    )?;
    let sinks = settings.open()?;
    Ok((settings, sinks))
}

/// Makes `path` absolute against the current directory, so it keeps naming the same
/// file after a later change of directory (e.g. when detaching into the background).
/// A path that cannot be made absolute is returned unchanged.
fn absolute_config_path(path: &std::path::Path) -> std::path::PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Builds the multi-threaded tokio runtime the device tasks run on, logging (and
/// returning `None`) if the OS refuses to provide one.
fn build_runtime(log: Log) -> Option<tokio::runtime::Runtime> {
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            log.error(format!("failed to start the async runtime: {error}"));
            None
        }
    }
}

/// Everything [`serve`] needs besides the loaded config: how it was started and what to
/// do on conflicts, so a reload can reapply the same command line.
struct ServeOptions<'a> {
    /// The parsed command line (for the logging overrides on reload).
    cli: &'a Cli,
    /// The absolute config path, re-read on `SIGHUP`.
    config_path: &'a std::path::Path,
    /// What to do about devices held by another dak.
    conflict: Conflict,
    /// Whether the program detached from its terminal.
    detached: bool,
    /// The startup pipe to the waiting `--detach` terminal, if any.
    readiness: Option<&'a daemon::Readiness>,
}

/// Matches the config's `devices` definitions against the discovered hardware and
/// drives every present device: each connects with its own key/encoder counts, applies
/// the `on_start` scene, and reacts to keys, encoder events and scene timers. Returns the
/// exit status once the program is told to stop or has nothing left to drive.
///
/// This is the device supervisor: besides starting the device tasks it handles
/// `SIGHUP` (re-read the config; when valid, rebuild logging and restart every device
/// with it, keeping the locks of devices still in use), `SIGUSR1` (look again for
/// configured devices that are missing, while devices that gave up reconnecting look
/// for themselves) and the end of every task.
///
/// A device definition is matched to a discovered device by its serial number, falling
/// back to the VID:PID string when the definition's serial is "unknown". In the
/// foreground the program ends with [`exit::NO_DEVICE`] when no configured device is
/// (or remains) available; as a service (`--detach`, or under systemd) it keeps
/// running and waits for a rescan instead.
async fn serve(config: actions::LoadedConfig, log: Log, options: ServeOptions<'_>) -> u8 {
    // Signal listeners go in first, so a SIGTERM during discovery already ends the
    // program cleanly instead of killing it.
    let controller = Arc::new(Controller::new());
    if let Err(error) = control::spawn_signal_handler(controller.clone(), log) {
        log.error(format!("failed to install signal handlers: {error}"));
        return exit::FAILURE;
    }
    let service = service_mode(options.detached, daemon::notify_socket().is_some());
    let mut supervisor = Supervisor::new(config, log, &options, controller.clone(), service);

    let devices = match discover().await {
        Ok(devices) => devices,
        Err(error) => {
            log.error(format!("device discovery failed: {error}"));
            return exit::FAILURE;
        }
    };
    let assignments = match_devices(&supervisor.config.devices.by_id, &devices, log, true);
    if assignments.is_empty() && !service {
        log.error("no device defined in config was found");
        return exit::NO_DEVICE;
    }
    let (started, busy, failed) = supervisor.start(assignments).await;
    if supervisor.tasks.is_empty() && !service {
        if supervisor.stopped() {
            return exit::SUCCESS;
        }
        if busy > 0 && failed == 0 {
            log.error("every configured device found is in use by another dak");
            return exit::DEVICE_BUSY;
        }
        return exit::FAILURE;
    }
    let summary = supervisor.wait_started(started).await;
    if !supervisor.stopped() {
        log.debug(Subsystem::Device, format!("startup complete: {summary}"));
        daemon::notify(&format!("READY=1\n{}", daemon::status_line(&summary)));
        if let Some(readiness) = options.readiness {
            readiness.report(daemon::StartupReport::Ready(summary));
        }
    }
    supervisor.run().await
}

/// Whether the program runs as a service, which keeps it alive (waiting for a rescan)
/// when it has no device to drive: detached, or started by systemd.
fn service_mode(detached: bool, notify_socket: bool) -> bool {
    detached || notify_socket
}

/// Lists every attached keypad of the supported family.
async fn discover() -> Result<Vec<HidDevice>, MirajazzError> {
    Ok(list_devices(&hardware::QUERIES)
        .await?
        .into_iter()
        .collect())
}

/// The device locks this process holds, by lock file name. Shared between the
/// supervisor and the device tasks, so a lock can outlive the task that took it (a
/// reload restarts the task but keeps the lock) and a task can give its lock up while
/// parked.
#[derive(Clone, Default)]
struct LockTable(Arc<std::sync::Mutex<HashMap<String, DeviceLock>>>);

impl LockTable {
    /// Whether the lock of `key` is held.
    fn holds(&self, key: &DeviceKey) -> bool {
        self.0
            .lock()
            .expect("lock table poisoned")
            .contains_key(&key.file_name())
    }

    /// Records a newly taken lock.
    fn insert(&self, key: &DeviceKey, lock: DeviceLock) {
        self.0
            .lock()
            .expect("lock table poisoned")
            .insert(key.file_name(), lock);
    }

    /// Releases the lock of `key`, if held.
    fn release(&self, key: &DeviceKey) {
        self.0
            .lock()
            .expect("lock table poisoned")
            .remove(&key.file_name());
    }

    /// Releases every lock whose file name is not in `keep`.
    fn retain(&self, keep: &std::collections::HashSet<String>) {
        self.0
            .lock()
            .expect("lock table poisoned")
            .retain(|name, _| keep.contains(name));
    }

    /// Ensures the lock of `key` is held, taking it per `conflict` when not.
    async fn ensure(
        &self,
        dir: &std::path::Path,
        key: &DeviceKey,
        conflict: Conflict,
        stop: &StopSignal,
        log: Log,
        start: Option<&StartSignal>,
    ) -> Result<(), LockError> {
        if self.holds(key) {
            return Ok(());
        }
        let lock = take_device_lock(dir, key, conflict, stop, log, start).await?;
        self.insert(key, lock);
        Ok(())
    }
}

/// What a device task tells the supervisor while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskEvent {
    /// The device gave up reconnecting and waits for a rescan.
    Parked(u8),
    /// A parked device is looking for its device again.
    Resumed(u8),
}

/// How a device task that gave up reconnecting waits for a rescan and gets back in
/// (see [`await_reconnect`]).
struct Park {
    /// The device's logical number.
    number: u8,
    /// Its lock identity.
    key: DeviceKey,
    /// The process's device locks.
    locks: LockTable,
    /// Where lock files live.
    lock_dir: std::path::PathBuf,
    /// What to do when another dak took the device meanwhile.
    conflict: Conflict,
    /// Where rescan requests are recorded.
    controller: Arc<Controller>,
    /// Tells the supervisor about parking and resuming.
    events: mpsc::UnboundedSender<TaskEvent>,
}

/// The state of one generation of device tasks (everything a reload replaces) and the
/// bookkeeping across generations.
struct Supervisor<'a> {
    /// The running configuration.
    config: Arc<actions::LoadedConfig>,
    /// The variable/default state shared by every device of this generation.
    variables: Arc<std::sync::Mutex<Variables>>,
    /// The current output filter.
    log: Log,
    /// How the program was started.
    options: &'a ServeOptions<'a>,
    /// Signal requests.
    controller: Arc<Controller>,
    /// Whether to keep running without devices.
    service: bool,
    /// Where lock files live.
    lock_dir: std::path::PathBuf,
    /// The locks held.
    locks: LockTable,
    /// Stops this generation's tasks (on quit or reload).
    generation: Arc<StopSource>,
    /// Stops `generation` when a quit is requested.
    quit_watch: tokio::task::JoinHandle<()>,
    /// The running device tasks, each returning its device number and outcome.
    tasks: tokio::task::JoinSet<(u8, Result<(), TaskError>)>,
    /// Device number to lock file name of every running task.
    live: HashMap<u8, String>,
    /// Running tasks that gave up and wait for a rescan.
    parked: std::collections::HashSet<u8>,
    /// Task events, and the sender handed to new tasks.
    events: (
        mpsc::UnboundedSender<TaskEvent>,
        mpsc::UnboundedReceiver<TaskEvent>,
    ),
    /// The first failure status seen.
    status: u8,
    /// The reload count already acted on. Taken when the supervisor is created (right
    /// after the signal handler is installed), not when [`Supervisor::run`] starts, so
    /// a `SIGHUP` arriving during startup - before or after `READY=1` - is still acted
    /// on once `run` begins.
    reload_seen: u64,
    /// The rescan count already acted on; see `reload_seen`.
    rescan_seen: u64,
}

impl<'a> Supervisor<'a> {
    /// A supervisor for `config` with no tasks started yet.
    fn new(
        config: actions::LoadedConfig,
        log: Log,
        options: &'a ServeOptions<'a>,
        controller: Arc<Controller>,
        service: bool,
    ) -> Self {
        let variables = Arc::new(std::sync::Mutex::new(Variables::new(
            config.variables.clone(),
            &config.defaults,
        )));
        let (generation, quit_watch) = new_generation(&controller);
        let requested = controller.state();
        Self {
            reload_seen: requested.reload,
            rescan_seen: requested.rescan,
            config: Arc::new(config),
            variables,
            log,
            options,
            controller,
            service,
            lock_dir: lock::lock_dir(),
            locks: LockTable::default(),
            generation,
            quit_watch,
            tasks: tokio::task::JoinSet::new(),
            live: HashMap::new(),
            parked: std::collections::HashSet::new(),
            events: mpsc::unbounded_channel(),
            status: exit::SUCCESS,
        }
    }

    /// Whether this generation was stopped.
    fn stopped(&self) -> bool {
        self.generation.signal().is_stopped()
    }

    /// Starts a task for every assignment whose lock can be taken, returning their
    /// startup reports plus how many devices were skipped as busy and as failed.
    ///
    /// Refusing and replacing settle the lock before the task starts (so a device held
    /// elsewhere is merely skipped); waiting happens inside the task, so the other
    /// devices start meanwhile.
    async fn start(
        &mut self,
        assignments: Vec<(u8, Mapping, HidDeviceInfo)>,
    ) -> (Vec<tokio::sync::oneshot::Receiver<Started>>, usize, usize) {
        let (mut busy, mut failed) = (0usize, 0usize);
        let mut started = Vec::new();
        let log = self.log;
        let conflict = self.options.conflict;
        for (device_number, definition, device_info) in assignments {
            let key = device_key(&device_info);
            if self.live.values().any(|name| *name == key.file_name()) {
                continue;
            }
            if conflict != Conflict::Wait {
                let signal = self.generation.signal();
                match self
                    .locks
                    .ensure(&self.lock_dir, &key, conflict, &signal, log, None)
                    .await
                {
                    Ok(()) => {}
                    Err(LockError::Busy(_)) => {
                        busy += 1;
                        continue;
                    }
                    Err(LockError::Cancelled) => break,
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                }
            }
            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            started.push(start_rx);
            let start = Arc::new(StartSignal::new(start_tx));
            let config = self.config.clone();
            let variables = self.variables.clone();
            let signal = self.generation.signal();
            let locks = self.locks.clone();
            let lock_dir = self.lock_dir.clone();
            let park = Park {
                number: device_number,
                key: key.clone(),
                locks: locks.clone(),
                lock_dir: lock_dir.clone(),
                conflict,
                controller: self.controller.clone(),
                events: self.events.0.clone(),
            };
            self.live.insert(device_number, key.file_name());
            self.tasks.spawn(async move {
                if let Err(error) = locks
                    .ensure(&lock_dir, &key, conflict, &signal, log, Some(&start))
                    .await
                {
                    return match error {
                        LockError::Cancelled => (device_number, Ok(())),
                        _ => (device_number, Err(TaskError::Reported(exit::FAILURE))),
                    };
                }
                let result = run_device(
                    device_number,
                    definition,
                    device_info,
                    config.scenes.clone(),
                    log,
                    config.defaults.clone(),
                    variables,
                    config.fonts.clone(),
                    signal,
                    start,
                    park,
                )
                .await
                .map_err(TaskError::Device);
                if result.is_err() {
                    locks.release(&key);
                }
                (device_number, result)
            });
        }
        (started, busy, failed)
    }

    /// Waits until every task in `started` has reported (or ended) and summarizes
    /// how many connected and how many wait for a lock.
    async fn wait_started(&self, started: Vec<tokio::sync::oneshot::Receiver<Started>>) -> String {
        let (mut connected, mut waiting) = (0usize, 0usize);
        for start in started {
            match start.await {
                Ok(Started::Connected) => connected += 1,
                Ok(Started::Waiting) => waiting += 1,
                Err(_) => {}
            }
        }
        startup_summary(connected, waiting)
    }

    /// Records how a finished task ended.
    fn finished(&mut self, joined: Result<(u8, Result<(), TaskError>), tokio::task::JoinError>) {
        let failure = match joined {
            Ok((number, outcome)) => {
                self.live.remove(&number);
                self.parked.remove(&number);
                match outcome {
                    Ok(()) => return,
                    Err(TaskError::Device(error)) => {
                        self.log
                            .error(format!("device task ended with an error: {error}"));
                        device_error_status(&error)
                    }
                    Err(TaskError::Reported(status)) => status,
                }
            }
            Err(error) => {
                self.log
                    .error(format!("device task failed unexpectedly: {error}"));
                exit::FAILURE
            }
        };
        if self.status == exit::SUCCESS {
            self.status = failure;
        }
    }

    /// Stops this generation and waits for every task, recording their outcomes when
    /// `record` is set (a reload discards them).
    async fn stop_all(&mut self, record: bool) {
        self.generation.stop();
        while let Some(joined) = self.tasks.join_next().await {
            if record {
                self.finished(joined);
            } else if let Ok((number, _)) = joined {
                self.live.remove(&number);
            }
        }
        self.live.clear();
        self.parked.clear();
    }

    /// Whether nothing is being driven: no task runs, or every running one gave up.
    fn idle(&self) -> bool {
        self.live.keys().all(|number| self.parked.contains(number))
    }

    /// Reacts to signals and task ends until the program should exit; returns the
    /// exit status.
    async fn run(&mut self) -> u8 {
        loop {
            if self.idle() {
                if !self.service {
                    // Nothing left to drive in the foreground: end like before parking
                    // existed, with the "no device" status when devices were given up.
                    let gave_up = !self.parked.is_empty();
                    self.stop_all(false).await;
                    if gave_up && self.status == exit::SUCCESS {
                        return exit::NO_DEVICE;
                    }
                    return self.status;
                }
                daemon::notify(&daemon::status_line(
                    "waiting for devices (send SIGUSR1 to rescan)",
                ));
            }
            let controller = self.controller.clone();
            tokio::select! {
                _ = controller.quit_requested() => {
                    daemon::notify("STOPPING=1");
                    self.stop_all(true).await;
                    return self.status;
                }
                seen = controller.reload_after(self.reload_seen) => {
                    self.reload_seen = seen;
                    self.reload().await;
                }
                seen = controller.rescan_after(self.rescan_seen) => {
                    self.rescan_seen = seen;
                    self.rescan().await;
                }
                Some(joined) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    self.finished(joined);
                }
                Some(event) = self.events.1.recv() => match event {
                    TaskEvent::Parked(number) => { self.parked.insert(number); }
                    TaskEvent::Resumed(number) => { self.parked.remove(&number); }
                },
            }
        }
    }

    /// `SIGUSR1`: starts tasks for configured devices that have none (never found, or
    /// skipped as busy). Parked tasks react to the same request on their own.
    async fn rescan(&mut self) {
        let log = self.log;
        let missing: std::collections::BTreeMap<u8, Mapping> = self
            .config
            .devices
            .by_id
            .iter()
            .filter(|(number, _)| !self.live.contains_key(number))
            .map(|(number, definition)| (*number, definition.clone()))
            .collect();
        if missing.is_empty() {
            if self.parked.is_empty() {
                log.debug(
                    Subsystem::Device,
                    "rescan: every configured device is running",
                );
            }
            return;
        }
        let devices = match discover().await {
            Ok(devices) => devices,
            Err(error) => {
                log.warn(format!("rescan: device discovery failed: {error}"));
                return;
            }
        };
        let assignments = match_devices(&missing, &devices, log, false);
        if assignments.is_empty() {
            log.info("rescan: no missing device was found");
            return;
        }
        let (started, _, _) = self.start(assignments).await;
        let summary = self.wait_started(started).await;
        log.info(format!("rescan: {summary}"));
        daemon::notify(&daemon::status_line(&summary));
    }

    /// `SIGHUP`: re-reads the config. An invalid one is reported and the running one
    /// kept (with the log file reopened); a valid one replaces logging, then every
    /// device task is stopped cleanly and restarted from its `on_start` scene with
    /// fresh variables, keeping the locks of devices that stay in use.
    async fn reload(&mut self) {
        let path = self.options.config_path;
        daemon::notify(&format!("RELOADING=1\nMONOTONIC_USEC={}", monotonic_usec()));
        self.log
            .info(format!("reloading configuration from {}", path.display()));
        let config = match actions::load_config_from_path(&path.to_string_lossy()) {
            Ok(config) => config,
            Err(errors) => {
                for error in &errors {
                    self.log.error(error);
                }
                self.log
                    .error("configuration not reloaded; still running the previous one");
                reopen_log_file(self.log);
                daemon::notify("READY=1");
                return;
            }
        };
        match open_logging(&config, self.options.cli, self.options.detached) {
            Ok((settings, sinks)) => {
                dak::log::install(sinks);
                self.log = settings.log;
            }
            Err(error) => {
                self.log
                    .error(format!("{error}; keeping the previous log outputs"));
                reopen_log_file(self.log);
            }
        }
        let log = self.log;
        for warning in &config.warnings {
            log.warn(warning);
        }

        self.stop_all(false).await;
        self.quit_watch.abort();
        let (generation, quit_watch) = new_generation(&self.controller);
        self.generation = generation;
        self.quit_watch = quit_watch;
        self.variables = Arc::new(std::sync::Mutex::new(Variables::new(
            config.variables.clone(),
            &config.defaults,
        )));
        self.config = Arc::new(config);

        let devices = match discover().await {
            Ok(devices) => devices,
            Err(error) => {
                log.error(format!("device discovery failed: {error}"));
                Vec::new()
            }
        };
        let assignments = match_devices(&self.config.devices.by_id, &devices, log, true);
        let keep = assignments
            .iter()
            .map(|(_, _, info)| device_key(info).file_name())
            .collect();
        self.locks.retain(&keep);
        let (started, _, _) = self.start(assignments).await;
        let summary = self.wait_started(started).await;
        log.info(format!("configuration reloaded: {summary}"));
        daemon::notify(&format!("READY=1\n{}", daemon::status_line(&summary)));
    }
}

/// A fresh generation stop source, plus the task that stops it on a quit request (so
/// lock waits and device loops end even while the supervisor is busy, e.g. starting
/// devices).
fn new_generation(controller: &Arc<Controller>) -> (Arc<StopSource>, tokio::task::JoinHandle<()>) {
    let generation = Arc::new(StopSource::new());
    let watch = {
        let (controller, generation) = (controller.clone(), generation.clone());
        tokio::spawn(async move {
            controller.quit_requested().await;
            generation.stop();
        })
    };
    (generation, watch)
}

/// Reopens the installed log file (after logrotate moved it), logging a failure.
fn reopen_log_file(log: Log) {
    if let Some(sinks) = dak::log::installed() {
        if let Err(error) = sinks.reopen() {
            log.error(format!("cannot reopen the log file: {error}"));
        }
    }
}

/// `CLOCK_MONOTONIC` in microseconds, as systemd wants with `RELOADING=1`.
fn monotonic_usec() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes into `now`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1_000
}

/// How far a device task got by the end of startup (see [`StartSignal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Started {
    /// Connected and painted its `on_start` scene.
    Connected,
    /// Waiting (`--wait`) for another dak to release it.
    Waiting,
}

/// A device task's one-shot "startup done" report to [`serve`]; only the first report
/// counts, and a task ending without one reports nothing (the receiver sees it dropped).
struct StartSignal(std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Started>>>);

impl StartSignal {
    /// Wraps the sending half of the report channel.
    fn new(sender: tokio::sync::oneshot::Sender<Started>) -> Self {
        Self(std::sync::Mutex::new(Some(sender)))
    }

    /// Sends `state` unless something was already reported.
    fn report(&self, state: Started) {
        if let Some(sender) = self.0.lock().expect("start signal poisoned").take() {
            let _ = sender.send(state);
        }
    }
}

/// The startup summary of a service that found nothing to drive yet.
const NO_DEVICE_YET: &str = "no configured device available yet; send SIGUSR1 to rescan";

/// The one-line startup summary, e.g. "2 devices connected, 1 waiting for another dak".
fn startup_summary(connected: usize, waiting: usize) -> String {
    let devices = |n: usize| {
        if n == 1 {
            "1 device".to_string()
        } else {
            format!("{n} devices")
        }
    };
    match (connected, waiting) {
        (0, 0) => NO_DEVICE_YET.to_string(),
        (c, 0) => format!("{} connected", devices(c)),
        (0, w) => format!("{} waiting for another dak to release it", devices(w)),
        (c, w) => format!("{} connected, {w} waiting for another dak", devices(c)),
    }
}

/// Why a device task in [`serve`] ended early.
enum TaskError {
    /// Driving the device failed.
    Device(MirajazzError),
    /// Something already logged went wrong; exit with this status.
    Reported(u8),
}

/// The lock identity of a discovered device (see [`DeviceKey`]).
fn device_key(info: &HidDeviceInfo) -> DeviceKey {
    DeviceKey::new(
        info.vendor_id,
        info.product_id,
        info.serial_number.as_deref(),
        &format!("{:?}", info.id),
    )
}

/// Takes the lock of one device per `conflict`, logging what happens: a skipped busy
/// device is a warning, a failure to lock (or to take over) an error, and waiting for
/// another instance an info line.
async fn take_device_lock(
    dir: &std::path::Path,
    key: &DeviceKey,
    conflict: Conflict,
    stop: &StopSignal,
    log: Log,
    start: Option<&StartSignal>,
) -> Result<DeviceLock, LockError> {
    let result = lock::acquire(dir, key, conflict, stop, |holder| {
        if let Some(start) = start {
            start.report(Started::Waiting);
        }
        let holder = holder
            .map(|holder| holder.describe())
            .unwrap_or_else(|| "another dak".to_string());
        match conflict {
            Conflict::Replace => log.info(format!(
                "device {}: asked {holder} to stop; waiting for it to release the device",
                key.describe()
            )),
            _ => log.info(format!(
                "device {} is in use by {holder}; waiting for it to be released",
                key.describe()
            )),
        }
    })
    .await;
    match &result {
        Ok(lock) => log.debug(
            Subsystem::Device,
            format!(
                "device {}: locked {}",
                key.describe(),
                lock.path().display()
            ),
        ),
        Err(error @ LockError::Busy(_)) => log.warn(error.describe(key)),
        Err(LockError::Cancelled) => {
            log.debug(Subsystem::Device, LockError::Cancelled.describe(key))
        }
        Err(error) => log.error(error.describe(key)),
    }
    result
}

/// The exit status a device task's error maps to: a device that could not be found (or
/// was given up on) is [`exit::NO_DEVICE`]; anything else is [`exit::FAILURE`].
fn device_error_status(error: &MirajazzError) -> u8 {
    match error {
        MirajazzError::DeviceNotFoundError => exit::NO_DEVICE,
        _ => exit::FAILURE,
    }
}

/// Pairs each of `definitions` with the discovered device it describes, warning about
/// definitions without hardware and (when `warn_undefined`) hardware without a
/// definition.
fn match_devices(
    definitions: &std::collections::BTreeMap<u8, Mapping>,
    devices: &[HidDevice],
    log: Log,
    warn_undefined: bool,
) -> Vec<(u8, Mapping, HidDeviceInfo)> {
    let mut assignments: Vec<(u8, Mapping, HidDeviceInfo)> = Vec::new();
    for (device_id, definition) in definitions {
        match devices.iter().position(|dev| {
            actions::discovered_device_matches(
                definition,
                &dev.serial_number,
                dev.vendor_id,
                dev.product_id,
            )
        }) {
            Some(index) => {
                log.debug(
                    Subsystem::Device,
                    format!("device {device_id}: matched config definition to discovered hardware"),
                );
                assignments.push((*device_id, definition.clone(), devices[index].clone()));
            }
            None => log.warn(format!(
                "device #{device_id} ({} s/n {}, expecting {}) defined in config was not found",
                definition.device_name, definition.serial, definition.device_id
            )),
        }
    }

    for dev in devices.iter().filter(|_| warn_undefined) {
        if !assignments.iter().any(|(_, _, info)| info.id == dev.id) {
            log.warn(format!(
                "device found but not defined in config; ignoring: {}",
                device_summary_line(
                    &dev.id,
                    &dev.serial_number,
                    dev.vendor_id,
                    dev.product_id,
                    &dev.name
                )
            ));
        }
    }
    assignments
}

/// Runs one present device: connects with the key/encoder counts from its config
/// definition, applies the `on_start` scene, and reacts to keys, encoder events, scene
/// timers and complex press events until the reader closes or `stop` fires, then
/// restores the buttons this session changed and shuts the device down.
///
/// `device_number` is the id the definition is keyed under in the config; the runner
/// drives references naming this number and skips references to other devices with a
/// warning, so the same scenes address every device by its own number. `defaults`
/// carries the press-detection timing knobs from the config `defaults` section, and
/// `fonts` the fonts loaded from it that button text is drawn with.
#[allow(clippy::too_many_arguments)]
async fn run_device(
    device_number: u8,
    definition: Mapping,
    device_info: HidDeviceInfo,
    scenes: Value,
    log: Log,
    defaults: Defaults,
    variables: Arc<std::sync::Mutex<Variables>>,
    fonts: Arc<dak::text::FontSet>,
    stop: StopSignal,
    start: Arc<StartSignal>,
    park: Park,
) -> Result<(), MirajazzError> {
    log.debug(
        Subsystem::Device,
        format!("Connecting to device {device_number}"),
    );
    for line in device_info_lines(
        &device_info.id,
        &device_info.serial_number,
        device_info.vendor_id,
        device_info.product_id,
        &device_info.name,
    ) {
        log.debug(Subsystem::Device, line);
    }

    // Every device_info reaching this point already matched hardware::QUERIES during
    // discovery in main(), so this should always resolve; treated as a hard error
    // rather than assumed, in case that invariant is ever broken.
    let kind = match hardware::Kind::from_vid_pid(device_info.vendor_id, device_info.product_id) {
        Some(kind) => kind,
        None => {
            log.error(format!(
                "device {device_number}: unrecognized vendor/product ID {:04x}:{:04x}",
                device_info.vendor_id, device_info.product_id
            ));
            return Err(MirajazzError::DeviceNotFoundError);
        }
    };
    log.debug(
        Subsystem::Device,
        format!(
            "device {device_number}: recognized as {}",
            kind.human_name()
        ),
    );

    // The config definition's own protocol_version, when set, overrides the
    // recognized kind's default - e.g. for a kind this project has not verified
    // itself, if a different protocol version turns out to work better for the
    // user's specific unit. See `Mapping::protocol_version`'s doc comment.
    let protocol_version = definition
        .protocol_version
        .unwrap_or(kind.protocol_version());
    log.debug(
        Subsystem::Device,
        format!("device {device_number}: protocol version {protocol_version}"),
    );

    // Connect to the device using the counts its config definition declares. A failure
    // here, on the very first connection, is still fatal for this device; only a device
    // lost *after* it connected is waited for (see the reconnect handling below).
    let connected = connect_device(
        &device_info,
        &definition,
        protocol_version,
        defaults.button_brightness,
        defaults.encoder_brightness,
    )
    .await?;

    // Print out some info from the device
    log.debug(
        Subsystem::Device,
        format!("Connected to '{}'", connected.serial_number()),
    );
    log.info(format!(
        "Connected to {} s/n {} as device #{device_number} using protocol version {protocol_version}",
        device_info.name,
        connected.serial_number()
    ));
    // How often, and how many times, to try getting this device back if it disappears:
    // its own settings where the definition has them, else the `defaults` ones.
    let reconnect_policy = reconnect::ReconnectPolicy::resolve(
        &defaults,
        definition.device_reconnect_interval,
        definition.device_reconnect_max_attempts,
    );
    // The runner draws through this handle, whose connection is swapped for a fresh
    // one whenever the device disappears (host suspend, unplug) and comes back.
    let device = SwappableDevice::new(connected, reconnect::is_disconnect_error);
    let connected = device
        .current()
        .expect("a freshly wrapped device is connected");

    log.debug(
        Subsystem::Device,
        format!("Key count: {}", connected.key_count()),
    );
    log.debug(
        Subsystem::Device,
        format!("Encoder count: {}", connected.encoder_count()),
    );
    log.debug(
        Subsystem::Device,
        format!(
            "Supports_both_encoder_states: {}",
            connected.supports_both_encoder_states()
        ),
    );
    // Only the swappable handle may keep the connection alive: a reconnect needs every
    // reference to the old one gone, so its OS handle actually closes.
    drop(connected);

    // async image_exec/text_exec results land on buttons through this runner and its channel
    let (exec_tx, exec_rx) = mpsc::channel::<actions::ExecEvent>(8);

    // A button with a nonzero `refresh_seconds` sends its own key here once its
    // interval elapses; the runner redraws just that button and re-arms the next tick.
    let (refresh_tx, refresh_rx) = mpsc::channel::<u8>(8);

    // Buttons this device model has no display on; assigning an image to them is
    // pointless, so the runner warns, skips the work and the transfer.
    let screenless_buttons: std::collections::HashSet<u8> = definition
        .buttons
        .iter()
        .filter(|button| !button.screen)
        .map(|button| button.number)
        .collect();
    let mut runner = actions::SceneRunner::new(
        device_number,
        &device,
        kind.image_format(),
        exec_tx,
        refresh_tx,
        log,
        &screenless_buttons,
        defaults.background.clone(),
        defaults.text_color.clone(),
    );
    // Setup params are expanded against the shared variable state, re-resolved on every
    // refresh tick (see `SceneRunner::set_variables`).
    runner.set_variables(variables.clone());
    runner.set_text_settings(defaults.markup, fonts);

    // Complex press events (short/long press, double click) are confirmed through the
    // click channel, scene timers fire through the timer channel; see `Session`.
    let (click_tx, click_rx) = mpsc::channel::<(Reference, ClickEvent)>(8);
    let (timer_tx, timer_rx) = mpsc::channel::<Vec<String>>(1);
    let mut session = Session::new(
        device_number,
        definition,
        runner,
        scenes,
        defaults,
        variables.clone(),
        click_tx,
        timer_tx,
        log,
    );
    session.enter_on_start(&device).await;
    start.report(Started::Connected);
    let mut channels = SessionChannels {
        timer_rx,
        click_rx,
        exec_rx,
        refresh_rx,
    };

    // One pass per connection: the input loop runs until the program is told to stop
    // (a stop signal, a closed channel) or the device goes away; then this waits for the
    // device to come back, swaps the new connection in, repaints and loops again.
    'connection: loop {
        let Some(reader) = device
            .current()
            .map(|connection| connection.get_reader(|_, _| Ok(DeviceInput::NoData)))
        else {
            // Lost again while repainting after a reconnect.
            match await_reconnect(
                device_number,
                &ReopenDevice {
                    definition: &session.definition,
                    protocol_version,
                },
                &device_info.name,
                &device,
                &mut session.runner,
                &variables,
                log,
                "device lost while reconnecting",
                reconnect_policy,
                &stop,
                &park,
            )
            .await
            {
                Reconnect::Reconnected => continue 'connection,
                Reconnect::Cancelled => return Ok(()),
            }
        };
        let end = session
            .run_connection(&reader, &mut channels, &stop, &device)
            .await;
        // The reader holds the connection's input handle open; it must go before any
        // reconnect can open the device again.
        drop(reader);
        match end {
            SessionEnd::Quit => break 'connection,
            SessionEnd::Disconnected(reason) => {
                session.reset_after_disconnect();
                match await_reconnect(
                    device_number,
                    &ReopenDevice {
                        definition: &session.definition,
                        protocol_version,
                    },
                    &device_info.name,
                    &device,
                    &mut session.runner,
                    &variables,
                    log,
                    &reason,
                    reconnect_policy,
                    &stop,
                    &park,
                )
                .await
                {
                    Reconnect::Reconnected => continue 'connection,
                    // Stopped while waiting (possibly parked after giving up): nothing
                    // left to restore on a missing device.
                    Reconnect::Cancelled => return Ok(()),
                }
            }
        }
    }
    let runner = &mut session.runner;

    // Restore the buttons this program touched: clear the image on every button whose
    // image the session changed, then flush. Buttons never changed are left alone - and
    // a device that is already gone has nothing left to restore.
    if device.is_connected() {
        if let Err(error) = runner.clear_changed_button_images().await {
            warn_unless_disconnected(
                log,
                runner,
                format!("failed to restore changed buttons: {error}"),
            );
        }
    }

    match device.current() {
        Some(connection) => {
            if let Err(error) = connection.shutdown().await {
                log.warn(format!(
                    "device #{device_number}: failed to shut down: {error}"
                ));
            }
        }
        None => log.debug(
            Subsystem::Device,
            format!("device #{device_number}: already gone at shutdown"),
        ),
    }
    Ok(())
}

/// What one control report resolves to, see [`route`].
#[derive(Debug, PartialEq)]
enum Dispatch {
    /// A press or release edge of a pressable control (a button or a pushed encoder).
    Edge(Reference),
    /// An encoder turn, with its event name (`turn_cw`/`turn_ccw`).
    Turn(Reference, &'static str),
}

/// Resolves the raw `code` of a report from device `device_number` through its
/// `definition` to the control it names, or `None` (logged as debug output) when no
/// control uses the code or the control is beyond the definition's counts.
///
/// The raw code names a widget by its captured press/release or turn code, not by its
/// number (buttons without a display report far larger codes).
fn route(
    definition: &Mapping,
    device_number: u8,
    code: u8,
    pressed: bool,
    log: Log,
) -> Option<Dispatch> {
    let Some(event) = definition.control_event(code, pressed) else {
        log.debug(
            Subsystem::Device,
            format!("no control uses raw code {code}; skipping"),
        );
        return None;
    };
    let (number, count, what) = match event {
        ControlEvent::Button { number } => (number, definition.key_count, "button"),
        ControlEvent::EncoderPress { number } | ControlEvent::EncoderTurn { number, .. } => {
            (number, definition.encoder_count, "encoder")
        }
    };
    if number > count {
        log.debug(
            Subsystem::Device,
            format!("{what} {number} is out of range (device has {count} {what}s); skipping"),
        );
        return None;
    }
    Some(match event {
        ControlEvent::Button { number } => Dispatch::Edge(Reference::button(device_number, number)),
        ControlEvent::EncoderPress { number } => {
            Dispatch::Edge(Reference::encoder(device_number, number))
        }
        ControlEvent::EncoderTurn { number, direction } => Dispatch::Turn(
            Reference::encoder(device_number, number),
            match direction {
                TwistDirection::Clockwise => "turn_cw",
                TwistDirection::CounterClockwise => "turn_ccw",
            },
        ),
    })
}

/// The receiving ends of a device session's event channels (their senders live in the
/// [`Session`], its runner and the tasks they spawn).
struct SessionChannels {
    /// Scene timer deliveries.
    timer_rx: mpsc::Receiver<Vec<String>>,
    /// Confirmed short presses.
    click_rx: mpsc::Receiver<(Reference, ClickEvent)>,
    /// `*_exec` results and completed `$(...)` assignments.
    exec_rx: mpsc::Receiver<actions::ExecEvent>,
    /// Refresh ticks of buttons with `refresh_seconds`.
    refresh_rx: mpsc::Receiver<u8>,
}

/// One device's input-loop state, kept across its connections: the scene runner, the
/// current and previous scene, the press-detection state and the scene timer.
///
/// [`run_device`] owns the connection side (connecting, reconnecting, the reader); every
/// event is handed to one of the `on_*` methods, which is what the tests drive.
struct Session<'r, D: ButtonDevice> {
    /// The output filter.
    log: Log,
    /// The number the device's definition is keyed under.
    device_number: u8,
    /// The device's config definition (raw code to control translation, counts).
    definition: Mapping,
    /// Draws the scenes on the device.
    runner: actions::SceneRunner<'r, D>,
    /// The config's scenes.
    scenes: Value,
    /// The press-detection timing knobs.
    defaults: Defaults,
    /// The shared variable/default state.
    variables: Arc<std::sync::Mutex<Variables>>,
    /// The active scene.
    current_scene: String,
    /// The scene switched away from last; its actions are inherited (see
    /// `actions::action_for_event`).
    previous_scene: Option<String>,
    /// Pressable controls currently held down, so repeated press reports of the same
    /// widget are not re-dispatched and releases without a press are ignored.
    down_controls: std::collections::HashSet<Reference>,
    /// Decides which complex press event each press or release produces.
    click_detector: ClickDetector,
    /// Where delayed short-press confirmations are sent.
    click_tx: mpsc::Sender<(Reference, ClickEvent)>,
    /// Short presses waiting out the double-click gap.
    pending_shorts: HashMap<Reference, PendingShortPress>,
    /// The armed scene timer, if any.
    timer_handle: Option<tokio::task::JoinHandle<()>>,
    /// Where the scene timer delivers its actions.
    timer_tx: mpsc::Sender<Vec<String>>,
}

impl<'r, D: ButtonDevice> Session<'r, D> {
    /// A session in the `on_start` scene (not yet drawn, see
    /// [`Session::enter_on_start`]).
    #[allow(clippy::too_many_arguments)]
    fn new(
        device_number: u8,
        definition: Mapping,
        runner: actions::SceneRunner<'r, D>,
        scenes: Value,
        defaults: Defaults,
        variables: Arc<std::sync::Mutex<Variables>>,
        click_tx: mpsc::Sender<(Reference, ClickEvent)>,
        timer_tx: mpsc::Sender<Vec<String>>,
        log: Log,
    ) -> Self {
        let click_detector = ClickDetector::new(&defaults);
        Self {
            log,
            device_number,
            definition,
            runner,
            scenes,
            defaults,
            variables,
            current_scene: String::from("on_start"),
            previous_scene: None,
            down_controls: std::collections::HashSet::new(),
            click_detector,
            click_tx,
            pending_shorts: HashMap::new(),
            timer_handle: None,
            timer_tx,
        }
    }

    /// Draws the `on_start` scene, flushes it to `device` and arms its timer. Draw
    /// failures are not fatal: a device that already went away is noticed by the input
    /// loop, which waits for it to come back.
    async fn enter_on_start(&mut self, device: &impl ButtonDevice) {
        let log = self.log;
        if let Err(error) = self.runner.enter_scene("on_start", &self.scenes).await {
            warn_unless_disconnected(
                log,
                &self.runner,
                format!("failed to apply on_start scene: {error}"),
            );
        }
        if let Err(error) = device.flush().await {
            warn_unless_disconnected(
                log,
                &self.runner,
                format!("failed to flush on_start scene: {error}"),
            );
        }
        self.timer_handle = arm_scene_timer(
            &self.current_scene,
            &self.scenes,
            &self.timer_tx,
            log,
            &self.variables,
        )
        .await;
    }

    /// Handles one raw input report: noise is skipped, a control's press/release goes
    /// through the press detection, an encoder turn runs its `turn_cw`/`turn_ccw`
    /// actions.
    async fn on_report(&mut self, data: &[u8]) {
        let Some((code, state)) = dak::input::decode_report(data) else {
            return;
        };
        let pressed = state != 0;
        let log = self.log;
        log.debug(
            Subsystem::Device,
            format!(
                "Key {code}, {}",
                if pressed { "pressed" } else { "released" }
            ),
        );
        match route(&self.definition, self.device_number, code, pressed, log) {
            Some(Dispatch::Edge(reference)) => {
                run_pressable_edge(
                    log,
                    &mut self.runner,
                    &mut self.current_scene,
                    &mut self.previous_scene,
                    &self.scenes,
                    &reference,
                    pressed,
                    &mut self.down_controls,
                    &mut self.click_detector,
                    &self.click_tx,
                    &mut self.pending_shorts,
                    &self.defaults,
                    &mut self.timer_handle,
                    &self.timer_tx,
                    &self.variables,
                )
                .await;
            }
            Some(Dispatch::Turn(reference, event)) => {
                self.run_bound(&reference, event).await;
            }
            None => {}
        }
    }

    /// Runs the actions `reference` has bound to `event` in the current scene.
    async fn run_bound(&mut self, reference: &Reference, event: &str) {
        run_bound_action(
            self.log,
            &mut self.runner,
            &mut self.current_scene,
            &mut self.previous_scene,
            &self.scenes,
            reference,
            event,
            &mut self.timer_handle,
            &self.timer_tx,
            &self.variables,
        )
        .await;
    }

    /// Runs the actions a scene timer delivered.
    async fn on_timer(&mut self, actions: Vec<String>) {
        self.log.debug(
            Subsystem::Actions,
            format!("timer for scene \"{}\" -> {actions:?}", self.current_scene),
        );
        let actions: Vec<&str> = actions.iter().map(String::as_str).collect();
        run_actions(
            self.log,
            &mut self.runner,
            &mut self.current_scene,
            &mut self.previous_scene,
            &self.scenes,
            &actions,
            &mut self.timer_handle,
            &self.timer_tx,
            &self.variables,
        )
        .await;
    }

    /// A short press confirmed after the double-click gap: runs its `short_press`
    /// actions.
    async fn on_click(&mut self, reference: Reference, event: ClickEvent) {
        // The confirmation task finished on its own; drop its handle.
        self.pending_shorts.remove(&reference);
        self.click_detector.confirm_single();
        match event {
            ClickEvent::ShortPress => {
                self.log.debug(
                    Subsystem::Device,
                    format!("{reference} single press detected"),
                );
                self.run_bound(&reference, "short_press").await;
            }
        }
    }

    /// An `*_exec` result lands on its button; a finished `$(...)` assignment is
    /// applied.
    async fn on_exec(&mut self, event: actions::ExecEvent) {
        match event {
            actions::ExecEvent::Assignment(completed) => {
                apply_completed_assignment(completed, &self.variables, &mut self.runner, self.log)
                    .await;
            }
            event => self.runner.handle_exec_event(event).await,
        }
    }

    /// A refresh tick: redraws that one button.
    async fn on_refresh(&mut self, key: u8) {
        self.log
            .debug(Subsystem::Scene, format!("refresh tick for button {key}"));
        if let Err(error) = self.runner.refresh_button(key).await {
            warn_unless_disconnected(
                self.log,
                &self.runner,
                format!("failed to refresh button {key}: {error}"),
            );
        }
    }

    /// Forgets the press state of a lost connection: a control held down when the
    /// device vanished never reports its release, and pending short-press
    /// confirmations belong to the old connection.
    fn reset_after_disconnect(&mut self) {
        self.down_controls.clear();
        for (_, pending) in self.pending_shorts.drain() {
            pending.alive.store(false, Ordering::SeqCst);
            pending.handle.abort();
        }
        self.click_detector = ClickDetector::new(&self.defaults);
    }

    /// The input loop of one connection: hands every report from `reader` and every
    /// channel event to its `on_*` handler until `stop` fires or a channel closes
    /// ([`SessionEnd::Quit`]), or the connection is lost - the reader fails, or a draw
    /// noticed `device` is gone ([`SessionEnd::Disconnected`]).
    async fn run_connection<C: ButtonDevice>(
        &mut self,
        reader: &impl dak::input::InputSource,
        channels: &mut SessionChannels,
        stop: &StopSignal,
        device: &SwappableDevice<C>,
    ) -> SessionEnd {
        loop {
            tokio::select! {
                report = reader.read_report() => match report {
                    Ok(data) => self.on_report(&data).await,
                    Err(error) => return SessionEnd::Disconnected(error.to_string()),
                },
                actions = channels.timer_rx.recv() => match actions {
                    Some(actions) => self.on_timer(actions).await,
                    None => return SessionEnd::Quit,
                },
                click = channels.click_rx.recv() => match click {
                    Some((reference, event)) => self.on_click(reference, event).await,
                    None => return SessionEnd::Quit,
                },
                event = channels.exec_rx.recv() => match event {
                    Some(event) => self.on_exec(event).await,
                    None => return SessionEnd::Quit,
                },
                key = channels.refresh_rx.recv() => match key {
                    Some(key) => self.on_refresh(key).await,
                    None => return SessionEnd::Quit,
                },
                // A stop signal (SIGINT/SIGTERM, see `control`): the cleanup and
                // shutdown in `run_device` still run.
                _ = stop.stopped() => return SessionEnd::Quit,
                // A draw/flush noticed the device is gone before the reader did.
                _ = device.disconnected() => {
                    return SessionEnd::Disconnected("device stopped responding".to_string());
                }
            }
        }
    }
}

/// Warns about a failed device write, unless the device is known to be gone, in which
/// case the failure is expected (every write fails until it reconnects and is fully
/// repainted) and only shows up as a `device` debug line.
fn warn_unless_disconnected<D: ButtonDevice>(
    log: Log,
    runner: &actions::SceneRunner<'_, D>,
    message: String,
) {
    if runner.is_connected() {
        log.warn(message);
    } else {
        log.debug(
            Subsystem::Device,
            format!("{message} (device disconnected)"),
        );
    }
}

/// How one connection's input loop in [`run_device`] ended.
enum SessionEnd {
    /// Stop the program: a stop signal, or one of the event channels closed.
    Quit,
    /// The device went away; carries how that was noticed, for the warning.
    Disconnected(String),
}

/// Result of waiting for a lost device in [`await_reconnect`].
#[derive(Debug, PartialEq)]
enum Reconnect {
    /// A new connection is attached and the screen was repainted.
    Reconnected,
    /// A stop signal arrived while waiting (or while parked after giving up); the
    /// session should end.
    Cancelled,
}

/// Connects to `device_info` with the key/encoder counts from `definition` and
/// initializes it the way every session starts: both keypress/encoder states reported,
/// the given brightnesses applied, and every button image cleared.
async fn connect_device(
    device_info: &HidDeviceInfo,
    definition: &Mapping,
    protocol_version: usize,
    button_brightness: u8,
    encoder_brightness: u8,
) -> Result<Device, MirajazzError> {
    let device = Device::connect(
        device_info,
        protocol_version,
        definition.key_count as usize,
        definition.encoder_count as usize,
    )
    .await?;
    let device = device.with_supports_both_keypress_states(true);
    let device = device.with_supports_both_encoder_states(true);
    device.set_brightness(button_brightness).await?;
    // Not verified to have any visible effect: see `Defaults::encoder_brightness`'s
    // doc comment for why (no unit with functioning encoder LEDs was available to
    // confirm this against). Sent unconditionally anyway, same as `set_brightness`
    // above, since it costs nothing when the device has no encoders or LEDs.
    device.set_led_brightness(encoder_brightness).await?;
    device.clear_all_button_images().await?;
    Ok(device)
}

/// The current runtime brightness defaults (`$defaults.button_brightness`/
/// `$defaults.encoder_brightness`, possibly changed since startup), clamped to the
/// device's 0-100 range, so a reconnect restores what the user last set.
fn current_brightness(variables: &std::sync::Mutex<Variables>) -> (u8, u8) {
    let state = variables.lock().expect("variables mutex poisoned");
    let clamp = |value: i32| value.clamp(0, 100) as u8;
    (
        clamp(state.button_brightness()),
        clamp(state.encoder_brightness()),
    )
}

/// Handles a lost device: detaches the old connection, warns once (always shown),
/// then tries to rediscover and reopen the device matching `definition` right away and
/// every `policy.interval` after that, until it opens, `stop` fires, or
/// `policy.max_attempts` (when nonzero) attempts have failed - in which case an
/// always-shown error says so and the caller stops driving this device.
///
/// On success the fresh connection gets the *current* brightness defaults, is swapped
/// in under `runner`, the whole pre-disconnect screen is repainted with
/// [`actions::SceneRunner::redraw_all`], and an always-shown "reconnected" line is
/// printed. Scene, timer and variable state is untouched throughout, so input simply
/// carries on where it left off.
#[allow(clippy::too_many_arguments)]
async fn await_reconnect<R: Reopen>(
    device_number: u8,
    reopen: &R,
    original_name: &str,
    device: &SwappableDevice<R::Connection>,
    runner: &mut actions::SceneRunner<'_, SwappableDevice<R::Connection>>,
    variables: &std::sync::Mutex<Variables>,
    log: Log,
    reason: &str,
    policy: reconnect::ReconnectPolicy,
    stop: &StopSignal,
    park: &Park,
) -> Reconnect {
    device.mark_disconnected();
    log.warn(reconnect::disconnected_message(device_number, reason));
    let attempt = |number: u64| async move {
        let of = if policy.max_attempts == 0 {
            format!("attempt {number}")
        } else {
            format!("attempt {number}/{}", policy.max_attempts)
        };
        let (button_brightness, encoder_brightness) = current_brightness(variables);
        match reopen.reopen(button_brightness, encoder_brightness).await {
            Ok(Some(reopened)) => Some(reopened),
            Ok(None) => {
                log.debug(
                    Subsystem::Device,
                    format!("device #{device_number}: not back yet ({of})"),
                );
                None
            }
            Err(ReopenError::Discovery(error)) => {
                log.debug(
                    Subsystem::Device,
                    format!("device #{device_number}: discovery failed ({of}): {error}"),
                );
                None
            }
            Err(ReopenError::Connect(error)) => {
                // Typically the device is enumerated but not ready to open yet.
                log.debug(
                    Subsystem::Device,
                    format!("device #{device_number}: reconnect {of} failed: {error}"),
                );
                None
            }
        }
    };
    let reopened = loop {
        match reconnect::wait_until(policy, attempt, stop.stopped()).await {
            reconnect::WaitOutcome::Found(found) => break found,
            reconnect::WaitOutcome::Cancelled => return Reconnect::Cancelled,
            reconnect::WaitOutcome::GaveUp => {
                log.error(format!(
                    "{}; send SIGUSR1 to look for it again",
                    reconnect::gave_up_message(device_number, policy.max_attempts)
                ));
                // Parked: another dak may have the keypad meanwhile. A rescan starts a
                // fresh round of attempts, once the lock is ours again.
                if !park_until_rescan(park, stop, log).await {
                    return Reconnect::Cancelled;
                }
            }
        }
    };
    let name = if reopened.name.is_empty() {
        original_name
    } else {
        &reopened.name
    };
    let (name, serial) = (name.to_string(), reopened.serial);
    device.replace(reopened.connection);
    log.info(reconnect::reconnected_message(
        device_number,
        &name,
        &serial,
    ));
    if let Err(error) = runner.redraw_all().await {
        log.warn(format!(
            "device #{device_number}: failed to repaint after reconnect: {error}"
        ));
    }
    Reconnect::Reconnected
}

/// A connection [`Reopen::reopen`] opened, with what the reconnect message names.
struct Reopened<C> {
    /// The fresh, initialized connection.
    connection: C,
    /// The device name the rediscovered device reports (may be empty).
    name: String,
    /// Its serial number, as the connection reports it.
    serial: String,
}

/// Why one reopen attempt failed (both are retried; they only log differently).
#[derive(Debug)]
enum ReopenError {
    /// Listing the attached devices failed.
    Discovery(String),
    /// The device is back but could not be opened (yet).
    Connect(String),
}

/// One attempt at getting a lost device back, for [`await_reconnect`]: the real
/// implementation ([`ReopenDevice`]) rediscovers the keypad and connects to it; tests
/// script the outcomes.
#[allow(async_fn_in_trait)]
trait Reopen: Sync {
    /// The connection type the device is driven through.
    type Connection: ButtonDevice;

    /// Looks for the device once and opens it with the given brightnesses: `Ok(None)`
    /// when it is not back yet.
    async fn reopen(
        &self,
        button_brightness: u8,
        encoder_brightness: u8,
    ) -> Result<Option<Reopened<Self::Connection>>, ReopenError>;
}

/// Reopens a real keypad: the first discovered device matching `definition`, connected
/// like at startup (see [`connect_device`]).
struct ReopenDevice<'a> {
    /// The lost device's config definition.
    definition: &'a Mapping,
    /// The protocol version it was connected with.
    protocol_version: usize,
}

impl Reopen for ReopenDevice<'_> {
    type Connection = Device;

    async fn reopen(
        &self,
        button_brightness: u8,
        encoder_brightness: u8,
    ) -> Result<Option<Reopened<Device>>, ReopenError> {
        let devices = list_devices(&hardware::QUERIES)
            .await
            .map_err(|error| ReopenError::Discovery(error.to_string()))?;
        let Some(info) = devices.into_iter().find(|dev| {
            actions::discovered_device_matches(
                self.definition,
                &dev.serial_number,
                dev.vendor_id,
                dev.product_id,
            )
        }) else {
            return Ok(None);
        };
        let connection = connect_device(
            &info,
            self.definition,
            self.protocol_version,
            button_brightness,
            encoder_brightness,
        )
        .await
        .map_err(|error| ReopenError::Connect(error.to_string()))?;
        let serial = connection.serial_number().clone();
        Ok(Some(Reopened {
            connection,
            name: info.name.clone(),
            serial,
        }))
    }
}

/// Parks a device that gave up reconnecting: releases its lock, tells the supervisor,
/// and waits for a rescan request, then takes the lock back (per the conflict policy;
/// a device taken over by another dak meanwhile stays parked). Returns `false` when
/// `stop` fired instead.
async fn park_until_rescan(park: &Park, stop: &StopSignal, log: Log) -> bool {
    loop {
        let seen = park.controller.state().rescan;
        park.locks.release(&park.key);
        let _ = park.events.send(TaskEvent::Parked(park.number));
        tokio::select! {
            _ = park.controller.rescan_after(seen) => {}
            _ = stop.stopped() => return false,
        }
        let _ = park.events.send(TaskEvent::Resumed(park.number));
        log.info(format!("device #{}: looking for it again", park.number));
        match park
            .locks
            .ensure(&park.lock_dir, &park.key, park.conflict, stop, log, None)
            .await
        {
            Ok(()) => return true,
            Err(LockError::Cancelled) => return false,
            // Logged by `ensure`; wait for the next rescan.
            Err(_) => continue,
        }
    }
}

/// A delayed short-press confirmation for one pressable control (a button or a
/// pushed encoder).
///
/// The task sleeps `double_click_gap` after the release, then sends the short press
/// event unless `alive` was flipped first — which happens when the double click's
/// second press lands inside the window. The abort is a best-effort second line of
/// defence; the flag is what makes the cancellation authoritative.
struct PendingShortPress {
    /// False once the click turned out to be a double click's first half.
    alive: Arc<AtomicBool>,
    /// The sleeping confirmation task; dropped when it finishes on its own or aborted
    /// when a double click cancels it.
    handle: tokio::task::JoinHandle<()>,
}

/// Resolves the actions `reference` has bound to `event` (e.g. `pressed`,
/// `released` or `turn_cw`) and runs them in order. Unbound references
/// simply do nothing.
///
/// The actions are looked up in the current scene first, then in the previously active
/// scene (see `actions::action_for_event`), so pushed buttons keep their released
/// behavior after a scene switch.
///
/// The parameter list mirrors `run_action`: each device has exactly one input loop and
/// the shared state it needs is passed flat rather than bundled into a context type.
#[allow(clippy::too_many_arguments)]
async fn run_bound_action<D: actions::ButtonDevice>(
    log: Log,
    runner: &mut actions::SceneRunner<'_, D>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    reference: &Reference,
    event: &str,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
    variables: &Arc<std::sync::Mutex<Variables>>,
) {
    let actions = actions::action_for_event(
        current_scene,
        previous_scene.as_deref(),
        reference,
        event,
        scenes,
    );
    if actions.is_empty() {
        return;
    }

    log.debug(
        Subsystem::Actions,
        format!("{reference} {event} -> {actions:?}"),
    );

    run_actions(
        log,
        runner,
        current_scene,
        previous_scene,
        scenes,
        &actions,
        timer_handle,
        timer_tx,
        variables,
    )
    .await;
}

/// Feeds one press or release edge of a pressable control (a keypad button or a
/// pushed encoder) into the input handling: tracks the down state, runs the
/// `pressed`/`released` edge actions, and drives the shared [`ClickDetector`]
/// for `short_press` / `long_press` / `double_click`.
///
/// All pressable controls share one detector and one pending-short registry keyed
/// by [`Reference`], so encoder pushes behave exactly like buttons — including
/// double-click detection across the whole keypad.
///
/// The parameter list is deliberately kept flat over bundling the shared state into one
/// struct, mirroring `run_action` and `run_bound_action`.
#[allow(clippy::too_many_arguments)]
async fn run_pressable_edge<D: actions::ButtonDevice>(
    log: Log,
    runner: &mut actions::SceneRunner<'_, D>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    reference: &Reference,
    pressed: bool,
    down_controls: &mut std::collections::HashSet<Reference>,
    click_detector: &mut ClickDetector,
    click_tx: &mpsc::Sender<(Reference, ClickEvent)>,
    pending_shorts: &mut HashMap<Reference, PendingShortPress>,
    defaults: &Defaults,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
    variables: &Arc<std::sync::Mutex<Variables>>,
) {
    if pressed && !down_controls.contains(reference) {
        down_controls.insert(*reference);

        // A press inside the double-click gap turns the click into the
        // second half of a double click: cancel the first click's pending
        // short-press confirmation so it cannot fire as a short too.
        if let PressDecision::Double = click_detector.press(Instant::now()) {
            if let Some(pending) = pending_shorts.remove(reference) {
                pending.alive.store(false, Ordering::Relaxed);
                pending.handle.abort();
            }
        }

        run_bound_action(
            log,
            runner,
            current_scene,
            previous_scene,
            scenes,
            reference,
            "pressed",
            timer_handle,
            timer_tx,
            variables,
        )
        .await;
    } else if !pressed && down_controls.contains(reference) {
        down_controls.remove(reference);
        let release_time = Instant::now();

        run_bound_action(
            log,
            runner,
            current_scene,
            previous_scene,
            scenes,
            reference,
            "released",
            timer_handle,
            timer_tx,
            variables,
        )
        .await;

        match click_detector.release(release_time) {
            ReleaseDecision::Double => {
                log.debug(
                    Subsystem::Device,
                    format!("{reference} double click detected"),
                );
                run_bound_action(
                    log,
                    runner,
                    current_scene,
                    previous_scene,
                    scenes,
                    reference,
                    "double_click",
                    timer_handle,
                    timer_tx,
                    variables,
                )
                .await;
            }
            ReleaseDecision::Long => {
                log.debug(
                    Subsystem::Device,
                    format!("{reference} long press detected"),
                );
                run_bound_action(
                    log,
                    runner,
                    current_scene,
                    previous_scene,
                    scenes,
                    reference,
                    "long_press",
                    timer_handle,
                    timer_tx,
                    variables,
                )
                .await;
            }
            ReleaseDecision::Short => {
                // The short press only fires once the double-click gap has
                // passed without a second press; a second press inside the
                // window flips the flag and aborts this task.
                let alive = Arc::new(AtomicBool::new(true));
                let task_alive = alive.clone();
                let tx = click_tx.clone();
                let pending_reference = *reference;
                let gap = defaults.double_click_gap;
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(gap).await;
                    if task_alive.load(Ordering::Relaxed) {
                        let _ = tx.send((pending_reference, ClickEvent::ShortPress)).await;
                    }
                });
                pending_shorts.insert(pending_reference, PendingShortPress { alive, handle });
            }
        }
    }
}

/// Executes a scene action: stays (re-applying the current scene), switches scene,
/// or runs a command on its own background task.
///
/// Switching scenes re-arms the new scene's timer and remembers the old scene, so its
/// button actions remain available through inheritance; the bare `@` "stay" action
/// re-applies the current scene and re-arms its timer so periodic updates (e.g. a
/// clock) keep refreshing. Scene changes run through `runner`, which also owns the device.
///
/// The parameter list is deliberately kept flat over bundling the shared state into one
/// struct: each device has exactly one input loop, so a context type adds indirection
/// without removing any call sites.
#[allow(clippy::too_many_arguments)]
async fn run_action<D: actions::ButtonDevice>(
    log: Log,
    runner: &mut actions::SceneRunner<'_, D>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    action_value: &str,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
    variables: &Arc<std::sync::Mutex<Variables>>,
) {
    // References are expanded for every action except an assignment (whose left-hand
    // side is a target, not a value); an assignment's own right-hand side is resolved
    // when it is applied.
    let resolved = {
        let state = variables.lock().expect("variables mutex poisoned");
        actions::resolve_action(action_value, &state)
    };
    let resolved = match resolved {
        Ok(action) => action,
        Err(error) => {
            log.error(format!("action \"{action_value}\": {error}"));
            return;
        }
    };
    match resolved {
        Action::Stay => {
            log.debug(
                Subsystem::Actions,
                format!("stay on scene \"{current_scene}\" (re-applies it)"),
            );
            if let Err(error) = runner.enter_scene(&*current_scene, scenes).await {
                warn_unless_disconnected(
                    log,
                    runner,
                    format!("failed to refresh scene \"{current_scene}\": {error}"),
                );
            }
            rearm_scene_timer(
                &*current_scene,
                scenes,
                timer_handle,
                timer_tx,
                log,
                variables,
            )
            .await;
        }
        Action::Command { command } => {
            // Commands run on their own task so a running program never blocks the
            // device input loop or the scene timer, and they are not awaited inline.
            log.debug(Subsystem::Actions, format!("run command \"{command}\""));
            match actions::build_command(&command) {
                Ok(spec) => {
                    let display = spec.display();
                    tokio::spawn(async move {
                        match actions::run_action_command(spec).await {
                            Ok(()) => log.debug(
                                Subsystem::Actions,
                                format!("command \"{display}\" finished"),
                            ),
                            Err(error) => {
                                log.error(format!("command \"{display}\" failed: {error}"))
                            }
                        }
                    });
                }
                Err(error) => log.warn(format!("could not run command \"{command}\": {error}")),
            }
        }
        Action::SwitchScene { scene } => {
            log.debug(Subsystem::Actions, format!("switch to scene \"{scene}\""));
            log.debug(
                Subsystem::Scene,
                format!("Leaving scene \"{current_scene}\""),
            );
            *previous_scene = Some(current_scene.clone());
            *current_scene = scene.clone();
            if let Err(error) = runner.enter_scene(&scene, scenes).await {
                warn_unless_disconnected(
                    log,
                    runner,
                    format!("failed to enter scene \"{scene}\": {error}"),
                );
            }
            rearm_scene_timer(
                &*current_scene,
                scenes,
                timer_handle,
                timer_tx,
                log,
                variables,
            )
            .await;
        }
        Action::Assign { target, op, rhs } => {
            if let actions::AssignRhs::Command(inner) = &rhs {
                log.debug(
                    Subsystem::Actions,
                    format!("assign from command \"{inner}\""),
                );
                // One its own task so the program never blocks the input loop and
                // sibling actions run in parallel; the result arrives via the exec
                // channel and is applied by the loop below.
                actions::start_command_assignment(
                    target,
                    op,
                    inner,
                    variables,
                    runner.tracker.sender(),
                    log,
                );
                return;
            }
            let label = match &target {
                actions::AssignTarget::Variable(name) => format!("${name}"),
                actions::AssignTarget::Default(param) => param.path().to_string(),
            };
            log.debug(Subsystem::Actions, format!("assign {label}"));
            // A literal was already range-checked (and warned about) at load time; a
            // variable's value is only known now, so report clamping it here.
            let warn = !matches!(rhs, actions::AssignRhs::Int(_) | actions::AssignRhs::Str(_));
            let side_effect = {
                let mut state = variables.lock().expect("variables mutex poisoned");
                actions::apply_assignment(&target, op, &rhs, &mut state, warn, log)
            };
            if let Some(effect) = side_effect {
                match effect {
                    actions::DefaultEffect::Brightness(param, value) => {
                        let result = match param {
                            actions::SettableDefault::ButtonBrightness => {
                                runner.set_button_brightness(value as u8).await
                            }
                            actions::SettableDefault::EncoderBrightness => {
                                runner.set_encoder_brightness(value as u8).await
                            }
                            _ => unreachable!("brightness effect on a colour parameter"),
                        };
                        if let Err(error) = result {
                            warn_unless_disconnected(
                                log,
                                runner,
                                format!("failed to set {}: {error}", param.path()),
                            );
                        }
                    }
                    actions::DefaultEffect::Color(param, colour) => {
                        apply_colour_default(runner, param, colour);
                    }
                }
            }
        }
    }
}

/// Applies a finished command-substitution assignment: stores the converted value and,
/// for a writable default, pushes the new brightness or colour to the device.
///
/// Called by the input loop, which owns both the shared variable state and the device;
/// the spawned command task only reports the already-converted result.
async fn apply_completed_assignment<D: actions::ButtonDevice>(
    completed: actions::CompletedAssign,
    variables: &Arc<std::sync::Mutex<Variables>>,
    runner: &mut actions::SceneRunner<'_, D>,
    log: Log,
) {
    let value = match completed.outcome {
        Ok(value) => value,
        Err(error) => {
            log.error(error);
            return;
        }
    };

    let side_effect = match &completed.target {
        actions::AssignTarget::Variable(name) => {
            variables
                .lock()
                .expect("variables mutex poisoned")
                .store_mut()
                .set(name, value);
            None
        }
        actions::AssignTarget::Default(param) => {
            if param.is_colour() {
                let VarValue::Str(text) = value else {
                    log.error(format!(
                        "assignment to {} produced a non-colour value",
                        param.path()
                    ));
                    return;
                };
                let colour = match Color::parse(&text) {
                    Ok(colour) => colour,
                    Err(error) => {
                        log.error(format!("assignment to {}: {error}", param.path()));
                        return;
                    }
                };
                let mut state = variables.lock().expect("variables mutex poisoned");
                match param {
                    actions::SettableDefault::Background => state.set_background(colour.clone()),
                    actions::SettableDefault::TextColor => state.set_text_color(colour.clone()),
                    _ => unreachable!("colour parameter expected"),
                }
                Some(actions::DefaultEffect::Color(*param, colour))
            } else {
                let VarValue::Int(number) = value else {
                    log.error(format!(
                        "assignment to {} produced a non-numeric value",
                        param.path()
                    ));
                    return;
                };
                let mut state = variables.lock().expect("variables mutex poisoned");
                match param {
                    actions::SettableDefault::ButtonBrightness => {
                        state.set_button_brightness(number)
                    }
                    actions::SettableDefault::EncoderBrightness => {
                        state.set_encoder_brightness(number)
                    }
                    _ => unreachable!("numeric parameter expected"),
                }
                Some(actions::DefaultEffect::Brightness(*param, number))
            }
        }
    };

    if let Some(effect) = side_effect {
        match effect {
            actions::DefaultEffect::Brightness(param, number) => {
                let result = match param {
                    actions::SettableDefault::ButtonBrightness => {
                        runner.set_button_brightness(number as u8).await
                    }
                    actions::SettableDefault::EncoderBrightness => {
                        runner.set_encoder_brightness(number as u8).await
                    }
                    _ => unreachable!("brightness effect on a colour parameter"),
                };
                if let Err(error) = result {
                    warn_unless_disconnected(
                        log,
                        runner,
                        format!("failed to set {}: {error}", param.path()),
                    );
                }
            }
            actions::DefaultEffect::Color(param, colour) => {
                apply_colour_default(runner, param, colour);
            }
        }
    }
}

/// Pushes a newly assigned colour default to `runner`, which uses it on the next draw.
///
/// A `SettableDefault` is always either a brightness or a colour parameter, so this is
/// only called with the two colour variants.
fn apply_colour_default<D: actions::ButtonDevice>(
    runner: &mut actions::SceneRunner<'_, D>,
    param: actions::SettableDefault,
    colour: Color,
) {
    match param {
        actions::SettableDefault::Background => runner.set_background(colour),
        actions::SettableDefault::TextColor => runner.set_text_color(colour),
        other => unreachable!("not a colour parameter: {}", other.path()),
    }
}

/// Runs each action in `actions`, in order, exactly like calling [`run_action`] once per
/// entry. Shared by [`run_bound_action`] (a resolved event's actions) and the timer
/// branch of `run_device`'s select loop (a fired timer's actions) - the only two places
/// an action value ever resolves to more than one action to run.
///
/// Commands run without waiting on each other (each is spawned onto its own task by
/// `run_action` and never awaited inline); at most one entry may be a scene-changing
/// action (`@`/`@scene`), enforced at config-load time, so there is never a second
/// `enter_scene` call competing with this loop's own scene-mutating state.
#[allow(clippy::too_many_arguments)]
async fn run_actions<D: actions::ButtonDevice>(
    log: Log,
    runner: &mut actions::SceneRunner<'_, D>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    actions: &[&str],
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
    variables: &Arc<std::sync::Mutex<Variables>>,
) {
    for action in actions {
        run_action(
            log,
            runner,
            current_scene,
            previous_scene,
            scenes,
            action,
            timer_handle,
            timer_tx,
            variables,
        )
        .await;
    }
}

/// Starts (or restarts) the current scene's timer, aborting any previous one.
///
/// The timer read from the scene's `actions.timer` fires after its number of seconds
/// and delivers its action values through `timer_tx`. Returns the new task handle.
async fn arm_scene_timer(
    scene_name: &str,
    scenes: &Value,
    timer_tx: &mpsc::Sender<Vec<String>>,
    log: Log,
    variables: &Arc<std::sync::Mutex<Variables>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let timer = {
        let state = variables.lock().expect("variables mutex poisoned");
        actions::timer_for_scene_with(scene_name, scenes, &state)
    };
    match timer {
        Ok(Some((seconds, actions))) => {
            log.debug(
                Subsystem::Scene,
                format!("armed timer for scene \"{scene_name}\": {seconds}s -> {actions:?}"),
            );
            let tx = timer_tx.clone();
            let actions: Vec<String> = actions.into_iter().map(str::to_string).collect();
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                let _ = tx.send(actions).await;
            }))
        }
        Ok(None) => None,
        Err(error) => {
            log.error(error);
            None
        }
    }
}

/// Aborts any running timer task and arms the timer for `scene_name`, if it defines one.
async fn rearm_scene_timer(
    scene_name: &str,
    scenes: &Value,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
    log: Log,
    variables: &Arc<std::sync::Mutex<Variables>>,
) {
    if let Some(handle) = timer_handle.take() {
        handle.abort();
    }
    *timer_handle = arm_scene_timer(scene_name, scenes, timer_tx, log, variables).await;
}

/// Builds the `-d device` lines printed when a device is found, one per
/// identifying detail: VID:PID, the OS device id (on Linux the `/dev/hidrawN`
/// path), the device name and the serial number reported by the USB stack.
///
/// The id is passed as a `Debug` value because its concrete type (`DeviceId`,
/// behind `HidDeviceInfo.id`) is platform-specific and not re-exported by
/// mirajazz; `Debug`-formatting it in the caller keeps this helper constructible
/// in tests on any platform. A device without a serial is reported as "unknown"
/// rather than crashing the connect line.
fn device_info_lines(
    id: &dyn std::fmt::Debug,
    serial: &Option<String>,
    vid: u16,
    pid: u16,
    name: &str,
) -> Vec<String> {
    let serial = serial.as_deref().unwrap_or("unknown");
    vec![
        format!("device id: {vid:04X}:{pid:04X}"),
        format!("device path: {id:?}"),
        format!("device name: {name}"),
        format!("device serial: {serial}"),
    ]
}

/// One-line summary of a detected device, used e.g. when a discovered device has no
/// matching config definition.
///
/// Like [`device_info_lines`], the id is passed as a `Debug` value because its concrete
/// type is platform-specific and not re-exported by mirajazz.
fn device_summary_line(
    id: &dyn std::fmt::Debug,
    serial: &Option<String>,
    vid: u16,
    pid: u16,
    name: &str,
) -> String {
    let serial = serial.as_deref().unwrap_or("unknown");
    format!("{vid:04X}:{pid:04X} path {id:?} serial {serial} \"{name}\"")
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Mutex;
    use std::time::Duration;

    use clap::Parser;
    use image::{DynamicImage, Rgb, RgbImage};
    use mirajazz::types::ImageFormat;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use dak::actions::{AssignTarget, ButtonDevice, CompletedAssign, SceneRunner, SettableDefault};
    use dak::cli::Cli;
    use dak::hardware;
    use dak::log::Log;

    use super::{ClickDetector, ClickEvent, Defaults, Reference, VarValue, Variables};

    /// A tiny recording keypad for the dispatch-layer tests: scene `setup` operations
    /// that reach the device are recorded so a test can see which scene was applied.
    #[derive(Default)]
    struct MockButtonDevice {
        calls: Mutex<Vec<&'static str>>,
        /// The last value passed to `set_brightness`, if any.
        last_button_brightness: Mutex<Option<u8>>,
        /// The last value passed to `set_led_brightness`, if any.
        last_encoder_brightness: Mutex<Option<u8>>,
        /// When set, `set_brightness`/`set_led_brightness` fail instead of
        /// succeeding, for exercising `run_action`'s `SetConfig` failure branch.
        fail_brightness: AtomicBool,
    }

    /// [`MockButtonDevice`]'s error type: a fixed message, only ever produced when a
    /// test has explicitly armed a failure via `fail_brightness_calls`.
    #[derive(Debug)]
    struct MockWriteError(&'static str);

    impl std::fmt::Display for MockWriteError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for MockWriteError {}

    impl MockButtonDevice {
        /// Every `clear`/`flush`/`set`/`set_brightness`/`set_led_brightness` the runner
        /// attempted, in order.
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        /// The last value passed to `set_brightness`, if any.
        fn last_button_brightness(&self) -> Option<u8> {
            *self.last_button_brightness.lock().unwrap()
        }

        /// The last value passed to `set_led_brightness`, if any.
        fn last_encoder_brightness(&self) -> Option<u8> {
            *self.last_encoder_brightness.lock().unwrap()
        }

        /// Makes every future `set_brightness`/`set_led_brightness` call fail.
        fn fail_brightness_calls(&self, yes: bool) {
            self.fail_brightness
                .store(yes, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl ButtonDevice for MockButtonDevice {
        type Error = MockWriteError;

        async fn set_button_image(
            &self,
            _key: u8,
            _image_format: ImageFormat,
            _image: DynamicImage,
        ) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("set");
            Ok(())
        }

        async fn clear_button_image(&self, _key: u8) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("clear");
            Ok(())
        }

        async fn flush(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("flush");
            Ok(())
        }

        fn key_count(&self) -> u8 {
            9
        }

        async fn set_brightness(&self, percent: u8) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("set_brightness");
            if self
                .fail_brightness
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(MockWriteError("device brightness write refused"));
            }
            *self.last_button_brightness.lock().unwrap() = Some(percent);
            Ok(())
        }

        async fn set_led_brightness(&self, percent: u8) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("set_led_brightness");
            if self
                .fail_brightness
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(MockWriteError("device LED brightness write refused"));
            }
            *self.last_encoder_brightness.lock().unwrap() = Some(percent);
            Ok(())
        }
    }

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Writes a 2x2 test image to a unique temp file and returns its path, so a `type:
    /// "image"` setup entry has a real file to load in the dispatch-layer tests.
    fn write_temp_image() -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = format!("/tmp/dak_main_img_{}_{n}.png", std::process::id());
        let image = RgbImage::from_pixel(2, 2, Rgb([10, 20, 30]));
        image.save(&path).unwrap();
        PathBuf::from(path)
    }

    /// Builds a scene runner over a mock device, dropping the exec and refresh
    /// channels: none of these tests run `image_exec`/`text_exec` or rely on a
    /// scheduled refresh tick actually arriving.
    fn make_runner(mock: &MockButtonDevice) -> SceneRunner<'_, MockButtonDevice> {
        let (exec_tx, _exec_rx) = mpsc::channel(8);
        let (refresh_tx, _refresh_rx) = mpsc::channel(8);
        SceneRunner::new(
            1,
            mock,
            hardware::Kind::Akp03ERev2.image_format(),
            exec_tx,
            refresh_tx,
            Log::default(),
            &HashSet::new(),
            Defaults::default().background,
            Defaults::default().text_color,
        )
    }

    /// An empty shared variable state for tests that do not declare variables.
    fn test_variables() -> std::sync::Arc<std::sync::Mutex<Variables>> {
        std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
            std::collections::BTreeMap::new(),
            &Defaults::default(),
        )))
    }

    /// The shared state one device's input loop owns between events: the down-control
    /// set, the shared click detector and its pending short-press confirmations, and
    /// the timer plumbing. Mirrors the locals of `run_device`.
    struct EdgeState {
        down_controls: HashSet<Reference>,
        click_detector: ClickDetector,
        click_tx: mpsc::Sender<(Reference, ClickEvent)>,
        click_rx: mpsc::Receiver<(Reference, ClickEvent)>,
        pending_shorts: HashMap<Reference, super::PendingShortPress>,
        timer_handle: Option<tokio::task::JoinHandle<()>>,
        timer_tx: mpsc::Sender<Vec<String>>,
        defaults: Defaults,
        /// The shared variable/default state; most tests never assign, so it starts empty.
        variables: std::sync::Arc<std::sync::Mutex<Variables>>,
    }

    impl EdgeState {
        /// A fresh loop state with the given press-detection knobs.
        fn new(defaults: Defaults) -> EdgeState {
            let (click_tx, click_rx) = mpsc::channel(8);
            let (timer_tx, _timer_rx) = mpsc::channel(1);
            // Built before `defaults` is moved into the struct below.
            let click_detector = ClickDetector::new(&defaults);
            let variables = std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
                std::collections::BTreeMap::new(),
                &defaults,
            )));
            EdgeState {
                down_controls: HashSet::new(),
                click_detector,
                click_tx,
                click_rx,
                pending_shorts: HashMap::new(),
                timer_handle: None,
                timer_tx,
                defaults,
                variables,
            }
        }

        /// Waits (up to a second) for the delayed short-press confirmation the loop's
        /// release branch spawned, asserting it really arrives.
        async fn receive_click(&mut self) -> (Reference, ClickEvent) {
            tokio::time::timeout(Duration::from_millis(1000), self.click_rx.recv())
                .await
                .expect("a short-press confirmation should arrive")
                .expect("the click channel must not close")
        }

        /// Asserts no short-press confirmation arrives within `window`: used to prove
        /// a pending short was cancelled or never armed.
        async fn assert_no_click(&mut self, window: Duration) {
            let result = tokio::time::timeout(window, self.click_rx.recv()).await;
            assert!(
                result.is_err(),
                "expected no short-press confirmation: {result:?}"
            );
        }
    }

    /// Feeds one press/release edge through `run_pressable_edge` with the loop state,
    /// exactly like the `data` branch of `run_device` does.
    async fn press_edge<D: ButtonDevice>(
        runner: &mut SceneRunner<'_, D>,
        scenes: &Value,
        current_scene: &mut String,
        previous_scene: &mut Option<String>,
        reference: Reference,
        pressed: bool,
        state: &mut EdgeState,
    ) {
        super::run_pressable_edge(
            Log::default(),
            runner,
            current_scene,
            previous_scene,
            scenes,
            &reference,
            pressed,
            &mut state.down_controls,
            &mut state.click_detector,
            &state.click_tx,
            &mut state.pending_shorts,
            &state.defaults,
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
    }

    /// The five event bindings per pressable reference, each pointing at its own scene
    /// so a test can tell which event fired by which scene became current.
    fn pressable_bindings() -> Value {
        json!({
            "pressed": "@P",
            "released": "@R",
            "short_press": "@S",
            "long_press": "@L",
            "double_click": "@D"
        })
    }

    /// Scenes where `reference` (`"1b01"` or `"1e01"`) binds all five press events in
    /// `on_start` and to the two scenes that can be current or previous mid-sequence,
    /// so every lookup resolves within the two-scene chain.
    fn pressable_scenes(reference: &str) -> Value {
        let mut scenes = serde_json::Map::new();
        for name in ["on_start", "P", "R"] {
            let mut key = serde_json::Map::new();
            key.insert(reference.to_string(), pressable_bindings());
            let mut scene = serde_json::Map::new();
            scene.insert("actions".to_string(), Value::Object(key));
            scenes.insert(name.to_string(), Value::Object(scene));
        }
        for name in ["S", "L", "D"] {
            let mut scene = serde_json::Map::new();
            scene.insert("actions".to_string(), Value::Object(serde_json::Map::new()));
            scenes.insert(name.to_string(), Value::Object(scene));
        }
        Value::Object(scenes)
    }

    /// A quick press/release: the double-click gap is short but the short-press
    /// threshold is high, so quick presses are single clicks.
    fn quickly_clicking_defaults() -> Defaults {
        Defaults {
            short_press_duration: Duration::from_millis(700),
            double_click_gap: Duration::from_millis(80),
            ..Defaults::default()
        }
    }

    /// A short press threshold, for turning a held press into a long press quickly.
    fn long_press_defaults() -> Defaults {
        Defaults {
            short_press_duration: Duration::from_millis(60),
            double_click_gap: Duration::from_millis(600),
            ..Defaults::default()
        }
    }

    /// The startup summary names connected and waiting devices, singular or plural.
    #[test]
    fn startup_summary_wording() {
        assert_eq!(super::startup_summary(1, 0), "1 device connected");
        assert_eq!(super::startup_summary(2, 0), "2 devices connected");
        assert_eq!(
            super::startup_summary(0, 1),
            "1 device waiting for another dak to release it"
        );
        assert_eq!(
            super::startup_summary(2, 1),
            "2 devices connected, 1 waiting for another dak"
        );
    }

    /// A parking fixture: a lock table holding the lock of a test key in a fresh
    /// directory, and the park itself with its event receiver.
    fn park_fixture(
        conflict: dak::lock::Conflict,
    ) -> (
        super::Park,
        tokio::sync::mpsc::UnboundedReceiver<super::TaskEvent>,
        std::path::PathBuf,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "dak_park_{}_{}",
            std::process::id(),
            PARK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dak::lock::DeviceKey::new(0x0300, 0x3002, Some("PARKTEST"), "");
        let locks = super::LockTable::default();
        locks.insert(&key, dak::lock::try_lock(&dir, &key).unwrap());
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        let park = super::Park {
            number: 1,
            key,
            locks,
            lock_dir: dir.clone(),
            conflict,
            controller: std::sync::Arc::new(dak::control::Controller::new()),
            events,
        };
        (park, rx, dir)
    }

    /// Distinguishes the parking fixtures' directories.
    static PARK_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A parked device releases its lock (another dak could take the keypad), and a
    /// rescan takes the lock back and resumes it.
    #[tokio::test]
    async fn parking_releases_the_lock_until_a_rescan() {
        let (park, mut events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let park = std::sync::Arc::new(park);
        let stop = dak::control::StopSource::new();
        let task = {
            let (park, signal) = (park.clone(), stop.signal());
            tokio::spawn(async move {
                super::park_until_rescan(&park, &signal, dak::log::Log::default()).await
            })
        };
        assert_eq!(events.recv().await, Some(super::TaskEvent::Parked(1)));
        assert!(!park.locks.holds(&park.key));
        assert!(
            lock_frees_up(&dir, &park.key),
            "the lock is free while parked"
        );

        assert!(rescan_until_done(&park, &mut events, task).await, "resumed");
        assert!(park.locks.holds(&park.key), "the lock is ours again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A device another dak took while it was parked stays parked (with the default
    /// policy) until a later rescan finds it free.
    #[tokio::test]
    async fn parked_device_taken_elsewhere_stays_parked() {
        let (park, mut events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let park = std::sync::Arc::new(park);
        let stop = dak::control::StopSource::new();
        let task = {
            let (park, signal) = (park.clone(), stop.signal());
            tokio::spawn(async move {
                super::park_until_rescan(&park, &signal, dak::log::Log::default()).await
            })
        };
        assert_eq!(events.recv().await, Some(super::TaskEvent::Parked(1)));
        let other = take_lock_eventually(&dir, &park.key);
        park.controller.handle(dak::control::SignalAction::Rescan);
        assert_eq!(events.recv().await, Some(super::TaskEvent::Resumed(1)));
        assert_eq!(
            events.recv().await,
            Some(super::TaskEvent::Parked(1)),
            "busy: parked again"
        );
        drop(other);
        assert!(rescan_until_done(&park, &mut events, task).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Requests rescans of a parked task until it ends, returning its result. A lock
    /// just released may look held for an instant (see [`lock_frees_up`]), making the
    /// task's one attempt fail and park it again, so one rescan is not always enough.
    async fn rescan_until_done(
        park: &super::Park,
        events: &mut tokio::sync::mpsc::UnboundedReceiver<super::TaskEvent>,
        mut task: tokio::task::JoinHandle<bool>,
    ) -> bool {
        for _ in 0..200 {
            park.controller.handle(dak::control::SignalAction::Rescan);
            assert_eq!(events.recv().await, Some(super::TaskEvent::Resumed(1)));
            tokio::select! {
                done = &mut task => return done.unwrap(),
                event = events.recv() => {
                    assert_eq!(event, Some(super::TaskEvent::Parked(1)));
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
        panic!("the parked task never got its lock back");
    }

    /// Takes the lock of `key` in `dir`, retrying for up to a second: a lock just
    /// released may look held for an instant (see [`lock_frees_up`]).
    fn take_lock_eventually(
        dir: &std::path::Path,
        key: &dak::lock::DeviceKey,
    ) -> dak::lock::DeviceLock {
        for _ in 0..200 {
            if let Ok(lock) = dak::lock::try_lock(dir, key) {
                return lock;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the lock never became free");
    }

    /// A stop ends the parking without taking the lock back.
    #[tokio::test]
    async fn parking_ends_on_stop() {
        let (park, mut events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let stop = dak::control::StopSource::new();
        let signal = stop.signal();
        let parked = super::park_until_rescan(&park, &signal, dak::log::Log::default());
        let stopper = async {
            assert_eq!(events.recv().await, Some(super::TaskEvent::Parked(1)));
            stop.stop();
        };
        let (resumed, ()) = tokio::join!(parked, stopper);
        assert!(!resumed);
        assert!(!park.locks.holds(&park.key));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether the lock of `key` in `dir` can be taken within a second. A lock just
    /// released may look held for an instant: another test forking a child at that
    /// moment gives the child a copy of the fd until its exec closes it (O_CLOEXEC).
    fn lock_frees_up(dir: &std::path::Path, key: &dak::lock::DeviceKey) -> bool {
        (0..200).any(|_| {
            let free = dak::lock::try_lock(dir, key).is_ok();
            if !free {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            free
        })
    }

    /// The lock table keeps only the locks it is told to keep.
    #[test]
    fn lock_table_retain_releases_the_rest() {
        let dir = std::env::temp_dir().join(format!("dak_table_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dak::lock::DeviceKey::new(1, 2, Some("A"), "");
        let b = dak::lock::DeviceKey::new(1, 2, Some("B"), "");
        let table = super::LockTable::default();
        table.insert(&a, dak::lock::try_lock(&dir, &a).unwrap());
        table.insert(&b, dak::lock::try_lock(&dir, &b).unwrap());
        table.retain(&[a.file_name()].into_iter().collect());
        assert!(table.holds(&a) && !table.holds(&b));
        assert!(lock_frees_up(&dir, &b), "b was released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A service with nothing to drive says how to get it going.
    #[test]
    fn startup_summary_without_devices_mentions_the_rescan() {
        assert_eq!(super::startup_summary(0, 0), super::NO_DEVICE_YET);
        assert!(super::NO_DEVICE_YET.contains("SIGUSR1"));
    }

    /// Detached or under systemd the program stays up without devices; in a terminal
    /// it does not.
    #[test]
    fn service_mode_detection() {
        assert!(!super::service_mode(false, false));
        assert!(super::service_mode(true, false));
        assert!(super::service_mode(false, true));
    }

    /// `CLOCK_MONOTONIC` only moves forward.
    #[test]
    fn monotonic_usec_increases() {
        let first = super::monotonic_usec();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(super::monotonic_usec() > first);
    }

    /// Only the first startup report is delivered.
    #[test]
    fn start_signal_reports_once() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let start = super::StartSignal::new(tx);
        start.report(super::Started::Waiting);
        start.report(super::Started::Connected);
        assert_eq!(rx.try_recv().unwrap(), super::Started::Waiting);
    }

    /// A device that was never found (or was given up on) maps to the "no device"
    /// status; every other device error is a generic failure.
    #[test]
    fn device_error_status_distinguishes_missing_devices() {
        use mirajazz::error::MirajazzError;
        assert_eq!(
            super::device_error_status(&MirajazzError::DeviceNotFoundError),
            dak::exit::NO_DEVICE
        );
        assert_eq!(
            super::device_error_status(&MirajazzError::BadData),
            dak::exit::FAILURE
        );
    }

    /// A relative config path becomes absolute (rooted in the current directory), and
    /// an absolute one is kept as it is.
    #[test]
    fn absolute_config_path_roots_relative_paths() {
        let relative = super::absolute_config_path(Path::new("dir/config.json"));
        assert!(relative.is_absolute());
        assert!(relative.ends_with("dir/config.json"));
        assert_eq!(
            super::absolute_config_path(Path::new("/etc/dak/config.json")),
            PathBuf::from("/etc/dak/config.json")
        );
    }

    /// With no arguments the config path is left unset (so the search order applies)
    /// and no debug subsystems are requested.
    #[test]
    fn cli_defaults_to_search_and_no_debug() {
        let cli = Cli::try_parse_from(["dak"]).unwrap();
        assert!(cli.config.is_none());
        assert!(cli.debug.is_empty());
    }

    /// Both the short `-c` and long `--config` forms set the config path verbatim.
    #[test]
    fn cli_config_flag_short_and_long_forms() {
        let short = Cli::try_parse_from(["dak", "-c", "a.json"]).unwrap();
        assert_eq!(short.config.as_deref(), Some(Path::new("a.json")));

        let long = Cli::try_parse_from(["dak", "--config", "b.json"]).unwrap();
        assert_eq!(long.config.as_deref(), Some(Path::new("b.json")));
    }

    /// A comma-separated `-d` value is split into individual debug subsystems.
    #[test]
    fn cli_debug_splits_comma_separated_values() {
        let cli = Cli::try_parse_from(["dak", "-d", "device,scene"]).unwrap();
        assert_eq!(cli.debug, vec!["device".to_string(), "scene".to_string()]);
    }

    /// Repeating `-d` accumulates values, and config and debug flags combine.
    #[test]
    fn cli_debug_accumulates_and_combines_with_config() {
        let cli =
            Cli::try_parse_from(["dak", "-d", "device", "-d", "action", "-c", "c.json"]).unwrap();
        assert_eq!(cli.debug, vec!["device".to_string(), "action".to_string()]);
        assert_eq!(cli.config.as_deref(), Some(Path::new("c.json")));
    }

    /// `--help` reports that help was requested without running the device code.
    #[test]
    fn cli_help_is_detected_as_help_request() {
        let result = Cli::try_parse_from(["dak", "--help"]);
        assert!(matches!(
            result,
            Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp
        ));
    }

    /// Unknown options are rejected by clap rather than silently ignored.
    #[test]
    fn cli_rejects_unknown_option() {
        assert!(Cli::try_parse_from(["dak", "--bogus"]).is_err());
    }

    /// `-c` requires a value and `-d` requires at least one value.
    #[test]
    fn cli_rejects_missing_flag_values() {
        assert!(Cli::try_parse_from(["dak", "-c"]).is_err());
        assert!(Cli::try_parse_from(["dak", "-d"]).is_err());
    }

    /// `--map` selects the mapping wizard; it can be combined with nothing
    /// else because the wizard ignores config and debug flags.
    #[test]
    fn cli_map_flag_selects_wizard() {
        let cli = Cli::try_parse_from(["dak", "--map"]).unwrap();
        assert!(cli.map);
    }

    /// Debug-prints like the real Linux `DeviceId::DevPath`, so the device-line
    /// assertions read the way the actual output does.
    struct FakeDeviceId(&'static str);

    impl std::fmt::Debug for FakeDeviceId {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "DevPath({:?})", self.0)
        }
    }

    /// The device lines report VID:PID, OS device path, name and serial, one each.
    #[test]
    fn device_info_lines_report_each_detail() {
        let lines = super::device_info_lines(
            &FakeDeviceId("/dev/hidraw3"),
            &Some("ABC123".to_string()),
            0x0300,
            0x3002,
            "Ajazz HOTSPOTEKUSB HID DEMO",
        );
        assert_eq!(
            lines,
            vec![
                "device id: 0300:3002".to_string(),
                "device path: DevPath(\"/dev/hidraw3\")".to_string(),
                "device name: Ajazz HOTSPOTEKUSB HID DEMO".to_string(),
                "device serial: ABC123".to_string(),
            ]
        );
    }

    /// A missing serial number is reported as "unknown" instead of panicking.
    #[test]
    fn device_info_lines_without_serial_report_unknown() {
        let lines = super::device_info_lines(
            &FakeDeviceId("/dev/hidraw0"),
            &None,
            0x0300,
            0x3002,
            "keypad",
        );
        assert_eq!(lines[3], "device serial: unknown");
        assert_eq!(lines[1], "device path: DevPath(\"/dev/hidraw0\")");
    }

    /// The one-line device summary carries VID:PID, path, serial and name so an
    /// unconfigured device can be told apart from the configured ones.
    #[test]
    fn device_summary_line_identifies_a_device() {
        let line = super::device_summary_line(
            &FakeDeviceId("/dev/hidraw3"),
            &Some("ABC123".to_string()),
            0x0300,
            0x3002,
            "Ajazz HOTSPOTEKUSB HID DEMO",
        );
        assert_eq!(
            line,
            r#"0300:3002 path DevPath("/dev/hidraw3") serial ABC123 "Ajazz HOTSPOTEKUSB HID DEMO""#
        );
        assert_eq!(
            super::device_summary_line(&FakeDeviceId("/dev/hidraw0"), &None, 0x0300, 0x3002, "k"),
            r#"0300:3002 path DevPath("/dev/hidraw0") serial unknown "k""#
        );
    }

    /// A scene without a timer arms no timer task; a scene with one returns a handle.
    #[tokio::test]
    async fn arm_scene_timer_returns_none_without_timer() {
        let scenes = json!({
            "on_start": { "actions": {} },
            "Main": { "actions": { "timer": { "1": "@Main" } } }
        });
        let (tx, _rx) = mpsc::channel(1);
        assert!(super::arm_scene_timer(
            "on_start",
            &scenes,
            &tx,
            Log::default(),
            &test_variables()
        )
        .await
        .is_none());
        assert!(
            super::arm_scene_timer("Main", &scenes, &tx, Log::default(), &test_variables())
                .await
                .is_some()
        );
    }

    /// An armed timer fires after its configured seconds and delivers the action
    /// through the channel, like a scene timer running during normal operation.
    #[tokio::test]
    async fn timer_fires_after_its_seconds_and_delivers_the_action() {
        let scenes = json!({
            "on_start": { "actions": { "timer": { "1": "@Main" } } }
        });
        let (tx, mut rx) = mpsc::channel(1);
        let handle =
            super::arm_scene_timer("on_start", &scenes, &tx, Log::default(), &test_variables())
                .await
                .expect("scene has a timer");
        handle.await.unwrap();
        assert_eq!(rx.recv().await, Some(vec!["@Main".to_string()]));
    }

    /// Re-arming for a scene without a timer aborts the running timer and clears the
    /// handle; the aborted timer never delivers its action.
    #[tokio::test]
    async fn rearm_scene_timer_aborts_the_old_timer() {
        let scenes = json!({
            "on_start": { "actions": {} },
            "Main": { "actions": { "timer": { "1": "@Main" } } }
        });
        let (tx, mut rx) = mpsc::channel(1);
        let mut handle =
            super::arm_scene_timer("Main", &scenes, &tx, Log::default(), &test_variables()).await;
        assert!(handle.is_some());

        super::rearm_scene_timer(
            "on_start",
            &scenes,
            &mut handle,
            &tx,
            Log::default(),
            &test_variables(),
        )
        .await;
        assert!(handle.is_none(), "a scene without a timer arms nothing");

        let late = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
        assert!(late.is_err(), "the aborted timer must not fire: {late:?}");
    }

    /// A bare `@` action re-applies the current scene: its setup runs on the device
    /// again and the scene name is untouched, with no previous scene recorded.
    #[tokio::test]
    async fn run_action_stay_reapplies_the_current_scene() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": {
                "setup": { "1b01": { "type": "clear" } },
                "actions": {}
            }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
        assert_eq!(
            mock.calls(),
            vec!["clear", "flush"],
            "re-applying on_start redraws its setup"
        );
    }

    /// A command action spawns (a valid `true` runs) while an unparseable command
    /// is reported and skipped; both leave the scene untouched.
    #[tokio::test]
    async fn run_action_runs_valid_command_and_skips_bad_one() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "true",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "/bin/echo 'unbalanced",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// An `@scene` action switches scenes: the old scene is remembered as previous and
    /// the target scene's setup is applied.
    #[tokio::test]
    async fn run_action_switch_scene_remembers_and_enters() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "Test": {
                "setup": { "1b02": { "type": "clear" } },
                "actions": {}
            }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Test",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "Test");
        assert_eq!(previous_scene.as_deref(), Some("on_start"));
        assert_eq!(mock.calls(), vec!["clear", "flush"]);
    }

    /// A `type: "image"` setup entry loads a real file and stages it on the device:
    /// the dispatch-layer mock's `set_button_image` is exercised, not just `clear`.
    #[tokio::test]
    async fn run_action_stay_applies_an_image_setup_entry() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let path = write_temp_image();
        let scenes = json!({
            "on_start": {
                "setup": { "1b01": { "type": "image", "params": path.to_str().unwrap() } },
                "actions": {}
            }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        let _ = std::fs::remove_file(&path);

        assert_eq!(mock.calls(), vec!["set", "flush"]);
    }

    /// A bare `@` action whose scene has a button that fails to draw (here, an `image`
    /// setup entry pointing at a missing file) leaves the scene name and
    /// `previous_scene` untouched: staying never counts as leaving. The failing button
    /// draws the red "Error" label instead of stopping the reapply.
    #[tokio::test]
    async fn run_action_stay_keeps_scene_when_reapply_fails() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": {
                "setup": { "1b01": { "type": "image", "params": "/no/such/image.png" } },
                "actions": {}
            }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            current_scene, "on_start",
            "a failed reapply must not change the scene name"
        );
        assert!(previous_scene.is_none());
        assert_eq!(
            mock.calls(),
            vec!["set", "flush", "flush"],
            "the failing operation draws the red \"Error\" label (staged + flushed by \
             draw_error_label) and the batch's own trailing flush still runs afterward"
        );
    }

    /// A `$defaults.button_brightness := N` action dispatches straight to the device's
    /// `set_brightness`, and leaves the current/previous scene untouched (it isn't a
    /// scene-changing action).
    #[tokio::test]
    async fn run_action_set_config_calls_set_brightness() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.button_brightness := 80",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(mock.last_button_brightness(), Some(80));
        assert_eq!(mock.last_encoder_brightness(), None);
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A `$defaults.encoder_brightness := N` action dispatches to `set_led_brightness`
    /// instead, distinctly from `button_brightness` above.
    #[tokio::test]
    async fn run_action_set_config_calls_set_led_brightness() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.encoder_brightness := 15",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(mock.last_encoder_brightness(), Some(15));
        assert_eq!(mock.last_button_brightness(), None);
    }

    /// A `$defaults.background := "colour"` action stores the new colour in the shared
    /// state and pushes it to the runner, distinct from the brightness parameters.
    #[tokio::test]
    async fn run_action_set_config_calls_set_background_colour() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.background := \"red\"",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        let variables = state.variables.lock().unwrap();
        assert_eq!(variables.background().text(), "red");
        assert_eq!(variables.background().channels(), [0xff, 0x00, 0x00]);
        assert_eq!(mock.last_button_brightness(), None);
        assert_eq!(mock.last_encoder_brightness(), None);
    }

    /// A `$defaults.text_color := "#00ff00"` action stores the new colour in the shared
    /// state, exercising the text-colour side of `apply_colour_default`.
    #[tokio::test]
    async fn run_action_set_config_calls_set_text_color() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.text_color := \"#00ff00\"",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        let variables = state.variables.lock().unwrap();
        assert_eq!(variables.text_color().text(), "#00ff00");
        assert_eq!(variables.text_color().channels(), [0x00, 0xff, 0x00]);
    }

    /// A `$defaults.background = "bad"` action with an invalid colour is rejected at
    /// runtime (logged, value unchanged), matching every other strict `=` assignment.
    #[tokio::test]
    async fn run_action_set_config_rejects_bad_colour() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.background = \"chartreuse\"",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // The value is unchanged and nothing was pushed to the device.
        let variables = state.variables.lock().unwrap();
        assert_eq!(variables.background().text(), "#000000");
    }

    /// A finished `$(command)` colour assignment stores the converted colour (the
    /// command-substitution completion path, distinct from a literal assignment).
    #[tokio::test]
    async fn apply_completed_assignment_sets_colour() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::TextColor),
                outcome: Ok(VarValue::Str("#00ff00".to_string())),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(
            state.variables.lock().unwrap().text_color().text(),
            "#00ff00"
        );
        assert_eq!(mock.last_button_brightness(), None);
    }

    /// A finished assignment whose converted value is the wrong type for its target is
    /// logged and discarded, leaving the stored colour unchanged.
    #[tokio::test]
    async fn apply_completed_assignment_rejects_non_colour_value() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::Background),
                outcome: Ok(VarValue::Int(5)),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(
            state.variables.lock().unwrap().background().text(),
            "#000000"
        );
    }

    /// A colour assignment whose value does not parse as a colour is logged and
    /// discarded without changing the stored value.
    #[tokio::test]
    async fn apply_completed_assignment_ignores_bad_colour_text() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::Background),
                outcome: Ok(VarValue::Str("chartreuse".to_string())),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(
            state.variables.lock().unwrap().background().text(),
            "#000000"
        );
    }

    /// A finished `$(command)` brightness assignment stores the converted number and
    /// pushes it to the device.
    #[tokio::test]
    async fn apply_completed_assignment_sets_brightness() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::ButtonBrightness),
                outcome: Ok(VarValue::Int(73)),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(state.variables.lock().unwrap().button_brightness(), 73);
        assert_eq!(mock.last_button_brightness(), Some(73));
    }

    /// A finished assignment to the background colour parameter stores it (the other
    /// colour arm of the completion path).
    #[tokio::test]
    async fn apply_completed_assignment_sets_background_colour() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::Background),
                outcome: Ok(VarValue::Str("navy".to_string())),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(state.variables.lock().unwrap().background().text(), "navy");
    }

    /// A finished encoder-brightness assignment reaches `set_led_brightness`; if the
    /// device refuses the write the failure is logged, not propagated.
    #[tokio::test]
    async fn apply_completed_assignment_pushes_encoder_brightness() {
        let mock = MockButtonDevice::default();
        mock.fail_brightness_calls(true);
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::EncoderBrightness),
                outcome: Ok(VarValue::Int(21)),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(state.variables.lock().unwrap().encoder_brightness(), 21);
        assert_eq!(mock.last_encoder_brightness(), None);
    }

    /// A finished assignment whose converted value is the wrong type for a brightness
    /// target is logged and discarded.
    #[tokio::test]
    async fn apply_completed_assignment_rejects_non_numeric_value() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::ButtonBrightness),
                outcome: Ok(VarValue::Str("bright".to_string())),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(state.variables.lock().unwrap().button_brightness(), 50);
        assert_eq!(mock.last_button_brightness(), None);
    }

    /// A failed strict command assignment is logged and changes nothing.
    #[tokio::test]
    async fn apply_completed_assignment_logs_strict_failure() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let state = EdgeState::new(Defaults::default());

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::ButtonBrightness),
                outcome: Err("command failed".to_string()),
            },
            &state.variables,
            &mut runner,
            Log::default(),
        )
        .await;

        assert_eq!(state.variables.lock().unwrap().button_brightness(), 50);
        assert_eq!(mock.last_button_brightness(), None);
    }

    /// When the device rejects a `$defaults.button_brightness := N` write, the
    /// failure is logged rather than propagated, and neither the current nor the
    /// previous scene changes: a `SetConfig` action never touches scene state either
    /// way, success or failure.
    #[tokio::test]
    async fn run_action_set_config_logs_when_brightness_write_fails() {
        let mock = MockButtonDevice::default();
        mock.fail_brightness_calls(true);
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.button_brightness := 80",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            mock.last_button_brightness(),
            None,
            "the write failed, so the mock never recorded a percent"
        );
        assert_eq!(mock.calls(), vec!["set_brightness"]);
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// The same failure-logging behavior as
    /// `run_action_set_config_logs_when_brightness_write_fails`, but for
    /// `$defaults.encoder_brightness` (`set_led_brightness`) instead of
    /// `button_brightness` (`set_brightness`) - the two `SetConfig` targets dispatch
    /// to distinct device methods, so each needs its own failing-write coverage.
    #[tokio::test]
    async fn run_action_set_config_logs_when_led_brightness_write_fails() {
        let mock = MockButtonDevice::default();
        mock.fail_brightness_calls(true);
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.encoder_brightness := 15",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            mock.last_encoder_brightness(),
            None,
            "the write failed, so the mock never recorded a percent"
        );
        assert_eq!(mock.calls(), vec!["set_led_brightness"]);
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A `$defaults.button_brightness := 200` action that exceeds the valid range is
    /// clamped to `100` at parse time, and the clamped value is applied to the device.
    #[tokio::test]
    async fn run_action_set_config_clamps_button_brightness_to_max() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.button_brightness := 200",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // Clamped to max (100), not the raw value (200)
        assert_eq!(mock.last_button_brightness(), Some(100));
        assert_eq!(mock.last_encoder_brightness(), None);
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A `$defaults.button_brightness := -10` action that is negative is clamped to
    /// `0` at parse time, and the clamped value is applied to the device.
    #[tokio::test]
    async fn run_action_set_config_clamps_button_brightness_to_min() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.button_brightness := -10",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // Clamped to min (0), not the raw value (-10)
        assert_eq!(mock.last_button_brightness(), Some(0));
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A variable assignment updates the shared store through the dispatch layer (`~=`
    /// clamps silently, `=` rejects an out-of-range value) and never touches the device.
    #[tokio::test]
    async fn run_action_assigns_variables() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());
        let mut defs = std::collections::BTreeMap::new();
        defs.insert("count".to_string(), dak::variables::VarDef::int(0, 10, 5));
        state.variables = std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
            defs,
            &Defaults::default(),
        )));

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$count ~= 100",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(
            state.variables.lock().unwrap().store().get("count"),
            Some(&dak::variables::VarValue::Int(10)),
            "~= clamps the value silently"
        );

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$count = 100",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(
            state.variables.lock().unwrap().store().get("count"),
            Some(&dak::variables::VarValue::Int(10)),
            "a strict out-of-range assignment is rejected, leaving the value unchanged"
        );
        assert!(
            mock.calls().is_empty(),
            "a variable assignment touches no device"
        );
    }

    /// A `$(command)` assignment runs on its own task and reports its converted value
    /// through the exec channel; applying it updates the store (variable target) or the
    /// device (default target).
    #[tokio::test]
    async fn command_substitution_assignment_reports_and_applies() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let (tx, mut rx) = mpsc::channel(4);
        let mut defs = std::collections::BTreeMap::new();
        defs.insert("count".to_string(), dak::variables::VarDef::int(0, 100, 5));
        let variables = std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
            defs,
            &Defaults::default(),
        )));

        dak::actions::start_command_assignment(
            dak::actions::AssignTarget::Variable("count".to_string()),
            dak::actions::AssignOp::ClampWarn,
            "echo 42",
            &variables,
            tx.clone(),
            Log::default(),
        );
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a command-substitution result should arrive")
            .expect("the exec channel must not close");
        match event {
            dak::actions::ExecEvent::Assignment(completed) => {
                super::apply_completed_assignment(
                    completed,
                    &variables,
                    &mut runner,
                    Log::default(),
                )
                .await;
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(
            variables.lock().unwrap().store().get("count"),
            Some(&VarValue::Int(42))
        );

        dak::actions::start_command_assignment(
            dak::actions::AssignTarget::Default(dak::actions::SettableDefault::ButtonBrightness),
            dak::actions::AssignOp::ClampWarn,
            "echo 77",
            &variables,
            tx,
            Log::default(),
        );
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            dak::actions::ExecEvent::Assignment(completed) => {
                super::apply_completed_assignment(
                    completed,
                    &variables,
                    &mut runner,
                    Log::default(),
                )
                .await;
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(variables.lock().unwrap().button_brightness(), 77);
        assert_eq!(mock.last_button_brightness(), Some(77));
    }

    /// A failed command assignment is logged and leaves the target unchanged, and a
    /// non-numeric value for a default target is ignored rather than pushed to the device.
    #[tokio::test]
    async fn apply_completed_assignment_handles_failure_and_wrong_type() {
        use dak::actions::{AssignTarget, CompletedAssign, SettableDefault};

        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let mut defs = std::collections::BTreeMap::new();
        defs.insert("count".to_string(), dak::variables::VarDef::int(0, 100, 5));
        let variables = std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
            defs,
            &Defaults::default(),
        )));

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Variable("count".to_string()),
                outcome: Err("command failed".to_string()),
            },
            &variables,
            &mut runner,
            Log::default(),
        )
        .await;
        assert_eq!(
            variables.lock().unwrap().store().get("count"),
            Some(&VarValue::Int(5)),
            "a failed assignment leaves the variable unchanged"
        );

        super::apply_completed_assignment(
            CompletedAssign {
                target: AssignTarget::Default(SettableDefault::ButtonBrightness),
                outcome: Ok(VarValue::Str("nope".to_string())),
            },
            &variables,
            &mut runner,
            Log::default(),
        )
        .await;
        assert_eq!(mock.last_button_brightness(), None);
        assert!(mock.calls().is_empty(), "nothing should reach the device");
    }

    /// A `$defaults.encoder_brightness := 200` action that exceeds the valid range
    /// is clamped to `100` at parse time, and the clamped value is applied to the
    /// device's LED brightness.
    #[tokio::test]
    async fn run_action_set_config_clamps_encoder_brightness_to_max() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.encoder_brightness := 200",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // Clamped to max (100), not the raw value (200)
        assert_eq!(mock.last_encoder_brightness(), Some(100));
        assert_eq!(mock.last_button_brightness(), None);
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A `$defaults.encoder_brightness := -10` action that is negative is clamped to
    /// `0` at parse time, and the clamped value is applied to the device's LED
    /// brightness.
    #[tokio::test]
    async fn run_action_set_config_clamps_encoder_brightness_to_min() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "$defaults.encoder_brightness := -10",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // Clamped to min (0), not the raw value (-10)
        assert_eq!(mock.last_encoder_brightness(), Some(0));
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
    }

    /// A command that exits with non-zero status (e.g., `false`) is still non-blocking
    /// and leaves the scene state untouched; `run_action` logs the failure via its
    /// spawned task but the caller never awaits it.
    #[tokio::test]
    async fn run_action_nonzero_command_leaves_scene_untouched() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        // `false` exits with status 1, which `run_action`'s spawned task logs.
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "false",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // `run_action` returns immediately; the spawned task still runs.
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
        assert!(mock.calls().is_empty(), "no device calls expected");

        // Wait for the spawned task to log its failure.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// A command that runs for a long time (e.g., `sleep 10`) returns immediately
    /// without blocking `run_action`, and the caller can proceed with subsequent
    /// actions.
    #[tokio::test]
    async fn run_action_slow_command_returns_immediately() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "Test": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        // Start with a slow command, then switch scenes immediately.
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "sleep 10",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // The slow command hasn't finished yet, but run_action returned.
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());

        // Now switch scenes; this should happen immediately since the
        // previous command was spawned on a background task.
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Test",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "Test");
        assert_eq!(previous_scene.as_deref(), Some("on_start"));
    }

    /// A command with malformed quotes (unbalanced single quote) is rejected by
    /// `parse_command_line` and `run_action` logs a warning instead of spawning
    /// the command; scene state is untouched.
    #[tokio::test]
    async fn run_action_malformed_command_skipped() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        // Unbalanced single quote: the parser sees an unterminated string.
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "/bin/sh -c 'hello",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
        assert!(mock.calls().is_empty(), "no device calls expected");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// An `@scene` action still switches even when the target scene has a button that
    /// fails to draw: the scene name and `previous_scene` update before the setup runs,
    /// and the failing button draws the red "Error" label instead of stopping the
    /// switch, mirroring `run_action_stay_keeps_scene_when_reapply_fails`.
    #[tokio::test]
    async fn run_action_switch_scene_still_switches_when_enter_fails() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "Test": {
                "setup": { "1b02": { "type": "image", "params": "/no/such/image.png" } },
                "actions": {}
            }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Test",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            current_scene, "Test",
            "the scene name updates even though its setup failed to apply"
        );
        assert_eq!(previous_scene.as_deref(), Some("on_start"));
        assert_eq!(
            mock.calls(),
            vec!["set", "flush", "flush"],
            "the failing operation draws the red \"Error\" label (staged + flushed by \
             draw_error_label) and the batch's own trailing flush still runs afterward"
        );
    }

    /// A bare `@` action whose current scene is not defined at all (as opposed to
    /// defined but containing a button that fails to draw, covered by
    /// `run_action_stay_keeps_scene_when_reapply_fails`) fails `enter_scene` itself;
    /// that failure is logged and swallowed rather than propagated, and the scene's
    /// timer is still (re)armed with whatever `arm_scene_timer` makes of the same
    /// undefined scene name (nothing, here, since it has no `actions.timer`).
    #[tokio::test]
    async fn run_action_stay_logs_and_continues_when_scene_is_undefined() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({});
        let mut current_scene = String::from("missing");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            current_scene, "missing",
            "a failed reapply must not change the scene name"
        );
        assert!(previous_scene.is_none());
        assert!(
            mock.calls().is_empty(),
            "enter_scene must fail before touching the device: {:?}",
            mock.calls()
        );
        assert!(
            state.timer_handle.is_none(),
            "an undefined scene has no timer to arm"
        );
    }

    /// A reconnect re-applies the brightness the user last set at runtime (not the
    /// config's startup value), clamped to the device's 0-100 range.
    #[test]
    fn current_brightness_follows_runtime_changes_and_clamps() {
        let variables = test_variables();
        let defaults = Defaults::default();
        assert_eq!(
            super::current_brightness(&variables),
            (defaults.button_brightness, defaults.encoder_brightness)
        );
        {
            let mut state = variables.lock().unwrap();
            state.set_button_brightness(35);
            state.set_encoder_brightness(250);
        }
        assert_eq!(super::current_brightness(&variables), (35, 100));
        variables.lock().unwrap().set_button_brightness(-4);
        assert_eq!(super::current_brightness(&variables).0, 0);
    }

    /// A second press landing inside the double-click gap cancels the first click's
    /// pending short-press confirmation, and the pair fires `double_click` on the
    /// second release.
    #[tokio::test]
    async fn pressable_edge_double_click_cancels_pending_short() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = pressable_scenes("1b01");
        let reference = Reference::button(1, 1);
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(quickly_clicking_defaults());

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;
        assert!(state.pending_shorts.contains_key(&reference));

        // Second press arrives within the gap: it cancels the pending confirmation
        // and its release is a double click instead of a short.
        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert!(
            state.pending_shorts.is_empty(),
            "the pending short press must be cancelled by the second press"
        );
        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;

        assert_eq!(
            current_scene, "D",
            "the double click fired on the second release"
        );
        assert!(state.pending_shorts.is_empty());
        state.assert_no_click(Duration::from_millis(120)).await;
    }

    /// Pushing an encoder knob flows through the same edge handler as a button: the
    /// remembered push code resolves to an encoder reference that behaves like any
    /// pressable control.
    #[tokio::test]
    async fn pressable_edge_handles_encoder_pushes_like_buttons() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = pressable_scenes("1e01");
        let reference = Reference::encoder(1, 1);
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(quickly_clicking_defaults());

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "P");

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "R");
        assert!(state.pending_shorts.contains_key(&reference));

        let (confirmed, event) = state.receive_click().await;
        assert_eq!(confirmed, reference);
        assert_eq!(event, ClickEvent::ShortPress);
    }

    /// Duplicate press reports while a control is already down, and releases of a
    /// control that was never pressed, are ignored: each event runs at most once.
    #[tokio::test]
    async fn pressable_edge_ignores_duplicate_and_unmatched_edges() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = pressable_scenes("1b01");
        let reference = Reference::button(1, 1);
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(quickly_clicking_defaults());

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "P");

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert_eq!(
            current_scene, "P",
            "a repeated press while down must not re-fire"
        );

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "R");

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;
        assert_eq!(
            current_scene, "R",
            "a repeated release while up must not re-fire"
        );

        let never_pressed = Reference::button(1, 2);
        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            never_pressed,
            false,
            &mut state,
        )
        .await;
        assert_eq!(
            current_scene, "R",
            "a release without a press must be ignored"
        );
    }

    /// A press held past the short-press threshold fires `long_press` on release, and
    /// no short-press confirmation is scheduled afterwards.
    #[tokio::test]
    async fn pressable_edge_long_press_fires_on_release() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = pressable_scenes("1b01");
        let reference = Reference::button(1, 1);
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(long_press_defaults());

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "P");

        tokio::time::sleep(Duration::from_millis(100)).await;
        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;

        assert_eq!(current_scene, "L", "the long press fired on release");
        assert!(state.pending_shorts.is_empty());
        state.assert_no_click(Duration::from_millis(120)).await;
    }

    /// A full click of a button flows through the edge handler: `pressed` fires on the
    /// down edge, `released` on the up edge, and a quick release schedules the
    /// short-press confirmation that a test can drain like the input loop does.
    #[tokio::test]
    async fn pressable_edge_runs_press_release_then_confirms_short_press() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = pressable_scenes("1b01");
        let reference = Reference::button(1, 1);
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(quickly_clicking_defaults());

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            true,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "P");
        assert_eq!(previous_scene.as_deref(), Some("on_start"));

        press_edge(
            &mut runner,
            &scenes,
            &mut current_scene,
            &mut previous_scene,
            reference,
            false,
            &mut state,
        )
        .await;
        assert_eq!(current_scene, "R");
        assert!(
            state.pending_shorts.contains_key(&reference),
            "a short press waits for the double-click gap"
        );

        let (confirmed, event) = state.receive_click().await;
        assert_eq!(confirmed, reference);
        assert_eq!(event, ClickEvent::ShortPress);

        state.pending_shorts.remove(&reference);
        state.click_detector.confirm_single();
        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &reference,
            "short_press",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "S");
    }

    /// the background task to completion so `run_action_command`'s success and
    /// failure branches both run, not just the `tokio::spawn` call itself.
    #[tokio::test]
    async fn run_action_command_completion_runs_for_success_and_failure() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "true",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "false",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        // `run_action` spawns the command and returns immediately; yield long enough
        // for both background tasks to run to completion (and hit their log lines)
        // before the test ends and the runtime drops any task still in flight.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(current_scene, "on_start", "commands never change the scene");
    }

    /// An `@scene` action targeting a scene that is not defined at all fails
    /// `enter_scene` itself, mirroring
    /// `run_action_stay_logs_and_continues_when_scene_is_undefined` for `SwitchScene`
    /// instead of `Stay`: the scene name and `previous_scene` still update (the switch
    /// itself does not depend on the target existing), the failure is logged and
    /// swallowed, and no device call is ever attempted.
    #[tokio::test]
    async fn run_action_switch_scene_logs_and_continues_when_target_is_undefined() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Missing",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(
            current_scene, "Missing",
            "the scene name updates even though the target scene does not exist"
        );
        assert_eq!(previous_scene.as_deref(), Some("on_start"));
        assert!(
            mock.calls().is_empty(),
            "enter_scene must fail before touching the device: {:?}",
            mock.calls()
        );
    }

    /// `run_actions` with multiple scene-changing actions keeps the last one's scene,
    /// since each `run_action` call updates `current_scene`.
    #[tokio::test]
    async fn run_actions_keeps_last_scene_in_list() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "A": { "actions": {} },
            "B": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        let actions: Vec<&str> = vec!["@A", "@B"];

        super::run_actions(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &actions,
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "B");
        assert_eq!(previous_scene.as_deref(), Some("A"));
    }

    /// `run_actions` runs multiple actions in sequence from the timer delivery path.
    /// A slow command in the list does not block subsequent actions since each command
    /// spawns on its own task.
    #[tokio::test]
    async fn run_actions_runs_sequence_from_timer_delivery() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "A": { "actions": {} },
            "B": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        let actions: Vec<&str> = vec!["/bin/sh -c 'true'", "/bin/sh -c 'sleep 2'", "@A", "@B"];

        super::run_actions(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &actions,
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "B");
        assert_eq!(
            previous_scene.as_deref(),
            Some("A"),
            "intermediate scene switch stored as previous"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// When `run_actions` encounters an unbound reference in the action list, it
    /// silently skips it; the sequence continues through remaining actions.
    #[tokio::test]
    async fn run_actions_skips_unbound_in_sequence() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": {} },
            "Test": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        let actions: Vec<&str> = vec!["@Test", "/bin/true", "/bin/false", "sleep 1"];

        super::run_actions(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &actions,
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "Test");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// `run_bound_action` dispatches the `released` event for a button reference.
    #[tokio::test]
    async fn run_bound_action_dispatches_released_event() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": { "1b01": { "released": "@Test" } } },
            "Test": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &Reference::button(1, 1),
            "released",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;

        assert_eq!(current_scene, "Test");
        assert_eq!(
            previous_scene.as_deref(),
            Some("on_start"),
            "released event switches scene like other events"
        );
    }

    /// `run_bound_action` dispatches the encoder turn events (`turn_cw` / `turn_ccw`)
    /// exactly like any other bound event on the encoder reference.
    #[tokio::test]
    async fn run_bound_action_dispatching_turn_events() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": { "1e01": { "turn_cw": "@CW", "turn_ccw": "@CCW" } } },
            "CW": { "actions": {} },
            "CCW": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &Reference::encoder(1, 1),
            "turn_cw",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "CW");

        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &Reference::encoder(1, 1),
            "turn_ccw",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "CCW");
    }

    /// A list of actions runs every entry in order; a slow command in the list does not
    /// block the scene-changing action that follows it. `Action::Command` spawns and
    /// never awaits its own completion, so looping over a list (see `run_actions`) is
    /// exactly as non-blocking as a single `run_action` call already is - "run without
    /// waiting on each other" falls out of that for free, since validation caps a list
    /// to at most one scene-changing entry, so there is never a second synchronous
    /// `enter_scene` competing with this loop's own state mutation.
    #[tokio::test]
    async fn run_bound_action_runs_a_list_without_waiting_on_a_slow_command() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": { "1b01": { "pressed": ["sleep 5", "@Test"] } } },
            "Test": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            super::run_bound_action(
                Log::default(),
                &mut runner,
                &mut current_scene,
                &mut previous_scene,
                &scenes,
                &Reference::button(1, 1),
                "pressed",
                &mut state.timer_handle,
                &state.timer_tx,
                &state.variables,
            ),
        )
        .await;

        assert!(
            result.is_ok(),
            "run_bound_action should not block on the slow command"
        );
        assert_eq!(
            current_scene, "Test",
            "the scene-changing action must still run"
        );
    }

    /// `run_bound_action` runs the action a reference bound to an event, switching
    /// scenes; a reference without a binding does nothing.
    #[tokio::test]
    async fn run_bound_action_runs_bound_and_ignores_unbound() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({
            "on_start": { "actions": { "1b01": { "pressed": "@Test" } } },
            "Test": { "actions": {} }
        });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());

        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &Reference::button(1, 1),
            "pressed",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "Test");

        super::run_bound_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            &Reference::button(1, 9),
            "pressed",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "Test", "an unbound reference must not act");
    }

    /// A minimal device definition for tests that only need one to exist.
    fn test_mapping() -> super::Mapping {
        serde_json::from_value(json!({
            "device_id": "0300:3002",
            "device_name": "keypad",
            "serial": "NOWHERE",
            "key_count": 9,
            "encoder_count": 3,
            "screens": 6,
            "buttons": [],
            "encoders": []
        }))
        .unwrap()
    }

    /// A defined device with no hardware found is left out (and warned about) rather
    /// than paired with anything.
    #[test]
    fn match_devices_skips_definitions_without_hardware() {
        let definitions = std::collections::BTreeMap::from([(1u8, test_mapping())]);
        assert!(super::match_devices(&definitions, &[], Log::default(), true).is_empty());
    }

    /// A lock directory that does not exist is an I/O error, logged and returned
    /// without waiting.
    #[tokio::test]
    async fn take_device_lock_reports_io_errors() {
        let key = dak::lock::DeviceKey::new(0x0300, 0x3002, Some("IOERR"), "");
        let stop = dak::control::StopSource::new();
        let result = super::take_device_lock(
            std::path::Path::new("/nonexistent/dak/locks"),
            &key,
            dak::lock::Conflict::Wait,
            &stop.signal(),
            Log::default(),
            None,
        )
        .await;
        assert!(matches!(result, Err(super::LockError::Io(_))), "{result:?}");
    }

    /// Device-write warnings are only worth a warning while the device is connected;
    /// once it is gone they are expected and only logged as debug output.
    #[test]
    fn warn_unless_disconnected_follows_the_connection() {
        let device = dak::reconnect::SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (exec_tx, _exec_rx) = mpsc::channel(1);
        let (refresh_tx, _refresh_rx) = mpsc::channel(1);
        let runner = SceneRunner::new(
            1,
            &device,
            hardware::Kind::Akp03ERev2.image_format(),
            exec_tx,
            refresh_tx,
            Log::default(),
            &HashSet::new(),
            Defaults::default().background,
            Defaults::default().text_color,
        );
        assert!(runner.is_connected());
        super::warn_unless_disconnected(Log::default(), &runner, "connected".to_string());
        device.mark_disconnected();
        assert!(!runner.is_connected());
        super::warn_unless_disconnected(Log::default(), &runner, "gone".to_string());
    }

    /// An action whose references do not resolve is reported and skipped: nothing is
    /// drawn and the scene stays.
    #[tokio::test]
    async fn run_action_skips_an_unresolvable_action() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(Defaults::default());
        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@$missing",
            &mut state.timer_handle,
            &state.timer_tx,
            &state.variables,
        )
        .await;
        assert_eq!(current_scene, "on_start");
        assert!(previous_scene.is_none());
        assert!(mock.calls().is_empty(), "{:?}", mock.calls());
    }

    /// A timer whose seconds come from a variable holding an invalid number of seconds
    /// is reported and not armed.
    #[tokio::test]
    async fn arm_scene_timer_skips_invalid_variable_seconds() {
        let mut defs = std::collections::BTreeMap::new();
        defs.insert(
            "period".to_string(),
            dak::variables::VarDef::int(-10, 10, -1),
        );
        let variables = std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
            defs,
            &Defaults::default(),
        )));
        let scenes = json!({ "S": { "actions": { "timer": { "$period": "@S" } } } });
        let (timer_tx, _timer_rx) = mpsc::channel(1);
        let handle =
            super::arm_scene_timer("S", &scenes, &timer_tx, Log::default(), &variables).await;
        assert!(handle.is_none());
    }

    // -- the supervisor, with fake device descriptions (no hardware is opened) --

    use super::{actions, exit, lock, Arc, Conflict, Controller, HidDeviceInfo};

    /// A device description as discovery reports it, for a node that does not exist:
    /// anything trying to open it fails at once.
    fn fake_info(vendor_id: u16, product_id: u16, serial: &str) -> HidDeviceInfo {
        HidDeviceInfo {
            id: async_hid::DeviceId::DevPath(PathBuf::from("/nonexistent/dak-test-hidraw")),
            name: "fake".to_string(),
            manufacturer: None,
            product_id,
            vendor_id,
            usage_id: 0,
            usage_page: 0,
            serial_number: Some(serial.to_string()),
        }
    }

    /// Distinguishes the supervisor tests' scratch directories.
    static SUPERVISOR_COUNTER: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A scratch directory (for lock files and the config) and a valid config file in
    /// it, with no devices.
    fn supervisor_scratch() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dak_supervisor_{}_{}",
            std::process::id(),
            SUPERVISOR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, r#"{"scenes": {"on_start": {}}, "devices": {}}"#).unwrap();
        (dir, config)
    }

    /// The options a plain foreground run with `conflict` has.
    fn serve_options<'a>(
        cli: &'a dak::cli::Cli,
        config_path: &'a std::path::Path,
        conflict: Conflict,
    ) -> super::ServeOptions<'a> {
        super::ServeOptions {
            cli,
            config_path,
            conflict,
            detached: false,
            readiness: None,
        }
    }

    /// A supervisor over the config at `config_path`, keeping its locks in `dir`.
    fn supervisor<'a>(
        options: &'a super::ServeOptions<'a>,
        dir: &std::path::Path,
        service: bool,
    ) -> super::Supervisor<'a> {
        let config = actions::load_config_from_path(options.config_path.to_str().unwrap())
            .expect("the test config loads");
        let controller = Arc::new(Controller::new());
        let mut supervisor =
            super::Supervisor::new(config, Log::default(), options, controller, service);
        supervisor.lock_dir = dir.to_path_buf();
        supervisor
    }

    /// Starting refuses a device whose lock another holder has (counted busy) and one
    /// whose lock cannot be taken at all (counted failed), starting no task for
    /// either.
    #[tokio::test]
    async fn supervisor_start_counts_busy_and_failed_devices() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, false);
        let definition = test_mapping();

        let held_info = fake_info(0x0300, 0x3002, "HELD");
        let _held = lock::try_lock(&dir, &super::device_key(&held_info)).unwrap();
        let (started, busy, failed) = supervisor
            .start(vec![(1, definition.clone(), held_info)])
            .await;
        assert_eq!((started.len(), busy, failed), (0, 1, 0));

        supervisor.lock_dir = dir.join("missing");
        let (started, busy, failed) = supervisor
            .start(vec![(2, definition, fake_info(0x0300, 0x3002, "NOLOCK"))])
            .await;
        assert_eq!((started.len(), busy, failed), (0, 0, 1));
        assert!(supervisor.tasks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A device task whose device cannot be driven (an unrecognized VID:PID) ends with
    /// the no-device status and gives its lock back; while it is still live, starting
    /// the same device again is skipped.
    #[tokio::test]
    async fn supervisor_task_for_an_undrivable_device_ends_with_no_device() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, false);
        let info = fake_info(0x1234, 0x5678, "UNKNOWN");
        let key = super::device_key(&info);

        let (started, busy, failed) = supervisor
            .start(vec![(1, test_mapping(), info.clone())])
            .await;
        assert_eq!((started.len(), busy, failed), (1, 0, 0));
        let (again, _, _) = supervisor.start(vec![(1, test_mapping(), info)]).await;
        assert!(again.is_empty(), "a live device is not started twice");

        assert_eq!(
            supervisor.wait_started(started).await,
            super::startup_summary(0, 0)
        );
        let joined = supervisor.tasks.join_next().await.unwrap();
        supervisor.finished(joined);
        assert_eq!(supervisor.status, exit::NO_DEVICE);
        assert!(supervisor.live.is_empty());
        assert!(!supervisor.locks.holds(&key), "the lock was given back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With `--wait`, a held device's task waits for the lock (reporting that it is
    /// waiting) and a stop ends the wait cleanly.
    #[tokio::test]
    async fn supervisor_waiting_task_reports_and_stops() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Wait);
        let mut supervisor = supervisor(&options, &dir, false);
        let info = fake_info(0x0300, 0x3002, "WAITING");
        let _held = lock::try_lock(&dir, &super::device_key(&info)).unwrap();

        let (started, _, _) = supervisor.start(vec![(1, test_mapping(), info)]).await;
        assert_eq!(
            supervisor.wait_started(started).await,
            super::startup_summary(0, 1)
        );
        supervisor.stop_all(true).await;
        assert_eq!(
            supervisor.status,
            exit::SUCCESS,
            "a cancelled wait is no failure"
        );
        assert!(supervisor.idle());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recognized keypad whose device node cannot be opened fails its task on the
    /// first connect (only a device lost *after* connecting is waited for), with the
    /// generic failure status, and its lock is given back.
    #[tokio::test]
    async fn supervisor_task_for_an_unopenable_device_fails() {
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, false);
        let info = fake_info(0x0300, 0x3002, "UNOPENABLE");
        let key = super::device_key(&info);
        let (started, _, _) = supervisor.start(vec![(1, test_mapping(), info)]).await;
        assert_eq!(
            supervisor.wait_started(started).await,
            super::startup_summary(0, 0)
        );
        let joined = tokio::time::timeout(Duration::from_secs(10), supervisor.tasks.join_next())
            .await
            .expect("the connect fails promptly")
            .unwrap();
        supervisor.finished(joined);
        assert_ne!(supervisor.status, exit::SUCCESS);
        assert!(!supervisor.locks.holds(&key));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `finished` keeps the first failure: a reported status, then a panicking task
    /// (which would be `FAILURE`) does not overwrite it.
    #[tokio::test]
    async fn supervisor_finished_keeps_the_first_failure() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, false);

        supervisor
            .tasks
            .spawn(async { (1, Err(super::TaskError::Reported(exit::CONFIG))) });
        let joined = supervisor.tasks.join_next().await.unwrap();
        supervisor.finished(joined);
        supervisor.tasks.spawn(async { panic!("task bug") });
        let joined = supervisor.tasks.join_next().await.unwrap();
        assert!(joined.is_err());
        supervisor.finished(joined);
        assert_eq!(supervisor.status, exit::CONFIG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In the foreground, a supervisor whose every device gave up (parked) has nothing
    /// left to drive: it stops and exits with the no-device status.
    #[tokio::test]
    async fn supervisor_foreground_with_only_parked_devices_exits() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, false);
        supervisor.live.insert(1, "parked.lock".to_string());
        supervisor.parked.insert(1);
        assert_eq!(supervisor.run().await, exit::NO_DEVICE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// As a service, the supervisor tracks parking and resuming from task events and
    /// ends on a quit request with the status so far.
    #[tokio::test]
    async fn supervisor_service_tracks_parking_until_quit() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, true);
        supervisor.live.insert(1, "one.lock".to_string());
        let events = supervisor.events.0.clone();
        let controller = supervisor.controller.clone();
        let (parked_tx, parked_rx) = tokio::sync::oneshot::channel();
        events.send(super::TaskEvent::Parked(1)).unwrap();
        let driver = tokio::spawn(async move {
            // Give the loop time to handle the parking before resuming and quitting.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = parked_tx.send(());
            events.send(super::TaskEvent::Resumed(1)).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            controller.handle(dak::control::SignalAction::Quit);
        });
        let status = tokio::time::timeout(Duration::from_secs(5), supervisor.run())
            .await
            .expect("the quit ends the loop");
        assert_eq!(status, exit::SUCCESS);
        parked_rx.await.unwrap();
        driver.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rescan or reload requested before the supervisor loop runs - e.g. a SIGUSR1
    /// arriving right after `READY=1`, while startup is still finishing - is acted on
    /// once the loop starts, not lost. (The counts used to be sampled only when the
    /// loop started, silently swallowing such a request.)
    #[tokio::test]
    async fn supervisor_acts_on_requests_made_during_startup() {
        use clap::Parser;
        let (dir, config) = supervisor_scratch();
        let cli = dak::cli::Cli::parse_from(["dak"]);
        let options = serve_options(&cli, &config, Conflict::Refuse);
        let mut supervisor = supervisor(&options, &dir, true);
        // The reload must pick up this edit, which is how it is seen to have happened.
        std::fs::write(
            &config,
            r#"{"scenes": {"on_start": {}, "reloaded": {}}, "devices": {}}"#,
        )
        .unwrap();
        let controller = supervisor.controller.clone();
        controller.handle(dak::control::SignalAction::Rescan);
        controller.handle(dak::control::SignalAction::Reload);
        let quitter = {
            let controller = controller.clone();
            tokio::spawn(async move {
                // Quit only once both requests were handled.
                let mut state = controller.subscribe();
                let _ = state.wait_for(|state| state.rescan > 0).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                controller.handle(dak::control::SignalAction::Quit);
            })
        };
        let status = tokio::time::timeout(Duration::from_secs(10), supervisor.run())
            .await
            .expect("the quit ends the loop");
        quitter.await.unwrap();
        assert_eq!(status, exit::SUCCESS);
        assert_eq!(supervisor.rescan_seen, 1, "the early rescan was handled");
        assert!(
            supervisor.config.scenes.get("reloaded").is_some(),
            "the early reload was acted on"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- the device session: routing, the event handlers and the input loop --

    use super::{Dispatch, Session, SessionChannels, SessionEnd};
    use dak::input::{encode_report, ScriptedInput, ScriptedReport};
    use dak::reconnect::SwappableDevice;

    /// A two-button, one-encoder device definition: button 1 reports code 1, button 2
    /// code 2 (outside the declared key count of 1), encoder 1 turns 81/80 and pushes 79;
    /// encoder 2's codes (91..89) are beyond the encoder count.
    fn session_mapping() -> super::Mapping {
        serde_json::from_value(json!({
            "device_id": "0300:3002", "device_name": "keypad", "serial": "S",
            "key_count": 1, "encoder_count": 1, "screens": 1,
            "buttons": [
                {"number": 1, "press": 1, "release": 1, "screen": true, "draw_id": 1},
                {"number": 2, "press": 2, "release": 2, "screen": false, "draw_id": -1}
            ],
            "encoders": [
                {"number": 1, "cw": 81, "ccw": 80, "press": 79, "release": 79},
                {"number": 2, "cw": 91, "ccw": 90, "press": 89, "release": 89}
            ]
        }))
        .unwrap()
    }

    /// A raw code resolves to the control it names (buttons and pushed encoders as
    /// press edges, turns with their event name); unknown codes and controls beyond the
    /// definition's counts resolve to nothing.
    #[test]
    fn route_resolves_codes_to_controls() {
        let definition = session_mapping();
        let route =
            |code: u8, pressed: bool| super::route(&definition, 1, code, pressed, Log::default());
        assert_eq!(
            route(1, true),
            Some(Dispatch::Edge(Reference::button(1, 1)))
        );
        assert_eq!(
            route(1, false),
            Some(Dispatch::Edge(Reference::button(1, 1)))
        );
        assert_eq!(
            route(79, true),
            Some(Dispatch::Edge(Reference::encoder(1, 1)))
        );
        assert_eq!(
            route(81, false),
            Some(Dispatch::Turn(Reference::encoder(1, 1), "turn_cw"))
        );
        assert_eq!(
            route(80, false),
            Some(Dispatch::Turn(Reference::encoder(1, 1), "turn_ccw"))
        );
        assert_eq!(route(2, true), None, "button 2 is beyond key_count");
        assert_eq!(route(91, false), None, "encoder 2 is beyond encoder_count");
        assert_eq!(route(89, true), None, "encoder 2 is beyond encoder_count");
        assert_eq!(route(200, true), None, "no control uses code 200");
    }

    /// Scenes for the session tests: in `on_start`, pressing button 1 switches to `P`,
    /// turning encoder 1 to `CW`/`CCW` and pushing it to `E`; the timer fires `@T`
    /// after an hour (never, in a test). Every scene clears button 1 on entry, so each
    /// switch is visible on the device.
    fn session_scenes() -> Value {
        let setup = json!({ "1b01": { "type": "clear" } });
        json!({
            "on_start": {
                "setup": setup,
                "actions": {
                    "1b01": { "pressed": "@P" },
                    "1e01": { "turn_cw": "@CW", "turn_ccw": "@CCW", "pressed": "@E" },
                    "timer": { "3600": "@T" }
                }
            },
            "P": { "setup": setup, "actions": {} },
            "CW": { "setup": setup, "actions": {} },
            "CCW": { "setup": setup, "actions": {} },
            "E": { "setup": setup, "actions": {} },
            "T": { "setup": setup, "actions": {} }
        })
    }

    /// Builds a session over `device` with fresh channels, returning the receiving
    /// ends too.
    fn new_session<'r, D: ButtonDevice>(
        device: &'r D,
        defaults: Defaults,
    ) -> (Session<'r, D>, SessionChannels) {
        let (exec_tx, exec_rx) = mpsc::channel(8);
        let (refresh_tx, refresh_rx) = mpsc::channel(8);
        let (click_tx, click_rx) = mpsc::channel(8);
        let (timer_tx, timer_rx) = mpsc::channel(1);
        let runner = SceneRunner::new(
            1,
            device,
            hardware::Kind::Akp03ERev2.image_format(),
            exec_tx,
            refresh_tx,
            Log::default(),
            &HashSet::new(),
            defaults.background.clone(),
            defaults.text_color.clone(),
        );
        let variables = std::sync::Arc::new(Mutex::new(Variables::new(
            std::collections::BTreeMap::new(),
            &defaults,
        )));
        let session = Session::new(
            1,
            session_mapping(),
            runner,
            session_scenes(),
            defaults,
            variables,
            click_tx,
            timer_tx,
            Log::default(),
        );
        let channels = SessionChannels {
            timer_rx,
            click_rx,
            exec_rx,
            refresh_rx,
        };
        (session, channels)
    }

    /// Entering `on_start` draws and flushes it and arms its timer.
    #[tokio::test]
    async fn session_enters_on_start_and_arms_its_timer() {
        let mock = MockButtonDevice::default();
        let (mut session, _channels) = new_session(&mock, Defaults::default());
        session.enter_on_start(&mock).await;
        assert_eq!(mock.calls(), vec!["clear", "flush", "flush"]);
        assert!(
            session.timer_handle.is_some(),
            "the on_start timer is armed"
        );
        assert_eq!(session.current_scene, "on_start");
    }

    /// Reports are decoded and routed: noise and unknown codes do nothing, a button
    /// press runs its `pressed` action (and a repeated press report is not
    /// re-dispatched), an encoder turn runs `turn_cw`.
    #[tokio::test]
    async fn session_dispatches_reports() {
        let mock = MockButtonDevice::default();
        let (mut session, _channels) = new_session(&mock, Defaults::default());
        let mut noise = encode_report(1, 1);
        noise[1] = 0;
        session.on_report(&noise).await;
        session.on_report(&encode_report(200, 1)).await;
        assert_eq!(session.current_scene, "on_start");
        assert!(mock.calls().is_empty());

        session.on_report(&encode_report(1, 1)).await;
        assert_eq!(session.current_scene, "P");
        assert_eq!(session.previous_scene.as_deref(), Some("on_start"));
        session.on_report(&encode_report(1, 1)).await;
        assert_eq!(
            mock.calls(),
            vec!["clear", "flush"],
            "a repeated press is ignored"
        );

        // `P` inherits on_start's bindings, so the encoder still works there.
        session.on_report(&encode_report(81, 0)).await;
        assert_eq!(session.current_scene, "CW");
    }

    /// The timer, click, exec and refresh events each reach their handler: timer
    /// actions run, a confirmed short press runs `short_press` (none bound here, so
    /// nothing changes), an exec result for a button without a running program is
    /// dropped, and a refresh of an empty button does nothing.
    #[tokio::test]
    async fn session_handles_channel_events() {
        let mock = MockButtonDevice::default();
        let (mut session, _channels) = new_session(&mock, Defaults::default());
        session.on_timer(vec!["@T".to_string()]).await;
        assert_eq!(session.current_scene, "T");

        session
            .on_click(Reference::button(1, 1), ClickEvent::ShortPress)
            .await;
        assert_eq!(session.current_scene, "T");

        session
            .on_exec(dak::actions::ExecEvent::Error {
                key: 5,
                generation: 99,
                error: "stale".to_string(),
            })
            .await;
        session.on_refresh(7).await;
        assert_eq!(
            mock.calls(),
            vec!["clear", "flush"],
            "only the timer's scene drew"
        );
    }

    /// After a disconnect the press state of the lost connection is forgotten: a
    /// control held down is up again, and a pending short press is cancelled.
    #[tokio::test]
    async fn session_reset_forgets_the_lost_connection() {
        let mock = MockButtonDevice::default();
        let (mut session, mut channels) = new_session(&mock, quickly_clicking_defaults());
        // Bind button 1 so its release starts a short-press confirmation.
        session.scenes = pressable_scenes("1b01");
        session.on_report(&encode_report(1, 1)).await;
        session.on_report(&encode_report(1, 0)).await;
        assert_eq!(session.pending_shorts.len(), 1);
        session.on_report(&encode_report(1, 1)).await;
        assert!(!session.down_controls.is_empty());

        session.reset_after_disconnect();
        assert!(session.down_controls.is_empty());
        assert!(session.pending_shorts.is_empty());
        let late = tokio::time::timeout(Duration::from_millis(300), channels.click_rx.recv()).await;
        assert!(
            !matches!(late, Ok(Some(_))),
            "the cancelled short press never arrives: {late:?}"
        );
    }

    /// The input loop hands reports to the session until the reader fails, which ends
    /// the connection as a disconnect with the reader's error.
    #[tokio::test]
    async fn run_connection_ends_when_the_reader_fails() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, mut channels) = new_session(&device, Defaults::default());
        let (input, reports) = ScriptedInput::new();
        reports
            .send(ScriptedReport::Data(encode_report(80, 0)))
            .unwrap();
        reports.send(ScriptedReport::Disconnect).unwrap();
        let stop = dak::control::StopSource::new();
        let end = tokio::time::timeout(
            Duration::from_secs(5),
            session.run_connection(&input, &mut channels, &stop.signal(), &device),
        )
        .await
        .unwrap();
        assert!(matches!(end, SessionEnd::Disconnected(_)));
        assert_eq!(
            session.current_scene, "CCW",
            "the report before it was handled"
        );
    }

    /// A stop signal ends the input loop as a quit, and so does a closed event
    /// channel.
    #[tokio::test]
    async fn run_connection_quits_on_stop_and_closed_channels() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, mut channels) = new_session(&device, Defaults::default());
        let input = ScriptedInput::with_events(&[]);
        let stop = dak::control::StopSource::new();
        stop.stop();
        let end = session
            .run_connection(&input, &mut channels, &stop.signal(), &device)
            .await;
        assert!(matches!(end, SessionEnd::Quit));

        let (mut session, mut channels) = new_session(&device, Defaults::default());
        // Replace the timer channel with one whose every sender is gone.
        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_tx);
        channels.timer_rx = closed_rx;
        let running = dak::control::StopSource::new();
        let end = session
            .run_connection(&input, &mut channels, &running.signal(), &device)
            .await;
        assert!(matches!(end, SessionEnd::Quit));
    }

    /// A draw that noticed the device is gone ends the input loop as a disconnect even
    /// while the reader has nothing to say.
    #[tokio::test]
    async fn run_connection_notices_a_lost_device() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, mut channels) = new_session(&device, Defaults::default());
        let input = ScriptedInput::with_events(&[]);
        let stop = dak::control::StopSource::new();
        device.mark_disconnected();
        let end = session
            .run_connection(&input, &mut channels, &stop.signal(), &device)
            .await;
        match end {
            SessionEnd::Disconnected(reason) => assert!(reason.contains("stopped responding")),
            SessionEnd::Quit => panic!("expected a disconnect"),
        }
    }

    /// Events arriving on the channels while the loop runs are handled in it: a timer
    /// delivery switches the scene, a refresh tick redraws.
    #[tokio::test]
    async fn run_connection_handles_channel_events() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, mut channels) = new_session(&device, Defaults::default());
        let input = ScriptedInput::with_events(&[]);
        let stop = dak::control::StopSource::new();
        session.timer_tx.send(vec!["@T".to_string()]).await.unwrap();
        let signal = stop.signal();
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.stop();
        };
        let (end, ()) = tokio::join!(
            session.run_connection(&input, &mut channels, &signal, &device),
            stopper
        );
        assert!(matches!(end, SessionEnd::Quit));
        assert_eq!(session.current_scene, "T");
    }

    // -- reconnecting, with scripted reopen outcomes --

    use super::{Reconnect, Reopen, ReopenError, Reopened};

    /// One scripted reopen attempt: an error, not back yet (`None`), or the device
    /// back under this name.
    type Outcome = Result<Option<&'static str>, ReopenError>;

    /// Plays back scripted reopen outcomes (then "not back yet" forever), recording
    /// the brightnesses each attempt was asked to apply.
    struct ScriptedReopen {
        /// The outcomes still to come.
        outcomes: Mutex<std::collections::VecDeque<Outcome>>,
        /// `(button, encoder)` brightness of every attempt.
        brightness: Mutex<Vec<(u8, u8)>>,
    }

    impl ScriptedReopen {
        /// A script of `outcomes`.
        fn new(outcomes: Vec<Outcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                brightness: Mutex::new(Vec::new()),
            }
        }
    }

    impl Reopen for ScriptedReopen {
        type Connection = MockButtonDevice;

        async fn reopen(
            &self,
            button_brightness: u8,
            encoder_brightness: u8,
        ) -> Result<Option<Reopened<MockButtonDevice>>, ReopenError> {
            self.brightness
                .lock()
                .unwrap()
                .push((button_brightness, encoder_brightness));
            let next = self.outcomes.lock().unwrap().pop_front();
            match next {
                None | Some(Ok(None)) => Ok(None),
                Some(Err(error)) => Err(error),
                Some(Ok(Some(name))) => Ok(Some(Reopened {
                    connection: MockButtonDevice::default(),
                    name: name.to_string(),
                    serial: "S2".to_string(),
                })),
            }
        }
    }

    /// A policy retrying quickly, at most `max_attempts` times (0 = forever).
    fn quick_policy(max_attempts: u64) -> dak::reconnect::ReconnectPolicy {
        dak::reconnect::ReconnectPolicy {
            interval: Duration::from_millis(5),
            max_attempts,
        }
    }

    /// A lost device is retried through discovery failures, "not back yet" and
    /// connect failures until it opens; the fresh connection gets the current
    /// brightness, is swapped in and repainted with what was on screen.
    #[tokio::test]
    async fn await_reconnect_retries_until_the_device_is_back() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, _channels) = new_session(&device, Defaults::default());
        session.enter_on_start(&device).await;
        session.variables.lock().unwrap().set_button_brightness(30);
        let reopen = ScriptedReopen::new(vec![
            Err(ReopenError::Discovery("enumeration failed".to_string())),
            Ok(None),
            Err(ReopenError::Connect("not ready".to_string())),
            Ok(Some("")),
        ]);
        let (park, _events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let stop = dak::control::StopSource::new();
        let variables = session.variables.clone();
        let outcome = super::await_reconnect(
            1,
            &reopen,
            "original name",
            &device,
            &mut session.runner,
            &variables,
            Log::default(),
            "unplugged",
            quick_policy(0),
            &stop.signal(),
            &park,
        )
        .await;
        assert_eq!(outcome, Reconnect::Reconnected);
        assert!(device.is_connected());
        let brightness = reopen.brightness.lock().unwrap().clone();
        assert_eq!(brightness.len(), 4);
        assert!(
            brightness.iter().all(|&(button, _)| button == 30),
            "{brightness:?}"
        );
        assert_eq!(
            device.current().unwrap().calls(),
            vec!["clear", "flush"],
            "the new connection is repainted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After its last allowed attempt the device task gives up and parks; a stop while
    /// parked ends it as cancelled.
    #[tokio::test]
    async fn await_reconnect_gives_up_parks_and_stops() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, _channels) = new_session(&device, Defaults::default());
        let reopen = ScriptedReopen::new(Vec::new());
        let (park, mut events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let stop = dak::control::StopSource::new();
        let variables = session.variables.clone();
        let signal = stop.signal();
        let waiting = super::await_reconnect(
            1,
            &reopen,
            "keypad",
            &device,
            &mut session.runner,
            &variables,
            Log::default(),
            "unplugged",
            quick_policy(2),
            &signal,
            &park,
        );
        let stopper = async {
            assert_eq!(events.recv().await, Some(super::TaskEvent::Parked(1)));
            stop.stop();
        };
        let (outcome, ()) = tokio::join!(waiting, stopper);
        assert_eq!(outcome, Reconnect::Cancelled);
        assert_eq!(reopen.brightness.lock().unwrap().len(), 2, "two attempts");
        assert!(!device.is_connected());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop while waiting for the device ends the wait as cancelled.
    #[tokio::test]
    async fn await_reconnect_is_cancelled_by_a_stop() {
        let device = SwappableDevice::new(MockButtonDevice::default(), |_| false);
        let (mut session, _channels) = new_session(&device, Defaults::default());
        let reopen = ScriptedReopen::new(Vec::new());
        let (park, _events, dir) = park_fixture(dak::lock::Conflict::Refuse);
        let stop = dak::control::StopSource::new();
        stop.stop();
        let variables = session.variables.clone();
        let outcome = super::await_reconnect(
            1,
            &reopen,
            "keypad",
            &device,
            &mut session.runner,
            &variables,
            Log::default(),
            "unplugged",
            quick_policy(0),
            &stop.signal(),
            &park,
        )
        .await;
        assert_eq!(outcome, Reconnect::Cancelled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
