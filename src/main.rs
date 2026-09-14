use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use mirajazz::{
    device::{list_devices, Device, DeviceQuery},
    error::MirajazzError,
    types::{
        DeviceInput, HidDevice, HidDeviceInfo, ImageFormat, ImageMirroring, ImageMode,
        ImageRotation,
    },
};
use serde_json::Value;
use tokio::sync::mpsc;

use dak::actions::{self, Action};
use dak::baseplane::{Baseplane, Reference};
use dak::log::{Log, Subsystem};
use dak::map::Mapping;
use dak::press::{ClickDetector, ClickEvent, PressDecision, PressDefaults, ReleaseDecision};

/// Command-line arguments for DAK (Dynamic Ajazz Keyboard), parsed by clap.
#[derive(Debug, Parser)]
#[command(
    name = "dak",
    about = "DAK (Dynamic Ajazz Keyboard): controls an Ajazz AKP03E / AKP03R USB macro keypad from a JSON config"
)]
struct Cli {
    /// Path to the config file. When omitted, `config.json` is searched for in
    /// `~/.config/dak/`, then the current directory, then the binary directory.
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,

    /// Debug subsystems to enable: device, scene, action.
    #[arg(short = 'd', long, value_delimiter = ',', num_args = 1..)]
    debug: Vec<String>,

    /// Run the interactive device-mapping wizard instead of normal operation:
    /// no config is read and no actions run; the wizard prints the collected
    /// device mapping as JSON and exits.
    #[arg(long)]
    map: bool,
}

const QUERY: DeviceQuery = DeviceQuery::new(65440, 1, 0x0300, 0x3002);

/// Protocol version used to connect to every device.
const PROTOCOL_VERSION: usize = 2;

const IMAGE_FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (60, 60),
    // The device's LCDs display images rotated 90 degrees clockwise.
    rotation: ImageRotation::Rot90,
    mirror: ImageMirroring::None,
};

/// Loads the config, matches the config's `devices` definitions against the discovered
/// hardware, and drives every present device: each connects with its own key/encoder
/// counts, applies the `on_start` scene, and reacts to keys, encoder events and scene
/// timers.
///
/// A device definition is matched to a discovered device by its serial number, falling
/// back to the VID:PID string when the definition's serial is "unknown". Definitions with
/// no matching hardware and discovered devices without a config definition are reported
/// and skipped; when no configured device is found the program exits with
/// `DeviceNotFoundError`.
#[tokio::main]
async fn main() -> Result<(), MirajazzError> {
    let cli = Cli::parse();
    let log = Log::from_debug_values(&cli.debug);
    log.info(format!(
        "DAK (Dynamic Ajazz Keyboard) v{}",
        env!("CARGO_PKG_VERSION")
    ));

    // The mapping wizard runs standalone: it must not read the config nor
    // execute any actions, and it exits on its own when done.
    if cli.map {
        return dak::map::run_map_wizard(log).await;
    }

    let config_path = actions::resolve_config_path(cli.config.as_deref());
    log.info(format!("Using config: {}", config_path.display()));
    let config = match actions::load_config_from_path(&config_path.to_string_lossy()) {
        Ok(config) => {
            for warning in &config.warnings {
                log.warn(warning);
            }
            config
        }
        Err(errors) => {
            for error in &errors {
                log.error(error);
            }
            return Err(MirajazzError::BadData);
        }
    };

    // Discovered devices come back from an unordered set. Each config device definition
    // (keyed by a logical device id) is matched against this set: serial numbers tell
    // identical devices apart, a VID:PID fallback covers devices without serials.
    let devices: Vec<HidDevice> = list_devices(&[QUERY]).await?.into_iter().collect();
    let mut assignments: Vec<(u8, Mapping, HidDeviceInfo)> = Vec::new();
    for (device_id, definition) in &config.devices.by_id {
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
                "device {device_id} defined in config was not found"
            )),
        }
    }

    for dev in &devices {
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

    if assignments.is_empty() {
        log.error("no device defined in config was found");
        return Err(MirajazzError::DeviceNotFoundError);
    }

    let baseplane = Baseplane::from_present(assignments.iter().map(|(id, _, _)| *id));
    log.info(format!(
        "{} device(s) present: {}",
        baseplane.present_numbers().len(),
        baseplane
            .present_numbers()
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // Drive every present device, each on its own task with its own input loop, scene
    // state and timer. Ctrl-C reaches every loop, so every device runs its cleanup.
    let mut handles = Vec::new();
    for (device_number, definition, device_info) in assignments {
        let scenes = config.scenes.clone();
        handles.push(tokio::spawn(run_device(
            device_number,
            definition,
            device_info,
            scenes,
            log,
            config.defaults,
        )));
    }
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(error) => {
                log.error(format!("device task failed unexpectedly: {error}"));
                return Err(MirajazzError::BadData);
            }
        }
    }

    Ok(())
}

