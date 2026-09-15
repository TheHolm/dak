use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// See src/lib.rs for why this is needed on FreeBSD only: the binary crate compiles
// separately from the library crate, so it needs its own copy of the rename.
#[cfg(target_os = "freebsd")]
extern crate mirajazz_freebsd as mirajazz;

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
use dak::map::{ControlEvent, Mapping, TwistDirection};
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
    // which event each press or release produces. Widgets are addressed by their
    // reference, so buttons and pushed encoders share one registry; only the short
    // press outlives its release — it is confirmed through this channel after the
    // double-click gap — so a second press inside the gap can cancel the first
    // click's pending confirmation.
    let (click_tx, mut click_rx) = mpsc::channel::<(Reference, ClickEvent)>(8);
    let mut click_detector = ClickDetector::new(defaults);
    let mut pending_shorts: HashMap<Reference, PendingShortPress> = HashMap::new();

    if let Err(error) = runner.enter_scene("on_start", &scenes).await {
        log.warn(format!("failed to apply on_start scene: {error}"));
    }

    // Flush
    device.flush().await?;

    let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));
    let mut current_scene = String::from("on_start");
    // Button and pushed-encoder controls currently held down, so repeated press
    // reports of the same widget are not re-dispatched and releases without a
    // press are ignored.
    let mut down_controls: std::collections::HashSet<Reference> = std::collections::HashSet::new();

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
                // The raw code in the report names a widget by its captured
                // press/release or turn code, not by its number (buttons without a
                // display report far larger codes). Translate it through the
                // definition and skip reports that no widget uses.
                let code = data[9];
                let pressed = data[10] != 0;
                let state = if pressed { "pressed" } else { "released" };
                log.debug(
                    Subsystem::Device,
                    format!("Key {code}, {state}"),
                );

                let Some(event) = definition.control_event(code, pressed) else {
                    log.debug(
                        Subsystem::Device,
                        format!("no control uses raw code {code}; skipping"),
                    );
                    continue;
                };

                match event {
                    ControlEvent::Button { number } => {
                        if number > definition.key_count {
                            log.debug(
                                Subsystem::Device,
                                format!("button {number} is out of range (device has {} buttons); skipping", definition.key_count),
                            );
                            continue;
                        }
                        let reference = Reference::button(device_number, number);
                        run_pressable_edge(
                            log,
                            &mut runner,
                            &mut current_scene,
                            &mut previous_scene,
                            &scenes,
                            &reference,
                            pressed,
                            &mut down_controls,
                            &mut click_detector,
                            &click_tx,
                            &mut pending_shorts,
                            defaults,
                            &mut timer_handle,
                            &timer_tx,
                        )
                        .await;
                    }
                    ControlEvent::EncoderPress { number } => {
                        if number > definition.encoder_count {
                            log.debug(
                                Subsystem::Device,
                                format!("encoder {number} is out of range (device has {} encoders); skipping", definition.encoder_count),
                            );
                            continue;
                        }
                        let reference = Reference::encoder(device_number, number);
                        run_pressable_edge(
                            log,
                            &mut runner,
                            &mut current_scene,
                            &mut previous_scene,
                            &scenes,
                            &reference,
                            pressed,
                            &mut down_controls,
                            &mut click_detector,
                            &click_tx,
                            &mut pending_shorts,
                            defaults,
                            &mut timer_handle,
                            &timer_tx,
                        )
                        .await;
                    }
                    ControlEvent::EncoderTurn { number, direction } => {
                        if number > definition.encoder_count {
                            log.debug(
                                Subsystem::Device,
                                format!("encoder {number} is out of range (device has {} encoders); skipping", definition.encoder_count),
                            );
                            continue;
                        }
                        let event = match direction {
                            TwistDirection::Clockwise => "turn_cw",
                            TwistDirection::CounterClockwise => "turn_ccw",
                        };
                        run_bound_action(
                            log,
                            &mut runner,
                            &mut current_scene,
                            &mut previous_scene,
                            &scenes,
                            &Reference::encoder(device_number, number),
                            event,
                            &mut timer_handle,
                            &timer_tx,
                        )
                        .await;
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
                let Some((pending_reference, event)) = click else {
                    break;
                };
                // The confirmation task finished on its own; drop its handle.
                pending_shorts.remove(&pending_reference);
                click_detector.confirm_single();
                match event {
                    ClickEvent::ShortPress => {
                        log.debug(
                            Subsystem::Device,
                            format!("{pending_reference} single press detected"),
                        );
                        run_bound_action(
                            log,
                            &mut runner,
                            &mut current_scene,
                            &mut previous_scene,
                            &scenes,
                            &pending_reference,
                            "short_press",
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

/// Resolves the action `reference` has bound to `event` (e.g. `pressed`,
/// `released` or `turn_cw`) and runs it, logging the dispatch. Unbound
/// references simply do nothing.
///
/// The action is looked up in the current scene first, then in the previously active
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
        format!("{reference} {event} -> \"{action}\""),
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
    defaults: PressDefaults,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<String>,
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
/// button actions remain available through inheritance; the `~` "stay" action re-applies
/// the current scene and re-arms its timer so periodic updates (e.g. a clock) keep
/// refreshing. Scene changes run through `runner`, which also owns the device.
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
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use std::time::Duration;

    use clap::Parser;
    use image::{DynamicImage, Rgb, RgbImage};
    use mirajazz::types::ImageFormat;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use dak::actions::{ButtonDevice, SceneRunner};
    use dak::log::Log;

    use super::{Cli, ClickDetector, ClickEvent, PressDefaults, Reference};

    /// A tiny recording keypad for the dispatch-layer tests: scene `setup` operations
    /// that reach the device are recorded so a test can see which scene was applied.
    #[derive(Default)]
    struct MockButtonDevice {
        calls: Mutex<Vec<&'static str>>,
    }

    impl MockButtonDevice {
        /// Every `clear`/`flush`/`set` the runner attempted, in order.
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ButtonDevice for MockButtonDevice {
        type Error = std::convert::Infallible;

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

    /// Builds a scene runner over a mock device, dropping the exec channel: none of
    /// these tests run `image_exec`/`text_exec`.
    fn make_runner(mock: &MockButtonDevice) -> SceneRunner<'_, MockButtonDevice> {
        let (exec_tx, _exec_rx) = mpsc::channel(8);
        SceneRunner::new(
            1,
            mock,
            super::IMAGE_FORMAT,
            exec_tx,
            Log::default(),
            &HashSet::new(),
        )
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
        timer_tx: mpsc::Sender<String>,
        defaults: PressDefaults,
    }

    impl EdgeState {
        /// A fresh loop state with the given press-detection knobs.
        fn new(defaults: PressDefaults) -> EdgeState {
            let (click_tx, click_rx) = mpsc::channel(8);
            let (timer_tx, _timer_rx) = mpsc::channel(1);
            EdgeState {
                down_controls: HashSet::new(),
                click_detector: ClickDetector::new(defaults),
                click_tx,
                click_rx,
                pending_shorts: HashMap::new(),
                timer_handle: None,
                timer_tx,
                defaults,
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
            state.defaults,
            &mut state.timer_handle,
            &state.timer_tx,
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
    fn quickly_clicking_defaults() -> PressDefaults {
        PressDefaults {
            short_press_duration: Duration::from_millis(700),
            double_click_gap: Duration::from_millis(80),
        }
    }

    /// A short press threshold, for turning a held press into a long press quickly.
    fn long_press_defaults() -> PressDefaults {
        PressDefaults {
            short_press_duration: Duration::from_millis(60),
            double_click_gap: Duration::from_millis(600),
        }
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
        assert!(
            super::arm_scene_timer("on_start", &scenes, &tx, Log::default())
                .await
                .is_none()
        );
        assert!(super::arm_scene_timer("Main", &scenes, &tx, Log::default())
            .await
            .is_some());
    }

    /// An armed timer fires after its configured seconds and delivers the action
    /// through the channel, like a scene timer running during normal operation.
    #[tokio::test]
    async fn timer_fires_after_its_seconds_and_delivers_the_action() {
        let scenes = json!({
            "on_start": { "actions": { "timer": { "1": "@Main" } } }
        });
        let (tx, mut rx) = mpsc::channel(1);
        let handle = super::arm_scene_timer("on_start", &scenes, &tx, Log::default())
            .await
            .expect("scene has a timer");
        handle.await.unwrap();
        assert_eq!(rx.recv().await, Some("@Main".to_string()));
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
        let mut handle = super::arm_scene_timer("Main", &scenes, &tx, Log::default()).await;
        assert!(handle.is_some());

        super::rearm_scene_timer("on_start", &scenes, &mut handle, &tx, Log::default()).await;
        assert!(handle.is_none(), "a scene without a timer arms nothing");

        let late = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
        assert!(late.is_err(), "the aborted timer must not fire: {late:?}");
    }

    /// A `~` action re-applies the current scene: its setup runs on the device again
    /// and the scene name is untouched, with no previous scene recorded.
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "~",
            &mut state.timer_handle,
            &state.timer_tx,
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "true",
            &mut state.timer_handle,
            &state.timer_tx,
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Test",
            &mut state.timer_handle,
            &state.timer_tx,
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "~",
            &mut state.timer_handle,
            &state.timer_tx,
        )
        .await;
        let _ = std::fs::remove_file(&path);

        assert_eq!(mock.calls(), vec!["set", "flush"]);
    }

    /// A `~` action whose scene fails to re-apply (here, an `image` setup entry
    /// pointing at a missing file) logs a warning and leaves the scene name and
    /// `previous_scene` untouched: staying never counts as leaving.
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "~",
            &mut state.timer_handle,
            &state.timer_tx,
        )
        .await;

        assert_eq!(
            current_scene, "on_start",
            "a failed reapply must not change the scene name"
        );
        assert!(previous_scene.is_none());
        assert_eq!(
            mock.calls(),
            Vec::<&str>::new(),
            "the failing operation must not stage anything on the device"
        );
    }

    /// An `@scene` action whose target scene fails to enter still switches: the scene
    /// name and `previous_scene` update before the setup runs, and the failure is only
    /// logged, mirroring `run_action_stay_keeps_scene_when_reapply_fails`.
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
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "@Test",
            &mut state.timer_handle,
            &state.timer_tx,
        )
        .await;

        assert_eq!(
            current_scene, "Test",
            "the scene name updates even though its setup failed to apply"
        );
        assert_eq!(previous_scene.as_deref(), Some("on_start"));
        assert_eq!(
            mock.calls(),
            Vec::<&str>::new(),
            "the failing operation must not stage anything on the device"
        );
    }

    /// A spawned command action's completion is logged on both outcomes: this drives
    /// the background task to completion so `run_action_command`'s success and
    /// failure branches both run, not just the `tokio::spawn` call itself.
    #[tokio::test]
    async fn run_action_command_completion_runs_for_success_and_failure() {
        let mock = MockButtonDevice::default();
        let mut runner = make_runner(&mock);
        let scenes = json!({ "on_start": { "actions": {} } });
        let mut current_scene = String::from("on_start");
        let mut previous_scene = None;
        let mut state = EdgeState::new(PressDefaults::default());

        super::run_action(
            Log::default(),
            &mut runner,
            &mut current_scene,
            &mut previous_scene,
            &scenes,
            "true",
            &mut state.timer_handle,
            &state.timer_tx,
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
        )
        .await;

        // `run_action` spawns the command and returns immediately; yield long enough
        // for both background tasks to run to completion (and hit their log lines)
        // before the test ends and the runtime drops any task still in flight.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(current_scene, "on_start", "commands never change the scene");
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
        let mut state = EdgeState::new(PressDefaults::default());

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
        )
        .await;
        assert_eq!(current_scene, "Test", "an unbound reference must not act");
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
        let mut state = EdgeState::new(PressDefaults::default());

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
        )
        .await;
        assert_eq!(current_scene, "CCW");
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
        )
        .await;
        assert_eq!(current_scene, "S");
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
}
