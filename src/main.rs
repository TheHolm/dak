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
    device::{list_devices, Device},
    error::MirajazzError,
    types::{DeviceInput, HidDevice, HidDeviceInfo},
};
use serde_json::Value;
use tokio::sync::mpsc;

use dak::actions::{self, Action};
use dak::baseplane::Reference;
use dak::hardware;
use dak::log::{Log, Subsystem};
use dak::map::{ControlEvent, Mapping, TwistDirection};
use dak::press::{ClickDetector, ClickEvent, Defaults, PressDecision, ReleaseDecision};
use dak::variables::{VarValue, Variables};

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
            log.info(format!(
                "Loaded config version {} from {}",
                config.version,
                config_path.display()
            ));
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
    let devices: Vec<HidDevice> = list_devices(&hardware::QUERIES)
        .await?
        .into_iter()
        .collect();
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
                "device #{device_id} ({} s/n {}, expecting {}) defined in config was not found",
                definition.device_name, definition.serial, definition.device_id
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

    // Drive every present device, each on its own task with its own input loop, scene
    // state and timer. Ctrl-C reaches every loop, so every device runs its cleanup.
    let mut handles = Vec::new();
    // One variable/default state shared by every device: the declarations are global, so
    // an assignment from one device's input is visible to all of them.
    let variables = Arc::new(std::sync::Mutex::new(Variables::new(
        config.variables.clone(),
        &config.defaults,
    )));
    for (device_number, definition, device_info) in assignments {
        let scenes = config.scenes.clone();
        handles.push(tokio::spawn(run_device(
            device_number,
            definition,
            device_info,
            scenes,
            log,
            config.defaults,
            variables.clone(),
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
    defaults: Defaults,
    variables: Arc<std::sync::Mutex<Variables>>,
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

    // Connect to the device using the counts its config definition declares.
    let device = Device::connect(
        &device_info,
        protocol_version,
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
    log.info(format!(
        "Connected to {} s/n {} as device #{device_number} using protocol version {protocol_version}",
        device_info.name,
        device.serial_number()
    ));

    device.set_brightness(defaults.button_brightness).await?;
    // Not verified to have any visible effect: see `Defaults::encoder_brightness`'s
    // doc comment for why (no unit with functioning encoder LEDs was available to
    // confirm this against). Sent unconditionally anyway, same as `set_brightness`
    // above, since it costs nothing when the device has no encoders or LEDs.
    device
        .set_led_brightness(defaults.encoder_brightness)
        .await?;
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

    // A button with a nonzero `refresh_seconds` sends its own key here once its
    // interval elapses; the runner redraws just that button and re-arms the next tick.
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<u8>(8);

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
    let (timer_tx, mut timer_rx) = mpsc::channel::<Vec<String>>(1);
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
                            &variables,
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
                            &variables,
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
                            &variables,
                        )
                        .await;
                    }
                }
            }
            action = timer_rx.recv() => {
                let Some(actions) = action else {
                    break;
                };
                log.debug(
                    Subsystem::Actions,
                    format!("timer for scene \"{current_scene}\" -> {actions:?}"),
                );
                let actions: Vec<&str> = actions.iter().map(String::as_str).collect();
                run_actions(
                    log,
                    &mut runner,
                    &mut current_scene,
                    &mut previous_scene,
                    &scenes,
                    &actions,
                    &mut timer_handle,
                    &timer_tx,
                    &variables,
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
                            &variables,
                        )
                        .await;
                    }
                }
            }
            event = exec_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    actions::ExecEvent::Assignment(completed) => {
                        apply_completed_assignment(completed, &variables, &mut runner, log).await;
                    }
                    event => runner.handle_exec_event(event).await,
                }
            }
            key = refresh_rx.recv() => {
                let Some(key) = key else {
                    break;
                };
                log.debug(
                    Subsystem::Scene,
                    format!("refresh tick for button {key}"),
                );
                if let Err(error) = runner.refresh_button(key).await {
                    log.warn(format!("failed to refresh button {key}: {error}"));
                }
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
    defaults: Defaults,
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
                log.warn(format!("failed to enter scene \"{scene}\": {error}"));
            }
            rearm_scene_timer(&*current_scene, scenes, timer_handle, timer_tx, log).await;
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
            if let Some((param, value)) = side_effect {
                let result = match param {
                    actions::SettableDefault::ButtonBrightness => {
                        runner.set_button_brightness(value as u8).await
                    }
                    actions::SettableDefault::EncoderBrightness => {
                        runner.set_encoder_brightness(value as u8).await
                    }
                };
                if let Err(error) = result {
                    log.warn(format!("failed to set {}: {error}", param.path()));
                }
            }
        }
    }
}

/// Applies a finished command-substitution assignment: stores the converted value and,
/// for a writable default, pushes the new brightness to the device.
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
            let VarValue::Int(number) = value else {
                log.error(format!(
                    "assignment to {} produced a non-numeric value",
                    param.path()
                ));
                return;
            };
            let mut state = variables.lock().expect("variables mutex poisoned");
            match param {
                actions::SettableDefault::ButtonBrightness => state.set_button_brightness(number),
                actions::SettableDefault::EncoderBrightness => state.set_encoder_brightness(number),
            }
            Some((*param, number))
        }
    };

    if let Some((param, number)) = side_effect {
        let result = match param {
            actions::SettableDefault::ButtonBrightness => {
                runner.set_button_brightness(number as u8).await
            }
            actions::SettableDefault::EncoderBrightness => {
                runner.set_encoder_brightness(number as u8).await
            }
        };
        if let Err(error) = result {
            log.warn(format!("failed to set {}: {error}", param.path()));
        }
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
) -> Option<tokio::task::JoinHandle<()>> {
    match actions::timer_for_scene(scene_name, scenes) {
        Some((seconds, actions)) => {
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
        None => None,
    }
}

/// Aborts any running timer task and arms the timer for `scene_name`, if it defines one.
async fn rearm_scene_timer(
    scene_name: &str,
    scenes: &Value,
    timer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    timer_tx: &mpsc::Sender<Vec<String>>,
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
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Mutex;
    use std::time::Duration;

    use clap::Parser;
    use image::{DynamicImage, Rgb, RgbImage};
    use mirajazz::types::ImageFormat;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    use dak::actions::{ButtonDevice, SceneRunner};
    use dak::hardware;
    use dak::log::Log;

    use super::{Cli, ClickDetector, ClickEvent, Defaults, Reference, VarValue, Variables};

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
            EdgeState {
                down_controls: HashSet::new(),
                click_detector: ClickDetector::new(defaults),
                click_tx,
                click_rx,
                pending_shorts: HashMap::new(),
                timer_handle: None,
                timer_tx,
                defaults,
                variables: std::sync::Arc::new(std::sync::Mutex::new(Variables::new(
                    std::collections::BTreeMap::new(),
                    &defaults,
                ))),
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
        let mut handle = super::arm_scene_timer("Main", &scenes, &tx, Log::default()).await;
        assert!(handle.is_some());

        super::rearm_scene_timer("on_start", &scenes, &mut handle, &tx, Log::default()).await;
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
}