/// Runs one present device: connects with the key/encoder counts from its config
/// definition, applies the `on_start` scene, and reacts to keys, encoder events, scene
/// timers and complex press events until the reader closes or Ctrl-C is pressed, then
/// restores the buttons this session changed and shuts the device down.
///
/// `device_number` is the id the definition is keyed under in the config; the runner
/// drives references naming this number and skips references to other devices with a
/// warning, so the same scenes address every device by its own number. `defaults`
/// carries the press-detection timing knobs from the config `defaults` section.
async fn run_device(
    device_number: u8,
    definition: Mapping,
    device_info: HidDeviceInfo,
    scenes: Value,
    log: Log,
    defaults: PressDefaults,
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

    // Connect to the device using the counts its config definition declares.
    let device = Device::connect(
        &device_info,
        PROTOCOL_VERSION,
        definition.key_count as usize,
        definition.encoder_count as usize,
    )
    .await?;
    let device = device.with_supports_both_keypress_states(true);
    let device = device.with_supports_both_encoder_states(true);

    // Print out some info from the device
    log.debug(
        Subsystem::Device,
        format!("Connected to '{}'", device.serial_number()),
    );

    device.set_brightness(50).await?;
    device.clear_all_button_images().await?;

    log.debug(
        Subsystem::Device,
        format!("Key count: {}", device.key_count()),
    );
    log.debug(
        Subsystem::Device,
        format!("Encoder count: {}", device.encoder_count()),
    );
    log.debug(
        Subsystem::Device,
        format!(
            "Supports_both_encoder_states: {}",
            device.supports_both_encoder_states()
        ),
    );

    // async image_exec/text_exec results land on buttons through this runner and its channel
    let (exec_tx, mut exec_rx) = mpsc::channel::<actions::ExecEvent>(8);

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
        IMAGE_FORMAT,
        exec_tx,
        log,
        &screenless_buttons,
    );

    // Complex press events (short/long press, double click): the detector decides
    // which event each press or release produces. Only the short press outlives its
    // release — it is confirmed through this channel after the double-click gap — so a
    // second press inside the gap can cancel the first click's pending confirmation.
    let (click_tx, mut click_rx) = mpsc::channel::<(u8, ClickEvent)>(8);
    let mut click_detector = ClickDetector::new(defaults);
    let mut pending_shorts: HashMap<u8, PendingShortPress> = HashMap::new();

    if let Err(error) = runner.enter_scene("on_start", &scenes).await {
        log.warn(format!("failed to apply on_start scene: {error}"));
    }

    // Flush
    device.flush().await?;

    let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));
    let mut current_scene = String::from("on_start");
    let mut buttons_down = vec![false; device.key_count()];

    // Timer events are delivered through a channel so the input loop can react to
    // them without blocking on the device reader.
    let (timer_tx, mut timer_rx) = mpsc::channel::<String>(1);
    let mut timer_handle = arm_scene_timer(&current_scene, &scenes, &timer_tx, log).await;

    // Actions are inherited from the previously active scene (see `action_for_event`),
    // so the scene we came from is remembered across scene switches.
    let mut previous_scene: Option<String> = None;

    loop {
        tokio::select! {
            data_result = reader.raw_read_data(512) => {
                let data = match data_result {
                    Ok(data) => data,
                    Err(_) => break,
                };
                if !data.starts_with(&[65, 67, 75]) {
                    continue;
                }
                let key = data[9] as usize;
                let pressed = data[10] != 0;
                let state = if pressed { "pressed" } else { "released" };
                log.debug(
                    Subsystem::Device,
                    format!("Key {}, {state}", data[9]),
                );

                if key >= buttons_down.len() {
                    continue;
                }

                if pressed && !buttons_down[key] {
                    buttons_down[key] = true;

                    // raw key codes are physical button numbers (1-based), so the
                    // event addresses the button of the number on this device
                    let reference = Reference::button(device_number, key as u8);

                    // A press inside the double-click gap turns the click into the
                    // second half of a double click: cancel the first click's pending
                    // short-press confirmation so it cannot fire as a short too.
                    if let PressDecision::Double = click_detector.press(Instant::now()) {
                        if let Some(pending) = pending_shorts.remove(&(key as u8)) {
                            pending.alive.store(false, Ordering::Relaxed);
                            pending.handle.abort();
                        }
                    }

                    run_bound_action(
                        log,
                        &mut runner,
                        &mut current_scene,
                        &mut previous_scene,
                        &scenes,
                        &reference,
                        "pressed",
                        key,
                        &mut timer_handle,
                        &timer_tx,
                    )
                    .await;
                } else if !pressed && buttons_down[key] {
                    buttons_down[key] = false;

                    let reference = Reference::button(device_number, key as u8);
                    let release_time = Instant::now();

                    run_bound_action(
                        log,
                        &mut runner,
                        &mut current_scene,
                        &mut previous_scene,
                        &scenes,
                        &reference,
                        "released",
                        key,
                        &mut timer_handle,
                        &timer_tx,
                    )
                    .await;

                    match click_detector.release(release_time) {
                        ReleaseDecision::Double => {
                            log.debug(
                                Subsystem::Device,
                                format!("key {key} double click detected"),
                            );
                            run_bound_action(
                                log,
                                &mut runner,
                                &mut current_scene,
                                &mut previous_scene,
                                &scenes,
                                &reference,
                                "double_click",
                                key,
                                &mut timer_handle,
                                &timer_tx,
                            )
                            .await;
                        }
                        ReleaseDecision::Long => {
                            log.debug(
                                Subsystem::Device,
                                format!("key {key} long press detected"),
                            );
                            run_bound_action(
                                log,
                                &mut runner,
                                &mut current_scene,
                                &mut previous_scene,
                                &scenes,
                                &reference,
                                "long_press",
                                key,
                                &mut timer_handle,
                                &timer_tx,
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
                            let gap = defaults.double_click_gap;
                            let handle = tokio::spawn(async move {
                                tokio::time::sleep(gap).await;
                                if task_alive.load(Ordering::Relaxed) {
                                    let _ = tx
                                        .send((key as u8, ClickEvent::ShortPress))
                                        .await;
                                }
                            });
                            pending_shorts.insert(
                                key as u8,
                                PendingShortPress { alive, handle },
                            );
                        }
                    }
                }
            }
            action = timer_rx.recv() => {
                let Some(action) = action else {
                    break;
                };
                log.debug(
                    Subsystem::Actions,
                    format!("timer for scene \"{current_scene}\" -> \"{action}\""),
                );
                run_action(
                    log,
                    &mut runner,
                    &mut current_scene,
                    &mut previous_scene,
                    &scenes,
                    &action,
                    &mut timer_handle,
                    &timer_tx,
                )
                .await;
            }
            click = click_rx.recv() => {
                let Some((key, event)) = click else {
                    break;
                };
                // The confirmation task finished on its own; drop its handle.
                pending_shorts.remove(&key);
                click_detector.confirm_single();
                let reference = Reference::button(device_number, key);
                match event {
                    ClickEvent::ShortPress => {
                        log.debug(
                            Subsystem::Device,
                            format!("key {key} single press detected"),
                        );
                        run_bound_action(
                            log,
                            &mut runner,
                            &mut current_scene,
                            &mut previous_scene,
                            &scenes,
                            &reference,
                            "short_press",
                            key as usize,
                            &mut timer_handle,
                            &timer_tx,
                        )
                        .await;
                    }
                }
            }
            event = exec_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                runner.handle_exec_event(event).await;
            }
            _ = tokio::signal::ctrl_c() => {
                // Ctrl-C (SIGINT) normally kills the process instantly; route it
                // through the same break so the cleanup + shutdown below run.
                break;
            }
        }
    }

    drop(reader);

    // Restore the buttons this program touched: clear the image on every button whose
    // image the session changed, then flush. Buttons never changed are left alone.
    if let Err(error) = runner.clear_changed_button_images().await {
        log.warn(format!("failed to restore changed buttons: {error}"));
    }

    device.shutdown().await?;
    Ok(())
}

/// A delayed short-press confirmation for one button.
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

/// Resolves the action `reference` has bound to `event` (e.g. `pressed` or `released`)
/// and runs it, logging the dispatch. Unbound references simply do nothing.
///
/// The action is looked up in the current scene first, then in the previously active
/// scene (see `actions::action_for_event`), so pushed buttons keep their released
/// behavior after a scene switch.
///
/// The parameter list mirrors `run_action`: each device has exactly one input loop and
/// the shared state it needs is passed flat rather than bundled into a context type.
#[allow(clippy::too_many_arguments)]
async fn run_bound_action(
    log: Log,
    runner: &mut actions::SceneRunner<'_, Device>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    reference: &Reference,
    event: &str,
    key: usize,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<String>,
) {
    let Some(action) = actions::action_for_event(
        current_scene,
        previous_scene.as_deref(),
        reference,
        event,
        scenes,
    ) else {
        return;
    };

    log.debug(
        Subsystem::Actions,
        format!("key {key} {event} -> \"{action}\""),
    );

    run_action(
        log,
        runner,
        current_scene,
        previous_scene,
        scenes,
        action,
        timer_handle,
        timer_tx,
    )
    .await;
}

/// Executes a scene action: stays (re-applying the current scene), switches scene,
/// or runs a command on its own background task.
///
/// Switching scenes re-arms the new scene's timer and remembers the old scene, so its
/// button actions remain available through inheritance; the `~` "stay" action re-applies
/// the current scene and re-arms its timer so periodic updates (e.g. a clock) keep
/// refreshing. Scene changes run through `runner`, which also owns the device.
///
/// The parameter list is deliberately kept flat over bundling the shared state into one
/// struct: each device has exactly one input loop, so a context type adds indirection
/// without removing any call sites.
#[allow(clippy::too_many_arguments)]
async fn run_action(
    log: Log,
    runner: &mut actions::SceneRunner<'_, Device>,
    current_scene: &mut String,
    previous_scene: &mut Option<String>,
    scenes: &Value,
    action_value: &str,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<String>,
) {
    match actions::parse_action(action_value) {
        Action::Stay => {
            log.debug(
                Subsystem::Actions,
                format!("stay on scene \"{current_scene}\" (re-applies it)"),
            );
            if let Err(error) = runner.enter_scene(&*current_scene, scenes).await {
                log.warn(format!(
                    "failed to refresh scene \"{current_scene}\": {error}"
                ));
            }
            rearm_scene_timer(&*current_scene, scenes, timer_handle, timer_tx, log).await;
        }
        Action::Command { command } => {
            // Commands run on their own task so a running program never blocks the
            // device input loop or the scene timer, and they are not awaited inline.
            log.debug(Subsystem::Actions, format!("run command \"{command}\""));
            match actions::parse_command_line(&command) {
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
                log.warn(format!("failed to enter scene \"{scene}\": {error}"));
            }
            rearm_scene_timer(&*current_scene, scenes, timer_handle, timer_tx, log).await;
        }
    }
}

/// Starts (or restarts) the current scene's timer, aborting any previous one.
///
/// The timer read from the scene's `actions.timer` fires after its number of seconds
/// and delivers its action value through `timer_tx`. Returns the new task handle.
async fn arm_scene_timer(
    scene_name: &str,
    scenes: &Value,
    timer_tx: &mpsc::Sender<String>,
    log: Log,
) -> Option<tokio::task::JoinHandle<()>> {
    match actions::timer_for_scene(scene_name, scenes) {
        Some((seconds, action)) => {
            log.debug(
                Subsystem::Scene,
                format!("armed timer for scene \"{scene_name}\": {seconds}s -> \"{action}\""),
            );
            let tx = timer_tx.clone();
            let action = action.to_string();
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                let _ = tx.send(action).await;
            }))
        }
        None => None,
    }
}

/// Aborts any running timer task and arms the timer for `scene_name`, if it defines one.
async fn rearm_scene_timer(
    scene_name: &str,
    scenes: &Value,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<String>,
    log: Log,
) {
    if let Some(handle) = timer_handle.take() {
        handle.abort();
    }
    *timer_handle = arm_scene_timer(scene_name, scenes, timer_tx, log).await;
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
    use std::path::Path;

    use clap::Parser;

    use super::Cli;

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
}
