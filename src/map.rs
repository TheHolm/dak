//! Interactive device-mapping wizard, started with `dak --map`.
//!
//! Unlike the normal mode this wizard never reads `config.json` and never
//! executes actions. It walks the user through picking a device, confirming
//! (or entering) the key/encoder counts, reporting how many buttons have
//! screens, capturing the raw code each physical button, encoder twist and
//! encoder push produces, and a manual display sanity check. Counts and codes
//! are typed as numbers, confirmations as `y`/`yes`/`n`/`no`; the collected
//! mapping is printed as JSON to stdout, then the device screens are cleared
//! and the device is shut down.

use std::collections::HashSet;
use std::fmt::Debug;
use std::io::{self, Write};

use async_hid::DeviceId as HidDeviceId;
use mirajazz::{
    device::{list_devices, Device},
    error::MirajazzError,
    types::{DeviceInput, HidDevice, ImageFormat},
};
use serde::{Deserialize, Serialize};

use crate::actions::ButtonDevice;
use crate::hardware::{self, Kind};
use crate::input::InputSource;
use crate::log::{Log, Subsystem};

/// One line each describing what `mirajazz`'s protocol versions 0-3 mean, shown to
/// the user before `run_map_wizard` asks them to pick one. See
/// `vendor/mirajazz-freebsd/README.md`'s "Protocol versions" section for the full
/// low-level detail this summarizes.
///
/// Deliberately does not mention "long press"/"short press"/PTT at all, unlike that
/// upstream README: the raw protocol capability some versions lack (an extra
/// "held"/PTT input state) is *not* what dak's own long-press/short-press/
/// double-click detection depends on - that's computed entirely in software from
/// press/release timing (`press.rs`) and works identically regardless of protocol
/// version. `run_device`/`run_map_wizard` also unconditionally request
/// `with_supports_both_keypress_states(true)` on every connection, so this
/// capability isn't even consulted for its intended purpose here.
const PROTOCOL_VERSION_DESCRIPTIONS: [(usize, &str); 4] = [
    (
        0,
        "oldest firmware fallback; 512-byte packets, no unique serial number reported",
    ),
    (1, "512-byte packets, hardcoded/shared serial number"),
    (
        2,
        "1024-byte packets, unique serial numbers (this project's own tested device uses this)",
    ),
    (
        3,
        "1024-byte packets, unique serial numbers, an extra raw \"held\" input state some \
         firmwares report (not used by dak's own long/short-press detection, which is timed \
         in software and identical across every version)",
    ),
];

/// Everything the wizard learned about one device.
///
/// [`Deserialize`] is derived so the same structure can be pasted into the config's
/// `devices` section to declare a device.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Mapping {
    /// VID:PID as an "XXXX:XXXX" string.
    pub device_id: String,
    /// Device name reported by the USB stack.
    pub device_name: String,
    /// Serial number of the device.
    pub serial: String,
    /// Key count confirmed by the user.
    pub key_count: u8,
    /// Encoder count confirmed by the user.
    pub encoder_count: u8,
    /// How many of the keys have a display.
    pub screens: u8,
    /// Protocol version to connect with (see `mirajazz::device::Device::connect`).
    /// `--map` fills this in with the value its recognized [`hardware::Kind`] uses
    /// (see [`hardware::Kind::protocol_version`]); absent (or explicit `null`) in an
    /// older config, or a config written by hand, falls back to that same
    /// recognized default at connect time. Set this to override it - e.g. for a
    /// device kind this project has not verified itself, if a different protocol
    /// version turns out to work better for your specific unit.
    #[serde(default)]
    pub protocol_version: Option<usize>,
    /// Per-device override of `defaults.device_reconnect_interval` (seconds between
    /// reconnect attempts after this device disappears); `None` uses the default.
    /// Validated by the config loader, never written by `--map`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_reconnect_interval: Option<u64>,
    /// Per-device override of `defaults.device_reconnect_max_attempts` (0 = retry
    /// forever); `None` uses the default. Validated by the config loader, never
    /// written by `--map`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_reconnect_max_attempts: Option<u64>,
    /// Raw codes per physical button, in the order the user pressed them.
    pub buttons: Vec<ButtonMapping>,
    /// Raw turn and push codes per encoder, in the order the user mapped them.
    pub encoders: Vec<EncoderMapping>,
}

/// Mapping of one physical button.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ButtonMapping {
    /// Logical button number (1-based), the order the user pressed them in.
    pub number: u8,
    /// Raw code reported while the button is pressed (`data[9]`, `data[10] != 0`).
    pub press: u8,
    /// Raw code reported when the button is released (`data[9]`, `data[10] == 0`).
    pub release: u8,
    /// Whether this button has a display.
    pub screen: bool,
    /// The number drawn on this button's display (`= number`) or `-1` when the
    /// button has no display and nothing was drawn on it.
    pub draw_id: i8,
}

/// Mapping of one encoder's twist and push codes.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct EncoderMapping {
    /// Logical encoder number (1-based), the order the user turned them in.
    pub number: u8,
    /// Raw code observed for the first turn of the knob.
    pub cw: u8,
    /// Raw code observed for the second turn (the other direction).
    pub ccw: u8,
    /// Raw code reported while the pushed knob is held down (`data[10] != 0`).
    /// Zero means the push was never captured (older configs): the knob's push
    /// is then ignored at runtime.
    #[serde(default)]
    pub press: u8,
    /// Raw code reported when a pushed knob is released (`data[10] == 0`).
    /// Zero means the push was never captured (older configs). Press and
    /// release are most often the same code.
    #[serde(default)]
    pub release: u8,
}

/// Direction of one encoder turn, from the knob's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwistDirection {
    /// One notch clockwise.
    Clockwise,
    /// One notch counter-clockwise.
    CounterClockwise,
}

/// What a raw report code addresses and how the report is read.
///
/// Buttons and pushed encoders are press/release controls (a non-zero `data[10]`
/// marks the down edge); an encoder turn is a discrete notch event with no
/// release edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// A keypad button's press or release edge.
    Button { number: u8 },
    /// An encoder knob's push (down or up edge), acting like a button.
    EncoderPress { number: u8 },
    /// One notch of an encoder wheel.
    EncoderTurn {
        number: u8,
        direction: TwistDirection,
    },
}

impl Mapping {
    /// Resolves a raw report code to the logical button number of the button
    /// whose captured press code (while held) or release code (when released)
    /// matches it. Returns `None` when no button in the definition uses that
    /// code, which is expected for unreported noise.
    ///
    /// The raw codes are device-specific — buttons without a display commonly
    /// report codes far above their number — so the runtime never treats a
    /// code as a button number; it always resolves through the mapping.
    pub fn button_number(&self, code: u8, pressed: bool) -> Option<u8> {
        self.buttons
            .iter()
            .find(|button| {
                if pressed {
                    button.press == code
                } else {
                    button.release == code
                }
            })
            .map(|button| button.number)
    }

    /// Resolves a raw report code to the [`ControlEvent`] it represents, looking
    /// up button edges, encoder pushes and encoder turns in that order. Returns
    /// `None` when no control in the definition uses the code, which is expected
    /// for unreported noise.
    ///
    /// The wizard captures only distinct codes per device, so a code normally
    /// addresses at most one control; a hand-written config may collide, and then
    /// buttons win over pushes, pushes over turns. An encoder without captured
    /// push codes (both zero, as in configs written before the wizard learned to
    /// capture knob pushes) never reports a push, so the raw code `0` some
    /// firmware sends when nothing is pressed stays inert.
    pub fn control_event(&self, code: u8, pressed: bool) -> Option<ControlEvent> {
        if let Some(number) = self.button_number(code, pressed) {
            return Some(ControlEvent::Button { number });
        }
        // A push code of 0 means "not captured" and never matches, even when another
        // encoder has real push codes: code 0 is what some firmware (or a short report)
        // sends when nothing is pressed.
        if let Some(encoder) = self
            .encoders
            .iter()
            .find(|encoder| code != 0 && (encoder.press == code || encoder.release == code))
        {
            return Some(ControlEvent::EncoderPress {
                number: encoder.number,
            });
        }
        if let Some(encoder) = self.encoders.iter().find(|encoder| encoder.cw == code) {
            return Some(ControlEvent::EncoderTurn {
                number: encoder.number,
                direction: TwistDirection::Clockwise,
            });
        }
        if let Some(encoder) = self.encoders.iter().find(|encoder| encoder.ccw == code) {
            return Some(ControlEvent::EncoderTurn {
                number: encoder.number,
                direction: TwistDirection::CounterClockwise,
            });
        }
        None
    }
}

/// Why the `dak --map` wizard ended without a mapping.
#[derive(Debug)]
pub enum MapError {
    /// The device could not be found or talked to.
    Device(MirajazzError),
    /// The chosen device is held by a running `dak` (already reported).
    Busy,
    /// The answers ran out (standard input was closed) before the wizard was done.
    InputClosed,
    /// Reading an answer failed.
    Input(io::Error),
}

impl From<MirajazzError> for MapError {
    /// Device errors pass through `?` unchanged.
    fn from(error: MirajazzError) -> Self {
        MapError::Device(error)
    }
}

impl std::fmt::Display for MapError {
    /// A one-line description for the error log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::Device(error) => write!(f, "{error}"),
            MapError::Busy => write!(f, "the device is in use by another dak"),
            MapError::InputClosed => {
                write!(
                    f,
                    "input ended before the mapping was complete; nothing was written"
                )
            }
            MapError::Input(error) => write!(f, "failed to read input: {error}"),
        }
    }
}

/// Where the wizard reads its answers from and writes its prompts and complaints to:
/// the terminal in a real run, in-memory buffers in tests.
///
/// Every question re-asks after an invalid answer, but none of them re-asks forever
/// once the input is gone: end of input (a closed or exhausted stdin) and read errors
/// end the wizard with [`MapError::InputClosed`]/[`MapError::Input`] - except
/// [`Console::ask_number_with_default`], for which end of input means "keep the
/// default". Output write errors are ignored (like a terminal that went away), so
/// they can never loop either.
pub struct Console<R, O, E> {
    /// Where answers are read from, one line each.
    input: R,
    /// Where prompts and progress go (stdout).
    out: O,
    /// Where complaints about invalid answers go (stderr).
    err: E,
}

impl Console<io::StdinLock<'static>, io::Stdout, io::Stderr> {
    /// The terminal: stdin, stdout and stderr.
    pub fn stdio() -> Self {
        Console::new(io::stdin().lock(), io::stdout(), io::stderr())
    }
}

impl<R: io::BufRead, O: Write, E: Write> Console<R, O, E> {
    /// A console over the given streams.
    pub fn new(input: R, out: O, err: E) -> Self {
        Self { input, out, err }
    }

    /// Writes one line of output.
    fn say(&mut self, text: impl std::fmt::Display) {
        let text = crate::log::escape_controls(&text.to_string());
        let _ = writeln!(self.out, "{text}");
    }

    /// Writes one line to the complaint stream.
    fn complain(&mut self, text: impl std::fmt::Display) {
        let text = crate::log::escape_controls(&text.to_string());
        let _ = writeln!(self.err, "{text}");
    }

    /// Shows `prompt` (without a newline) and reads one answer line: `Ok(None)` at end
    /// of input, an error when reading fails.
    fn answer(&mut self, prompt: &str) -> Result<Option<String>, MapError> {
        let _ = write!(self.out, "{prompt}");
        if let Err(error) = self.out.flush() {
            self.complain(format!("failed to flush stdout: {error}"));
        }
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line)),
            Err(error) => Err(MapError::Input(error)),
        }
    }

    /// [`Console::answer`] for a question that cannot be skipped: end of input is
    /// [`MapError::InputClosed`].
    fn required_answer(&mut self, prompt: &str) -> Result<String, MapError> {
        self.answer(prompt)?.ok_or(MapError::InputClosed)
    }

    /// Asks for a number in `min..=max`, asking again until a valid one is typed.
    fn ask_number(&mut self, prompt: &str, min: u64, max: u64) -> Result<u64, MapError> {
        loop {
            let line = self.required_answer(&format!("{prompt} [{min}..{max}]: "))?;
            match parse_number(&line) {
                Some(number) if (min..=max).contains(&number) => return Ok(number),
                _ => self.complain(format!("enter a number from {min} to {max}")),
            }
        }
    }

    /// Asks for a number in `min..=max`, or just Enter to keep `default`. End of input
    /// also keeps the default (the question has a safe answer), unlike every other
    /// question.
    fn ask_number_with_default(
        &mut self,
        prompt: &str,
        min: u64,
        max: u64,
        default: u64,
    ) -> Result<u64, MapError> {
        loop {
            let prompt = format!("{prompt} [{min}..{max}, default {default}, Enter to keep it]: ");
            let Some(line) = self.answer(&prompt)? else {
                return Ok(default);
            };
            match parse_number_or_default(&line, default, min, max) {
                Some(number) => return Ok(number),
                None => self.complain(format!(
                    "enter a number from {min} to {max}, or press Enter to keep {default}"
                )),
            }
        }
    }

    /// Asks a yes/no question (`y`/`yes`/`1` or `n`/`no`/`0`), asking again until a
    /// valid answer is typed.
    fn confirm(&mut self, prompt: &str) -> Result<bool, MapError> {
        loop {
            let line = self.required_answer(&format!("{prompt}: enter y/yes or n/no: "))?;
            match parse_yes_no(&line) {
                Some(answer) => return Ok(answer),
                None => self.complain("enter y/yes or n/no"),
            }
        }
    }

    /// Asks for a single key number in `1..=max`; a "no"-style answer (`no`, `n`,
    /// `none`, `0`) means the number in question is not shown on any key.
    fn ask_key_number(&mut self, prompt: &str, max: u8) -> Result<Option<u8>, MapError> {
        loop {
            let line = self.required_answer(&format!(
                "{prompt} [key number, or 'no' if not on any button]: "
            ))?;
            if let Some(key) = parse_key_number(&line, max) {
                return Ok(Some(key));
            }
            if is_no(&line) {
                return Ok(None);
            }
            self.complain(format!("enter a key number from 1 to {max} or 'no'"));
        }
    }

    /// Asks for a comma/space-separated list of key numbers in `1..=max`; `none` (or
    /// an empty line) is the empty list.
    fn ask_key_list(&mut self, prompt: &str, max: u8) -> Result<Vec<u8>, MapError> {
        loop {
            let line = self.required_answer(&format!("{prompt}: "))?;
            match parse_key_list(&line, max) {
                Some(keys) => return Ok(keys),
                None => self.complain(format!(
                    "enter key numbers from 1 to {max}, separated by spaces (or 'none')"
                )),
            }
        }
    }
}

/// Runs the whole `dak --map` wizard on the terminal: device selection, count
/// confirmation, screen count, button and encoder capture, display sanity check, JSON
/// output, and screen clearing/shutdown.
///
/// The chosen device is locked like a normal run would (see [`crate::lock`]), so the
/// wizard never captures input from a keypad a running `dak` is driving.
pub async fn run_map_wizard(log: Log) -> Result<(), MapError> {
    let mut console = Console::stdio();
    log.info("DAK device-mapping wizard (config is not read, no actions run)");

    // Step 1: numbered list of detected devices, user picks one by number.
    let devices: Vec<HidDevice> = list_devices(&hardware::QUERIES)
        .await?
        .into_iter()
        .collect();
    if devices.is_empty() {
        log.error("no compatible devices found");
        return Err(MirajazzError::DeviceNotFoundError.into());
    }
    let details: Vec<String> = devices
        .iter()
        .map(|dev| {
            device_details(
                &dev.id,
                dev.vendor_id,
                dev.product_id,
                &dev.serial_number,
                &dev.name,
            )
        })
        .collect();
    let dev = &devices[choose_device(&mut console, &details)?];

    let key = crate::lock::DeviceKey::new(
        dev.vendor_id,
        dev.product_id,
        dev.serial_number.as_deref(),
        &format!("{:?}", dev.id),
    );
    let _lock = match crate::lock::try_lock(&crate::lock::lock_dir(), &key) {
        Ok(lock) => lock,
        Err(crate::lock::LockError::Busy(holder)) => {
            let holder = holder
                .map(|holder| holder.describe())
                .unwrap_or_else(|| "another dak".to_string());
            log.error(format!(
                "device {} is in use by {holder}; stop it before mapping the device",
                key.describe()
            ));
            return Err(MapError::Busy);
        }
        Err(error) => {
            log.error(error.describe(&key));
            return Err(MapError::Busy);
        }
    };

    // Every dev reaching this point already matched hardware::QUERIES above, so this
    // should always resolve; treated as a hard error rather than assumed, in case
    // that invariant is ever broken.
    let Some(kind) = Kind::from_vid_pid(dev.vendor_id, dev.product_id) else {
        log.error(format!(
            "unrecognized vendor/product ID {:04x}:{:04x}",
            dev.vendor_id, dev.product_id
        ));
        return Err(MirajazzError::DeviceNotFoundError.into());
    };
    let protocol_version = choose_protocol_version(&mut console, kind)?;

    // Step 2: connect.
    console.say(format!(
        "Connecting to {}...",
        device_summary(
            &dev.id,
            dev.vendor_id,
            dev.product_id,
            &dev.serial_number,
            &dev.name
        )
    ));
    let device = Device::connect(
        dev,
        protocol_version,
        hardware::DEFAULT_KEY_COUNT,
        hardware::DEFAULT_ENCODER_COUNT,
    )
    .await?;
    let device = device.with_supports_both_keypress_states(true);
    let device = device.with_supports_both_encoder_states(true);

    // Mirror the normal mode: these calls double as the device-initialization
    // handshake. Without it the keypad stays silent and reports nothing, which
    // would make the capture steps hang forever.
    device.set_brightness(50).await?;
    device.clear_all_button_images().await?;

    let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));
    let captured = map_connected(
        &mut console,
        &device,
        device.encoder_count() as u8,
        &reader,
        kind.image_format(),
        log,
    )
    .await?;
    drop(reader);

    // Finished: emit the mapping as JSON, then clear the screens and end.
    let mapping = Mapping {
        device_id: format!("{:04X}:{:04X}", dev.vendor_id, dev.product_id),
        device_name: dev.name.clone(),
        serial: dev
            .serial_number
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        key_count: captured.key_count,
        encoder_count: captured.encoder_count,
        screens: captured.screens,
        protocol_version: Some(protocol_version),
        device_reconnect_interval: None,
        device_reconnect_max_attempts: None,
        buttons: captured.buttons,
        encoders: captured.encoders,
    };
    console.say(mapping_json(&mapping));

    device.clear_all_button_images().await?;
    device.flush().await?;
    device.shutdown().await?;
    log.debug(Subsystem::Device, "screens cleared, device shut down");
    Ok(())
}

/// Renders a [`Mapping`] as JSON in the compact form the shipped example
/// config uses: a pretty outer object with one line per button and per
/// encoder. Unlike `serde_json`'s pretty printing, this keeps the
/// one-`devices`-entry shape that drops straight into a config file.
fn mapping_json(mapping: &Mapping) -> String {
    let mut lines = String::from("{\n");
    for field in ["device_id", "device_name", "serial"] {
        let value: &str = match field {
            "device_id" => &mapping.device_id,
            "device_name" => &mapping.device_name,
            _ => &mapping.serial,
        };
        lines.push_str(&format!(
            "  \"{field}\": {},\n",
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
        ));
    }
    for (field, value) in [
        ("key_count", mapping.key_count as u64),
        ("encoder_count", mapping.encoder_count as u64),
        ("screens", mapping.screens as u64),
    ] {
        lines.push_str(&format!("  \"{field}\": {value},\n"));
    }
    lines.push_str(&format!(
        "  \"protocol_version\": {},\n",
        match mapping.protocol_version {
            Some(version) => version.to_string(),
            None => "null".to_string(),
        }
    ));
    lines.push_str("  \"buttons\": [\n");
    for (index, button) in mapping.buttons.iter().enumerate() {
        let comma = if index + 1 == mapping.buttons.len() {
            ""
        } else {
            ","
        };
        lines.push_str(&format!(
            "    {{ \"number\": {}, \"press\": {}, \"release\": {}, \"screen\": {}, \"draw_id\": {} }}{comma}\n",
            button.number, button.press, button.release, button.screen, button.draw_id
        ));
    }
    lines.push_str("  ],\n");
    lines.push_str("  \"encoders\": [\n");
    for (index, encoder) in mapping.encoders.iter().enumerate() {
        let comma = if index + 1 == mapping.encoders.len() {
            ""
        } else {
            ","
        };
        lines.push_str(&format!(
            "    {{ \"number\": {}, \"cw\": {}, \"ccw\": {}, \"press\": {}, \"release\": {} }}{comma}\n",
            encoder.number, encoder.cw, encoder.ccw, encoder.press, encoder.release
        ));
    }
    lines.push_str("  ]\n}");
    lines
}

/// The OS device path from a raw `DeviceId`.
///
/// On Linux the id holds a `PathBuf` (e.g. `/dev/hidraw3`); other platforms
/// (FreeBSD here) have no path and fall back to the `Debug` rendering. The
/// enum is `#[non_exhaustive]`, which is why the wildcard arm is required.
fn device_path(id: &HidDeviceId) -> String {
    match id {
        HidDeviceId::DevPath(path) => path.display().to_string(),
        _ => format!("{id:?}"),
    }
}

/// One-line summary of a detected device, used e.g. for the connect message.
///
/// The id is passed as a `Debug` value because its concrete type (the
/// platform-specific `DeviceId` behind `HidDeviceInfo.id`) is not re-exported
/// by mirajazz; this keeps the function constructible in tests.
fn device_summary(
    id: &dyn Debug,
    vid: u16,
    pid: u16,
    serial: &Option<String>,
    name: &str,
) -> String {
    let serial = crate::log::escape_text(serial.as_deref().unwrap_or("unknown"));
    let name = crate::log::escape_text(name);
    format!("{vid:04X}:{pid:04X} path {id:?} serial {serial} \"{name}\"")
}

/// Multi-line description of one detected device for the numbered picker list:
/// name, serial, vendor/product id, and the OS device path.
fn device_details(
    id: &HidDeviceId,
    vid: u16,
    pid: u16,
    serial: &Option<String>,
    name: &str,
) -> String {
    let serial = crate::log::escape_text(serial.as_deref().unwrap_or("unknown"));
    let name = crate::log::escape_text(name);
    let path = crate::log::escape_text(&device_path(id));
    format!(
        "Device name: {name}\nSerial: {serial}\nVendorID/DeviceID {vid:04X}:{pid:04X}\nDevice Path: {path}"
    )
}

/// Parses a single unsigned number from a line of user input.
fn parse_number(input: &str) -> Option<u64> {
    input.trim().parse().ok()
}

/// Pure state machines behind the raw device capture loops.
///
/// Both machines are deliberately free of I/O: they only consume `(code, state)`
/// pairs, so the tricky event-filtering logic can be unit tested without a
/// device or async reader.
mod capture {
    use super::*;

    /// Pure state machine capturing a single press-and-release gesture.
    ///
    /// Starts empty; `feed` returns `Some((press, release))` once a code not
    /// present in `exclude` has been observed pressed and then released (a
    /// state `0` report of the same code). Firmware differs in how it signals
    /// a press: some report `(code, 1)`, some only `(code, 0)`, so any fresh
    /// code is accepted as the press regardless of its state. Code `0` is the
    /// reserved "released, nothing pressed" sentinel and is never accepted,
    /// like the already-assigned codes in `exclude`. If a different fresh code
    /// arrives while a gesture is held, the held code's release was missed and
    /// the new code takes over as the press under observation.
    pub struct PressCapture {
        exclude: HashSet<u8>,
        held: Option<u8>,
    }

    impl PressCapture {
        /// A capture that refuses to match any code in `exclude`.
        pub fn new(exclude: HashSet<u8>) -> Self {
            Self {
                exclude,
                held: None,
            }
        }

        /// Feeds one observed `(code, state)` event and returns the completed
        /// `(press, release)` pair once the gesture finishes.
        pub fn feed(&mut self, code: u8, state: u8) -> Option<(u8, u8)> {
            if code == 0 || self.exclude.contains(&code) {
                return None;
            }
            match self.held {
                Some(held) if code == held && state == 0 => {
                    self.held = None;
                    Some((held, code))
                }
                Some(_) => {
                    self.held = Some(code);
                    None
                }
                None => {
                    self.held = Some(code);
                    None
                }
            }
        }
    }

    /// Pure state machine capturing the two turn codes of one encoder.
    ///
    /// Turn events arrive as `(code, 0)` reports (nothing pressed). The first
    /// fresh code is taken as the clockwise turn; repeats of it (the extra notches
    /// of a turn that went further than one) are ignored, and the first *different*
    /// fresh code is the counter-clockwise turn. So the user may turn each way by
    /// as many notches as they like, with no timing involved: only the change of
    /// code separates the directions.
    ///
    /// A knob push also ends in a `(code, 0)` report - its release - so any code
    /// seen pressed (`state != 0`) is a push code and never taken as a turn.
    /// (Pushing during the turn step, after a notch the device did not report,
    /// once recorded the push's release as the second direction.)
    pub struct TwistCapture {
        exclude: HashSet<u8>,
        first: Option<u8>,
        /// Codes seen with a pressed state: push codes, not turns.
        pressed: HashSet<u8>,
    }

    impl TwistCapture {
        /// A capture that refuses to match any code in `exclude`.
        pub fn new(exclude: HashSet<u8>) -> Self {
            Self {
                exclude,
                first: None,
                pressed: HashSet::new(),
            }
        }

        /// Feeds one observed `(code, state)` event and returns `Some((cw, ccw))`
        /// once two distinct fresh turn codes have been seen.
        pub fn feed(&mut self, code: u8, state: u8) -> Option<(u8, u8)> {
            if state != 0 {
                self.pressed.insert(code);
                return None;
            }
            if self.exclude.contains(&code) || self.pressed.contains(&code) {
                return None;
            }
            match self.first {
                None => {
                    self.first = Some(code);
                    None
                }
                Some(first) if first != code => Some((first, code)),
                Some(_) => None,
            }
        }
    }
}

/// Step 1: lists the detected devices (each described by a multi-line `details`
/// text, numbered from 1) and asks which one to map; returns its index.
fn choose_device<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    details: &[String],
) -> Result<usize, MapError> {
    console.say("Detected devices:");
    for (index, text) in details.iter().enumerate() {
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            console.say(format!("  {}. {first}", index + 1));
        }
        for line in lines {
            console.say(format!("     {line}"));
        }
    }
    let pick = console.ask_number("device to work on", 1, details.len() as u64)? as usize - 1;
    console.say("");
    Ok(pick)
}

/// Reports what `kind` was recognized as and asks which protocol version to connect
/// with, defaulting to the one `kind` uses.
///
/// The protocol version has to be settled before connecting (unlike the counts, which
/// are read back from an already-connected device and can be corrected afterwards): it
/// changes how mirajazz talks to the device at the wire level, so whatever value ends
/// up used here is also what every later capture step runs under.
fn choose_protocol_version<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    kind: Kind,
) -> Result<usize, MapError> {
    console.say(format!("Recognized as: {}", kind.human_name()));
    if !matches!(kind, Kind::Akp03ERev2) {
        console.say(
            "Note: this device kind has not been verified against real hardware by DAK - see the README's \"Help me support more devices\" section.",
        );
    }
    console.say("Protocol versions:");
    for (version, description) in PROTOCOL_VERSION_DESCRIPTIONS {
        console.say(format!("  {version}: {description}"));
    }
    let version = console.ask_number_with_default(
        "protocol version to connect with",
        0,
        3,
        kind.protocol_version() as u64,
    )? as usize;
    console.say("");
    Ok(version)
}

/// Everything the wizard learned from a connected device (steps 2 to 6).
#[derive(Debug, PartialEq)]
struct Captured {
    /// The confirmed key count.
    key_count: u8,
    /// The confirmed encoder count.
    encoder_count: u8,
    /// How many keys have a screen.
    screens: u8,
    /// Each button's codes and display, in logical order.
    buttons: Vec<ButtonMapping>,
    /// Each encoder's codes, in logical order.
    encoders: Vec<EncoderMapping>,
}

/// Steps 2 to 6 on a connected, initialized device: confirm (or enter) the key and
/// encoder counts the device reports (`device.key_count()`, `reported_encoders`), ask
/// how many keys have screens, capture every button's and encoder's codes from
/// `reader`, paint the logical numbers and let the user correct the display mapping.
async fn map_connected<R, O, E, D, I>(
    console: &mut Console<R, O, E>,
    device: &D,
    reported_encoders: u8,
    reader: &I,
    image_format: ImageFormat,
    log: Log,
) -> Result<Captured, MapError>
where
    R: io::BufRead,
    O: Write,
    E: Write,
    D: ButtonDevice<Error = MirajazzError>,
    I: InputSource,
{
    let reported_keys = device.key_count();
    console.say(format!(
        "mirajazz reports {reported_keys} keys and {reported_encoders} encoders."
    ));
    let key_count = if console.confirm("is the key count correct")? {
        reported_keys
    } else {
        console.ask_number("number of keys", 1, u8::MAX as u64)? as u8
    };
    let encoder_count = if console.confirm("is the encoder count correct")? {
        reported_encoders
    } else {
        console.ask_number("number of encoders", 0, u8::MAX as u64)? as u8
    };
    console.say("");

    // Step 3: how many buttons have screens.
    let screens = console.ask_number("how many buttons have screens", 0, key_count as u64)? as u8;
    console.say("");

    // Step 4: capture one physical button at a time, in the order the user presses
    // them. Codes already assigned to an earlier button are skipped.
    let mut buttons = Vec::new();
    let mut used: HashSet<u8> = HashSet::new();
    for number in 1..=key_count {
        console.say(format!(
            "press button {number}: press it, hold it, then release it"
        ));
        let (press, release) = capture_press_release(console, reader, &used).await?;
        console.say(format!(
            "button {number} -> press code {press}, release code {release}"
        ));
        used.insert(press);
        used.insert(release);
        buttons.push(ButtonMapping {
            number,
            press,
            release,
            screen: number <= screens,
            draw_id: if number <= screens { number as i8 } else { -1 },
        });
    }
    console.say("");

    // Step 5: capture one encoder at a time: its two turn codes (separated by the
    // change of code, see `capture::TwistCapture`), then its push/release codes -
    // encoder knobs are pushed like buttons.
    let mut encoders = Vec::new();
    for number in 1..=encoder_count {
        console.say(format!(
            "turn encoder {number}: clockwise (one or more notches), then counter-clockwise"
        ));
        let (cw, ccw) = capture_encoder_twists(console, reader, &used).await?;
        console.say(format!(
            "encoder {number} -> clockwise code {cw}, counter-clockwise code {ccw}"
        ));
        used.insert(cw);
        used.insert(ccw);
        console.say(format!(
            "press encoder {number}: push the knob, hold it, then release it"
        ));
        let (press, release) = capture_press_release(console, reader, &used).await?;
        console.say(format!(
            "encoder {number} -> push code {press}, release code {release}"
        ));
        used.insert(press);
        used.insert(release);
        encoders.push(EncoderMapping {
            number,
            cw,
            ccw,
            press,
            release,
        });
    }
    console.say("");

    // Step 6: paint the logical button number on every button (including the ones
    // without a display, which stay dark) and ask the user to verify.
    console.say("painting button numbers on every button...");
    for (index, number) in (1..=key_count).enumerate() {
        let image = match crate::text::render_text(&[number.to_string()], image_format) {
            Ok(image) => image,
            Err(error) => {
                log.error(format!("failed to render test label: {error}"));
                return Err(MirajazzError::BadData.into());
            }
        };
        device
            .set_button_image(index as u8, image_format, image)
            .await?;
    }
    device.flush().await?;
    console.say("");

    recheck_display(console, &mut buttons, key_count)?;
    console.say("");
    Ok(Captured {
        key_count,
        encoder_count,
        screens,
        buttons,
        encoders,
    })
}

/// The end of step 6: asks whether the painted numbers match the physical layout and,
/// if not, which key shows each number and which keys show none, rewriting every
/// button's `screen`/`draw_id` from the answers.
fn recheck_display<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    buttons: &mut [ButtonMapping],
    key_count: u8,
) -> Result<(), MapError> {
    if console
        .confirm("do the painted button numbers match your physical layout (1..N in order)")?
    {
        return Ok(());
    }
    console
        .say("display recheck: for every number painted on a screen, name the key that shows it");
    // shown[i] = number displayed on key `i` (0-based), None if unlit.
    let mut shown: Vec<Option<i8>> = vec![None; buttons.len()];
    for number in 1..=key_count {
        let question = format!("which key number has number {number} displayed");
        if let Some(key) = console.ask_key_number(&question, key_count)? {
            let Some(slot) = shown.get_mut((key - 1) as usize) else {
                continue;
            };
            if slot.is_none() {
                *slot = Some(number as i8);
            } else {
                console.complain(format!(
                    "key {key} already shows another number; keeping the first answer"
                ));
            }
        }
    }
    let unlit = console.ask_key_list(
        "enter the key numbers without any number displayed (or 'none')",
        key_count,
    )?;
    for key in unlit {
        if let Some(slot) = shown.get_mut((key - 1) as usize) {
            *slot = None;
        }
    }
    for (button, shown) in buttons.iter_mut().zip(shown) {
        match shown {
            Some(draw) => {
                button.screen = true;
                button.draw_id = draw;
            }
            None => {
                button.screen = false;
                button.draw_id = -1;
            }
        }
    }
    Ok(())
}

/// Reads raw device reports until the `PressCapture` records a full
/// press-and-release for a fresh button.
async fn capture_press_release<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    reader: &impl InputSource,
    exclude: &HashSet<u8>,
) -> Result<(u8, u8), MirajazzError> {
    let mut capture = capture::PressCapture::new(exclude.clone());
    loop {
        let (code, state) = next_event(console, reader).await?;
        if let Some(done) = capture.feed(code, state) {
            return Ok(done);
        }
    }
}

/// Reads raw device reports until the `TwistCapture` records two distinct
/// turn codes for one encoder.
async fn capture_encoder_twists<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    reader: &impl InputSource,
    exclude: &HashSet<u8>,
) -> Result<(u8, u8), MirajazzError> {
    let mut capture = capture::TwistCapture::new(exclude.clone());
    loop {
        let (code, state) = next_event(console, reader).await?;
        if let Some(done) = capture.feed(code, state) {
            return Ok(done);
        }
    }
}

/// Reads one report from the device and extracts its `(code, state)` pair,
/// skipping non-ACK noise reports. Every observed event is echoed so the user sees
/// that the device is responding during a capture.
async fn next_event<R: io::BufRead, O: Write, E: Write>(
    console: &mut Console<R, O, E>,
    reader: &impl InputSource,
) -> Result<(u8, u8), MirajazzError> {
    loop {
        let data = reader.read_report().await?;
        if let Some(event) = crate::input::decode_report(&data) {
            console.say(format!("  observed key {:>3}, state {}", event.0, event.1));
            return Ok(event);
        }
    }
}

/// Parses one typed line for a number question that has a default: an empty
/// (whitespace-only) line means "keep the default" (`Some(default)`), a valid
/// number in `min..=max` is used as typed, anything else is invalid (`None`).
fn parse_number_or_default(input: &str, default: u64, min: u64, max: u64) -> Option<u64> {
    if input.trim().is_empty() {
        return Some(default);
    }
    match parse_number(input) {
        Some(number) if (min..=max).contains(&number) => Some(number),
        _ => None,
    }
}

/// Parses a yes/no answer: `y`, `yes` or `1` for yes; `n`, `no` or `0` for no.
fn parse_yes_no(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "1" => Some(true),
        "n" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Parses one typed line into an optional key number in `1..=max`: a valid
/// number yields `Some`, a "no"-style answer `None`, anything else is invalid.
fn parse_key_number(input: &str, max: u8) -> Option<u8> {
    if is_no(input) {
        return None;
    }
    match parse_number(input) {
        Some(number) if (1..=max as u64).contains(&number) => Some(number as u8),
        _ => None,
    }
}

/// Parses one typed line into a list of key numbers; "none" or an empty line
/// yields an empty list, any out-of-range or non-numeric token is invalid.
fn parse_key_list(input: &str, max: u8) -> Option<Vec<u8>> {
    let line = input.trim();
    if line.is_empty() || is_no(line) {
        return Some(Vec::new());
    }
    let mut keys = Vec::new();
    for token in line.split(|c: char| c == ',' || c.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let key = parse_key_number(token, max)?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Some(keys)
}

/// Whether a typed answer is a "no"-style reply (`no`, `n`, `none`, `0`).
fn is_no(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "no" | "n" | "none" | "0"
    )
}

#[cfg(test)]
mod tests {
    use super::capture::{PressCapture, TwistCapture};
    use super::mapping_json;
    use super::HidDeviceId;
    use super::{
        device_details, device_summary, is_no, parse_key_list, parse_key_number, parse_number,
        parse_number_or_default, parse_yes_no,
    };
    use super::{ButtonMapping, ControlEvent, EncoderMapping, Mapping, TwistDirection};
    use crate::input::decode_report as raw_event;
    use crate::log::Log;
    use std::collections::HashSet;

    /// `parse_number` accepts surrounding whitespace and the decimal format the
    /// user types manually.
    #[test]
    fn parse_number_handles_whitespace_and_decimals() {
        assert_eq!(parse_number("  42 \n"), Some(42));
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("12.5"), None);
    }

    /// An empty (or whitespace-only) line means "keep the default"; a valid
    /// in-range number is used as typed; anything else (out of range, or not a
    /// number at all) is rejected so the caller re-prompts.
    #[test]
    fn parse_number_or_default_handles_empty_valid_and_invalid_input() {
        assert_eq!(parse_number_or_default("", 2, 0, 3), Some(2));
        assert_eq!(parse_number_or_default("   \n", 2, 0, 3), Some(2));
        assert_eq!(parse_number_or_default("3", 2, 0, 3), Some(3));
        assert_eq!(parse_number_or_default(" 0 \n", 2, 0, 3), Some(0));
        assert_eq!(parse_number_or_default("4", 2, 0, 3), None);
        assert_eq!(parse_number_or_default("abc", 2, 0, 3), None);
    }

    /// The shared report decoder the capture loops use extracts `(data[9], data[10])`
    /// from an ACK-prefixed report and rejects reports without the ACK prefix.
    #[test]
    fn raw_event_extracts_code_and_state_from_ack_reports() {
        let mut report = vec![0u8; 512];
        report[0] = 65;
        report[1] = 67;
        report[2] = 75;
        report[9] = 0x61;
        report[10] = 0;
        assert_eq!(raw_event(&report), Some((0x61, 0)));

        let noise = vec![0u8; 512];
        assert_eq!(raw_event(&noise), None);
    }

    /// The button gesture capture records a fresh code as the press, waits for
    /// the same code with state 0, and ignores already-mapped codes.
    #[test]
    fn press_capture_records_press_and_release() {
        let mut capture = PressCapture::new(HashSet::new());
        assert_eq!(capture.feed(5, 1), None); // pressed
        assert_eq!(capture.feed(5, 0), Some((5, 5))); // released
    }

    /// Some firmware reports presses with state 0 as well, so a fresh code is
    /// accepted as a press regardless of its state.
    #[test]
    fn press_capture_accepts_presses_reported_with_zero_state() {
        let mut capture = PressCapture::new(HashSet::new());
        assert_eq!(capture.feed(5, 0), None); // press reported with state 0
        assert_eq!(capture.feed(5, 0), Some((5, 5))); // release report
    }

    /// If the held button's release is missed and another fresh code arrives,
    /// the new code takes over as the press under observation.
    #[test]
    fn press_capture_supersedes_held_code_on_fresh_report() {
        let mut capture = PressCapture::new(HashSet::new());
        assert_eq!(capture.feed(0x61, 0), None); // an unrelated twist report
        assert_eq!(capture.feed(5, 1), None); // the real press supersedes it
        assert_eq!(capture.feed(5, 0), Some((5, 5)));
    }

    /// Code 0 is the reserved "released, nothing pressed" sentinel and can
    /// never start a gesture.
    #[test]
    fn press_capture_ignores_reserved_zero_code() {
        let mut capture = PressCapture::new(HashSet::new());
        assert_eq!(capture.feed(0, 1), None);
        assert_eq!(capture.feed(0, 0), None);
        assert_eq!(capture.feed(5, 0), None);
        assert_eq!(capture.feed(5, 0), Some((5, 5)));
    }

    /// A code already assigned to another button is never captured.
    #[test]
    fn press_capture_skips_excluded_codes() {
        let mut capture = PressCapture::new(HashSet::from([6]));
        assert_eq!(capture.feed(6, 1), None);
        assert_eq!(capture.feed(6, 0), None);
        assert_eq!(capture.feed(1, 1), None);
        assert_eq!(capture.feed(1, 0), Some((1, 1)));
    }

    /// Twist capture takes the first two distinct fresh state-0 codes as the two
    /// directions: extra notches of the first direction are ignored, pushes too.
    #[test]
    fn twist_capture_records_two_directions() {
        let mut capture = TwistCapture::new(HashSet::new());
        assert_eq!(capture.feed(0x33, 1), None); // a pressed-state event (a push)
        assert_eq!(capture.feed(0x90, 0), None); // first notch
        assert_eq!(capture.feed(0x90, 0), None); // more notches, same direction
        assert_eq!(capture.feed(0x90, 0), None);
        assert_eq!(capture.feed(0x91, 0), Some((0x90, 0x91)));
    }

    /// A knob push during the turn step (press then release of one code) is not
    /// taken as a turn: its release is a `(code, 0)` report like a turn, but the
    /// code was seen pressed. This happened on real hardware when a clockwise
    /// notch went unreported and the user went on to push the knob.
    #[test]
    fn twist_capture_ignores_a_knob_push() {
        let mut capture = TwistCapture::new(HashSet::new());
        assert_eq!(capture.feed(144, 0), None);
        assert_eq!(capture.feed(144, 0), None);
        assert_eq!(capture.feed(51, 1), None, "push");
        assert_eq!(capture.feed(51, 0), None, "its release is no turn");
        assert_eq!(capture.feed(145, 0), Some((144, 145)));
    }

    /// Twist capture never accepts codes that belong to controls already mapped.
    #[test]
    fn twist_capture_skips_button_codes() {
        let mut capture = TwistCapture::new(HashSet::from([0x61]));
        assert_eq!(capture.feed(0x61, 0), None);
        assert_eq!(capture.feed(0x60, 0), None);
        assert_eq!(capture.feed(0x33, 0), Some((0x60, 0x33)));
    }

    /// Raw codes resolve to the button number through the captured press and
    /// release codes, whether those codes match the number or not.
    #[test]
    fn button_number_resolves_raw_codes_to_logical_numbers() {
        let mapping = Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "Ajazz HOTSPOTEKUSB HID DEMO".to_string(),
            serial: "unknown".to_string(),
            key_count: 9,
            encoder_count: 3,
            screens: 6,
            protocol_version: None,
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![
                ButtonMapping {
                    number: 1,
                    press: 1,
                    release: 1,
                    screen: true,
                    draw_id: 1,
                },
                ButtonMapping {
                    number: 7,
                    press: 48,
                    release: 48,
                    screen: false,
                    draw_id: -1,
                },
            ],
            encoders: vec![],
        };
        assert_eq!(mapping.button_number(1, true), Some(1));
        assert_eq!(mapping.button_number(1, false), Some(1));
        assert_eq!(mapping.button_number(48, true), Some(7));
        assert_eq!(mapping.button_number(48, false), Some(7));
        assert_eq!(mapping.button_number(99, true), None);
        assert_eq!(mapping.button_number(99, false), None);
    }

    /// A button with distinct press and release codes resolves correctly in
    /// both directions.
    #[test]
    fn button_number_uses_distinct_press_and_release_codes() {
        let mapping = Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "t".to_string(),
            serial: "unknown".to_string(),
            key_count: 1,
            encoder_count: 0,
            screens: 1,
            protocol_version: None,
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![ButtonMapping {
                number: 3,
                press: 0x60,
                release: 0x61,
                screen: true,
                draw_id: 3,
            }],
            encoders: vec![],
        };
        assert_eq!(mapping.button_number(0x60, true), Some(3));
        assert_eq!(mapping.button_number(0x60, false), None);
        assert_eq!(mapping.button_number(0x61, true), None);
        assert_eq!(mapping.button_number(0x61, false), Some(3));
    }

    /// The mapping JSON matches the one-line-per-button/encoder example-config form
    /// (including the comma joining multiple encoder entries), escaping string
    /// fields like `serde_json` would.
    #[test]
    fn mapping_json_uses_compact_one_line_entries() {
        let mapping = Mapping {
            device_id: "0300:3002".to_string(),
            device_name: r#"HID "DEMO""#.to_string(),
            serial: "unknown".to_string(),
            key_count: 2,
            encoder_count: 1,
            screens: 1,
            protocol_version: Some(2),
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![
                ButtonMapping {
                    number: 1,
                    press: 1,
                    release: 1,
                    screen: true,
                    draw_id: 1,
                },
                ButtonMapping {
                    number: 2,
                    press: 48,
                    release: 48,
                    screen: false,
                    draw_id: -1,
                },
            ],
            encoders: vec![
                EncoderMapping {
                    number: 1,
                    cw: 81,
                    ccw: 80,
                    press: 79,
                    release: 79,
                },
                EncoderMapping {
                    number: 2,
                    cw: 83,
                    ccw: 82,
                    press: 78,
                    release: 78,
                },
            ],
        };
        assert_eq!(
            mapping_json(&mapping),
            concat!(
                "{\n",
                "  \"device_id\": \"0300:3002\",\n",
                "  \"device_name\": \"HID \\\"DEMO\\\"\",\n",
                "  \"serial\": \"unknown\",\n",
                "  \"key_count\": 2,\n",
                "  \"encoder_count\": 1,\n",
                "  \"screens\": 1,\n",
                "  \"protocol_version\": 2,\n",
                "  \"buttons\": [\n",
                "    { \"number\": 1, \"press\": 1, \"release\": 1, \"screen\": true, \"draw_id\": 1 },\n",
                "    { \"number\": 2, \"press\": 48, \"release\": 48, \"screen\": false, \"draw_id\": -1 }\n",
                "  ],\n",
                "  \"encoders\": [\n",
                "    { \"number\": 1, \"cw\": 81, \"ccw\": 80, \"press\": 79, \"release\": 79 },\n",
                "    { \"number\": 2, \"cw\": 83, \"ccw\": 82, \"press\": 78, \"release\": 78 }\n",
                "  ]\n",
                "}"
            )
        );
    }

    /// An absent `protocol_version` (the wizard never leaves it unset itself, but a
    /// hand-written or older config might) round-trips through the mapping JSON as
    /// a literal `null`, matching how a config author would spell "use the
    /// recognized kind's default" - not `0`, and not simply omitting the field
    /// (which would also default to `None` on the next load, but reads as an
    /// oversight rather than a deliberate choice when looking at emitted JSON).
    #[test]
    fn mapping_json_renders_absent_protocol_version_as_null() {
        let mapping = Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "t".to_string(),
            serial: "unknown".to_string(),
            key_count: 1,
            encoder_count: 0,
            screens: 0,
            protocol_version: None,
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![],
            encoders: vec![],
        };
        assert!(mapping_json(&mapping).contains("\"protocol_version\": null,\n"));
    }

    /// Debug-prints like the real Linux `DeviceId::DevPath`, so the summary
    /// assertion reads the way the actual output does.
    struct FakeDeviceId(&'static str);

    impl std::fmt::Debug for FakeDeviceId {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "DevPath({:?})", self.0)
        }
    }

    /// The device summary carries VID:PID, path, serial and name so the user
    /// can tell identical devices apart.
    #[test]
    fn device_summary_includes_all_identifying_details() {
        let line = device_summary(
            &FakeDeviceId("/dev/hidraw3"),
            0x0300,
            0x3002,
            &Some("ABC123".to_string()),
            "Ajazz HOTSPOTEKUSB HID DEMO",
        );
        assert_eq!(
            line,
            r#"0300:3002 path DevPath("/dev/hidraw3") serial ABC123 "Ajazz HOTSPOTEKUSB HID DEMO""#
        );
    }

    /// The picker list shows the human-readable details one per line, with the
    /// raw device path (`/dev/hidrawN`) instead of the `DevPath(...)` debug
    /// wrapper.
    #[test]
    fn device_details_are_one_field_per_line() {
        let id = HidDeviceId::DevPath("/dev/hidraw3".into());
        let line = device_details(&id, 0x0300, 0x3002, &Some("ABC123".to_string()), "MyPad");
        assert_eq!(
            line,
            "Device name: MyPad\nSerial: ABC123\nVendorID/DeviceID 0300:3002\nDevice Path: /dev/hidraw3"
        );
    }

    /// Missing serials are shown as "unknown" instead of being elided.
    #[test]
    fn device_details_fall_back_to_unknown_serial() {
        let id = HidDeviceId::DevPath("/dev/hidraw3".into());
        let line = device_details(&id, 0x0300, 0x3002, &None, "MyPad");
        assert!(line.contains("Serial: unknown"));
    }

    /// `parse_yes_no` understands `y`/`yes`/`1` (and upper case) and
    /// `n`/`no`/`0`, rejecting anything else.
    #[test]
    fn parse_yes_no_understands_text_and_numeric_answers() {
        assert_eq!(parse_yes_no("y"), Some(true));
        assert_eq!(parse_yes_no("YES"), Some(true));
        assert_eq!(parse_yes_no("  1 \n"), Some(true));
        assert_eq!(parse_yes_no("n"), Some(false));
        assert_eq!(parse_yes_no("No"), Some(false));
        assert_eq!(parse_yes_no("0"), Some(false));
        assert_eq!(parse_yes_no("maybe"), None);
        assert_eq!(parse_yes_no(""), None);
    }

    /// `parse_key_number` accepts one key in range, returns `None` for
    /// "no"-style answers, and rejects out-of-range and non-numeric input.
    #[test]
    fn parse_key_number_rejects_out_of_range_and_no_answers() {
        assert_eq!(parse_key_number("3", 9), Some(3));
        assert_eq!(parse_key_number(" 3 \n", 9), Some(3));
        assert_eq!(parse_key_number("no", 9), None);
        assert_eq!(parse_key_number("None", 9), None);
        assert_eq!(parse_key_number("0", 9), None);
        assert_eq!(parse_key_number("10", 9), None);
        assert_eq!(parse_key_number("abc", 9), None);
    }

    /// `parse_key_list` splits on spaces and commas, deduplicates, treats
    /// "none"/empty as an empty list, and rejects invalid tokens.
    #[test]
    fn parse_key_list_splits_deduplicates_and_handles_none() {
        assert_eq!(parse_key_list("1, 3  2", 9), Some(vec![1, 3, 2]));
        assert_eq!(parse_key_list("2 2 1", 9), Some(vec![2, 1]));
        assert_eq!(parse_key_list("none", 9), Some(vec![]));
        assert_eq!(parse_key_list("  ", 9), Some(vec![]));
        assert_eq!(parse_key_list("1 12", 9), None);
        assert_eq!(parse_key_list("1 x", 9), None);
    }

    /// `is_no` recognizes every "no"-style answer case-insensitively.
    #[test]
    fn is_no_recognizes_all_negative_forms() {
        for input in ["no", "No", "NO", "n", "none", "0"] {
            assert!(is_no(input), "expected {input:?} to be a no answer");
        }
        assert!(!is_no("yes"));
        assert!(!is_no("1"));
        assert!(!is_no(""));
    }

    /// `--map` output never carries the per-device reconnect overrides: neither the
    /// printed JSON nor the serde form, when they are unset.
    #[test]
    fn map_output_has_no_reconnect_settings() {
        let mapping = sample_mapping();
        assert!(!mapping_json(&mapping).contains("device_reconnect"));
        let value = serde_json::to_value(&mapping).unwrap();
        assert!(value.get("device_reconnect_interval").is_none());
        assert!(value.get("device_reconnect_max_attempts").is_none());
    }

    /// The mapping serializes to the documented JSON shape.
    #[test]
    fn mapping_serializes_as_expected_json() {
        let mapping = super::Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "Ajazz HOTSPOTEKUSB HID DEMO".to_string(),
            serial: "ABC123".to_string(),
            key_count: 9,
            encoder_count: 3,
            screens: 6,
            protocol_version: Some(2),
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![
                super::ButtonMapping {
                    number: 1,
                    press: 1,
                    release: 1,
                    screen: true,
                    draw_id: 1,
                },
                super::ButtonMapping {
                    number: 2,
                    press: 2,
                    release: 2,
                    screen: false,
                    draw_id: -1,
                },
            ],
            encoders: vec![super::EncoderMapping {
                number: 1,
                cw: 0x90,
                ccw: 0x91,
                press: 0x92,
                release: 0x92,
            }],
        };
        let value = serde_json::to_value(&mapping).unwrap();
        assert_eq!(value["device_id"], "0300:3002");
        assert_eq!(value["key_count"], 9);
        assert_eq!(value["screens"], 6);
        assert_eq!(value["protocol_version"], 2);
        assert_eq!(value["buttons"][0]["press"], 1);
        assert_eq!(value["buttons"][0]["screen"], true);
        assert_eq!(value["buttons"][0]["draw_id"], 1);
        assert_eq!(value["buttons"][1]["screen"], false);
        assert_eq!(value["buttons"][1]["draw_id"], -1);
        assert_eq!(value["encoders"][0]["cw"], 0x90);
        assert_eq!(value["encoders"][0]["ccw"], 0x91);
        assert_eq!(value["encoders"][0]["press"], 0x92);
        assert_eq!(value["encoders"][0]["release"], 0x92);
    }

    /// `Mapping::control_event` resolves a pressed/released button code to a
    /// button reference, including the large scan codes of screenless buttons.
    #[test]
    fn control_event_resolves_button_edges() {
        let mapping = sample_mapping();
        assert_eq!(
            mapping.control_event(3, true),
            Some(ControlEvent::Button { number: 3 })
        );
        assert_eq!(
            mapping.control_event(3, false),
            Some(ControlEvent::Button { number: 3 })
        );
        assert_eq!(
            mapping.control_event(37, true),
            Some(ControlEvent::Button { number: 7 })
        );
        assert_eq!(
            mapping.control_event(37, false),
            Some(ControlEvent::Button { number: 7 })
        );
    }

    /// A distinct button press code only resolves while pressed, its release
    /// code only on the release edge, mirroring `button_number`.
    #[test]
    fn control_event_resolves_distinct_button_press_and_release_codes() {
        let mapping = Mapping {
            buttons: vec![ButtonMapping {
                number: 3,
                press: 0x60,
                release: 0x61,
                screen: true,
                draw_id: 3,
            }],
            ..sample_mapping()
        };
        assert_eq!(
            mapping.control_event(0x60, true),
            Some(ControlEvent::Button { number: 3 })
        );
        assert_eq!(mapping.control_event(0x60, false), None);
        assert_eq!(
            mapping.control_event(0x61, false),
            Some(ControlEvent::Button { number: 3 })
        );
        assert_eq!(mapping.control_event(0x61, true), None);
    }

    /// Both encoder twist codes resolve to an encoder turn with the right
    /// direction, regardless of the reported state.
    #[test]
    fn control_event_resolves_encoder_turns_to_directions() {
        let mapping = sample_mapping();
        assert_eq!(
            mapping.control_event(81, false),
            Some(ControlEvent::EncoderTurn {
                number: 1,
                direction: TwistDirection::Clockwise,
            })
        );
        assert_eq!(
            mapping.control_event(81, true),
            Some(ControlEvent::EncoderTurn {
                number: 1,
                direction: TwistDirection::Clockwise,
            })
        );
        assert_eq!(
            mapping.control_event(80, false),
            Some(ControlEvent::EncoderTurn {
                number: 1,
                direction: TwistDirection::CounterClockwise,
            })
        );
    }

    /// Code 0 never means a knob push, also when some encoders have push codes and
    /// others have none (0): it used to resolve to the first encoder without codes.
    #[test]
    fn code_zero_is_never_an_encoder_push() {
        let mut mapping = sample_mapping();
        mapping.encoders[0].press = 0x31;
        mapping.encoders[0].release = 0x31;
        mapping.encoder_count = 2;
        mapping.encoders.push(EncoderMapping {
            number: 2,
            cw: 91,
            ccw: 90,
            press: 0,
            release: 0,
        });
        assert_eq!(mapping.control_event(0, true), None);
        assert_eq!(mapping.control_event(0, false), None);
        assert_eq!(
            mapping.control_event(0x31, true),
            Some(ControlEvent::EncoderPress {
                number: mapping.encoders[0].number
            })
        );
    }

    /// The knob's push code resolves to an encoder press on both edges.
    #[test]
    fn control_event_resolves_encoder_push_and_release() {
        let mapping = sample_mapping();
        assert_eq!(
            mapping.control_event(79, true),
            Some(ControlEvent::EncoderPress { number: 1 })
        );
        assert_eq!(
            mapping.control_event(79, false),
            Some(ControlEvent::EncoderPress { number: 1 })
        );
        assert_eq!(mapping.control_event(78, true), None);
    }

    /// Codes no control in the definition uses are reported as unknown.
    #[test]
    fn control_event_returns_none_for_unmapped_codes() {
        let mapping = sample_mapping();
        for code in [0, 9, 42, 200, 255] {
            assert_eq!(mapping.control_event(code, true), None);
            assert_eq!(mapping.control_event(code, false), None);
        }
    }

    /// An encoder definition without push codes (older configs) still resolves
    /// its turns but never reports a push.
    #[test]
    fn control_event_without_push_codes_ignores_pushes() {
        let mapping = Mapping {
            encoders: vec![EncoderMapping {
                number: 1,
                cw: 81,
                ccw: 80,
                press: 0,
                release: 0,
            }],
            ..sample_mapping()
        };
        assert_eq!(mapping.control_event(0, true), None);
        assert!(matches!(
            mapping.control_event(81, true),
            Some(ControlEvent::EncoderTurn { .. })
        ));
    }

    /// A reusable encoder-bearing mapping for the resolver tests.
    fn sample_mapping() -> Mapping {
        Mapping {
            device_id: "0300:3002".to_string(),
            device_name: "keypad".to_string(),
            serial: "unknown".to_string(),
            key_count: 9,
            encoder_count: 1,
            screens: 6,
            protocol_version: None,
            device_reconnect_interval: None,
            device_reconnect_max_attempts: None,
            buttons: vec![
                ButtonMapping {
                    number: 1,
                    press: 1,
                    release: 1,
                    screen: true,
                    draw_id: 1,
                },
                ButtonMapping {
                    number: 3,
                    press: 3,
                    release: 3,
                    screen: true,
                    draw_id: 3,
                },
                ButtonMapping {
                    number: 7,
                    press: 37,
                    release: 37,
                    screen: false,
                    draw_id: -1,
                },
            ],
            encoders: vec![EncoderMapping {
                number: 1,
                cw: 81,
                ccw: 80,
                press: 79,
                release: 79,
            }],
        }
    }

    // -- the console and the wizard steps, driven by scripted answers --

    /// A console reading `answers` and collecting its output and complaints.
    fn console(answers: &str) -> super::Console<&[u8], Vec<u8>, Vec<u8>> {
        super::Console::new(answers.as_bytes(), Vec::new(), Vec::new())
    }

    /// What a console has written to stdout and stderr so far.
    fn written(console: &super::Console<&[u8], Vec<u8>, Vec<u8>>) -> (String, String) {
        (
            String::from_utf8_lossy(&console.out).into_owned(),
            String::from_utf8_lossy(&console.err).into_owned(),
        )
    }

    /// Every question re-asks after an invalid answer (complaining on stderr) and
    /// returns the first valid one.
    #[test]
    fn questions_re_ask_until_valid() {
        let mut c = console("x\n9\n3\n");
        assert_eq!(c.ask_number("n", 1, 5).unwrap(), 3);
        let (out, err) = written(&c);
        assert_eq!(out.matches("n [1..5]: ").count(), 3, "{out}");
        assert_eq!(err.matches("enter a number from 1 to 5").count(), 2);

        let mut c = console("maybe\nyes\nn\n");
        assert!(c.confirm("ok").unwrap());
        assert!(!c.confirm("ok").unwrap());
        assert!(written(&c).1.contains("enter y/yes or n/no"));

        let mut c = console("7\n\n2\n");
        assert_eq!(c.ask_number_with_default("v", 0, 3, 2).unwrap(), 2);
        assert_eq!(c.ask_number_with_default("v", 0, 3, 1).unwrap(), 2);
        assert!(written(&c).1.contains("or press Enter to keep 2"));

        let mut c = console("12\nno\n3\n");
        assert_eq!(c.ask_key_number("k", 4).unwrap(), None);
        assert_eq!(c.ask_key_number("k", 4).unwrap(), Some(3));
        assert!(written(&c).1.contains("from 1 to 4 or 'no'"));

        let mut c = console("1 9\n2, 1 2\nnone\n");
        assert_eq!(c.ask_key_list("l", 4).unwrap(), vec![2, 1]);
        assert_eq!(c.ask_key_list("l", 4).unwrap(), Vec::<u8>::new());
        assert!(written(&c).1.contains("separated by spaces"));
    }

    /// End of input ends every question that has no default with `InputClosed`
    /// instead of re-asking forever (the old behaviour spun at 100% CPU with stdin
    /// closed), even after invalid answers; a question with a default keeps it.
    #[test]
    fn end_of_input_ends_the_wizard() {
        use super::MapError;
        assert!(matches!(
            console("").ask_number("n", 1, 5),
            Err(MapError::InputClosed)
        ));
        assert!(matches!(
            console("bad\n").ask_number("n", 1, 5),
            Err(MapError::InputClosed)
        ));
        assert!(matches!(
            console("").confirm("ok"),
            Err(MapError::InputClosed)
        ));
        assert!(matches!(
            console("?\n").ask_key_number("k", 3),
            Err(MapError::InputClosed)
        ));
        assert!(matches!(
            console("").ask_key_list("l", 3),
            Err(MapError::InputClosed)
        ));
        assert_eq!(
            console("").ask_number_with_default("v", 0, 3, 2).unwrap(),
            2
        );
    }

    /// A reader whose every read fails.
    struct BrokenInput;

    impl std::io::Read for BrokenInput {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("terminal gone"))
        }
    }

    /// A read error ends the wizard with that error instead of re-asking forever, for
    /// questions with and without a default.
    #[test]
    fn read_errors_end_the_wizard() {
        use super::MapError;
        let mut c =
            super::Console::new(std::io::BufReader::new(BrokenInput), Vec::new(), Vec::new());
        assert!(matches!(c.ask_number("n", 1, 2), Err(MapError::Input(_))));
        assert!(matches!(
            c.ask_number_with_default("v", 0, 3, 2),
            Err(MapError::Input(_))
        ));
        let message = c.confirm("ok").unwrap_err().to_string();
        assert_eq!(message, "failed to read input: terminal gone");
    }

    /// Every wizard error has a one-line description for the log.
    #[test]
    fn map_errors_describe_themselves() {
        use super::MapError;
        assert!(MapError::InputClosed
            .to_string()
            .contains("nothing was written"));
        assert!(MapError::Busy.to_string().contains("in use"));
        assert_eq!(
            MapError::from(mirajazz::error::MirajazzError::BadData).to_string(),
            mirajazz::error::MirajazzError::BadData.to_string()
        );
    }

    /// The device list is numbered from 1 with continuation lines indented, and the
    /// answer is returned as a 0-based index.
    #[test]
    fn choose_device_lists_and_picks() {
        let mut c = console("2\n");
        let details = [
            "first\nserial A".to_string(),
            "second\nserial B".to_string(),
        ];
        assert_eq!(super::choose_device(&mut c, &details).unwrap(), 1);
        let (out, _) = written(&c);
        assert!(
            out.contains("  1. first\n     serial A\n  2. second\n"),
            "{out}"
        );
        assert!(out.contains("device to work on [1..2]: "), "{out}");
    }

    /// The protocol question defaults to the kind's own version, and only kinds the
    /// project has not verified get the "not verified" note.
    #[test]
    fn choose_protocol_version_defaults_to_the_kind() {
        use crate::hardware::Kind;
        let mut c = console("\n");
        let version = super::choose_protocol_version(&mut c, Kind::Akp03ERev2).unwrap();
        assert_eq!(version, Kind::Akp03ERev2.protocol_version());
        assert!(!written(&c).0.contains("not been verified"));

        let mut c = console("1\n");
        assert_eq!(
            super::choose_protocol_version(&mut c, Kind::SoomfonSe).unwrap(),
            1
        );
        let (out, _) = written(&c);
        assert!(out.contains("not been verified"), "{out}");
        assert!(out.contains("  3: "), "every version is described: {out}");
    }

    /// Buttons as step 4 creates them: the first `screens` have displays in order.
    fn captured_buttons(count: u8, screens: u8) -> Vec<ButtonMapping> {
        (1..=count)
            .map(|number| ButtonMapping {
                number,
                press: number,
                release: number,
                screen: number <= screens,
                draw_id: if number <= screens { number as i8 } else { -1 },
            })
            .collect()
    }

    /// When the painted numbers match, nothing changes.
    #[test]
    fn recheck_display_keeps_a_matching_layout() {
        let mut buttons = captured_buttons(3, 2);
        let before = buttons.clone();
        super::recheck_display(&mut console("y\n"), &mut buttons, 3).unwrap();
        assert_eq!(buttons, before);
    }

    /// A mismatch is corrected from the answers: which key shows each number (a key
    /// named twice keeps its first number, a number shown nowhere is "no"), then which
    /// keys are unlit.
    #[test]
    fn recheck_display_rewrites_screens_from_the_answers() {
        let mut buttons = captured_buttons(3, 3);
        // Number 1 is on key 2, number 2 on key 2 again (ignored), number 3 nowhere;
        // key 3 is unlit.
        let mut c = console("n\n2\n2\nno\n3\n");
        super::recheck_display(&mut c, &mut buttons, 3).unwrap();
        let screens: Vec<(bool, i8)> = buttons.iter().map(|b| (b.screen, b.draw_id)).collect();
        assert_eq!(screens, vec![(false, -1), (true, 1), (false, -1)]);
        assert!(written(&c).1.contains("key 2 already shows another number"));
    }

    /// A stand-in for the connected keypad in the wizard tests: records what is drawn.
    #[derive(Default)]
    struct WizardDevice {
        /// Every call, in order (`image<key>`, `flush`).
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl crate::actions::ButtonDevice for WizardDevice {
        type Error = mirajazz::error::MirajazzError;

        async fn set_button_image(
            &self,
            key: u8,
            _image_format: mirajazz::types::ImageFormat,
            _image: image::DynamicImage,
        ) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push(format!("image{key}"));
            Ok(())
        }

        async fn clear_button_image(&self, key: u8) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push(format!("clear{key}"));
            Ok(())
        }

        async fn flush(&self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("flush".to_string());
            Ok(())
        }

        fn key_count(&self) -> u8 {
            2
        }

        async fn set_brightness(&self, _percent: u8) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn set_led_brightness(&self, _percent: u8) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// The wizard's steps 2-6 against a scripted keypad: counts confirmed, one screen,
    /// both buttons and the encoder captured from the reports (noise, already-used
    /// codes and extra notches skipped), every button painted
    /// and flushed, the layout confirmed.
    #[tokio::test]
    async fn map_connected_captures_every_control() {
        use crate::input::{encode_report, ScriptedInput, ScriptedReport};
        let device = WizardDevice::default();
        let (input, reports) = ScriptedInput::new();
        let mut noise = encode_report(9, 1);
        noise[0] = 0;
        for report in [
            noise,
            // Button 1: press and release code 1.
            encode_report(1, 1),
            encode_report(1, 0),
            // Button 2: code 1 is taken; code 2 pressed and released.
            encode_report(1, 1),
            encode_report(2, 1),
            encode_report(2, 0),
        ] {
            reports.send(ScriptedReport::Data(report)).unwrap();
        }
        // Encoder 1: clockwise 81 (several notches), counter-clockwise 80, then its
        // push 79, all without pausing - the change of code separates them.
        for (code, state) in [
            (81, 0),
            (81, 0),
            (81, 0),
            (80, 0),
            (80, 0),
            (79, 1),
            (79, 0),
        ] {
            reports
                .send(ScriptedReport::Data(encode_report(code, state)))
                .unwrap();
        }
        let format = crate::hardware::Kind::Akp03ERev2.image_format();
        let mut c = console("y\ny\n1\ny\n");
        let captured = super::map_connected(&mut c, &device, 1, &input, format, Log::default())
            .await
            .unwrap();

        assert_eq!(
            captured,
            super::Captured {
                key_count: 2,
                encoder_count: 1,
                screens: 1,
                buttons: vec![
                    ButtonMapping {
                        number: 1,
                        press: 1,
                        release: 1,
                        screen: true,
                        draw_id: 1
                    },
                    ButtonMapping {
                        number: 2,
                        press: 2,
                        release: 2,
                        screen: false,
                        draw_id: -1
                    },
                ],
                encoders: vec![EncoderMapping {
                    number: 1,
                    cw: 81,
                    ccw: 80,
                    press: 79,
                    release: 79
                }],
            }
        );
        assert_eq!(*device.calls.lock().unwrap(), ["image0", "image1", "flush"]);
        let (out, _) = written(&c);
        assert!(
            out.contains("mirajazz reports 2 keys and 1 encoders."),
            "{out}"
        );
        assert!(out.contains("  observed key  81, state 0"), "{out}");
        assert!(
            out.contains("encoder 1 -> clockwise code 81, counter-clockwise code 80"),
            "{out}"
        );
    }

    /// Corrected counts are used instead of the reported ones, and a keypad that goes
    /// away during a capture ends the wizard with the device error.
    #[tokio::test]
    async fn map_connected_uses_corrected_counts_and_stops_on_disconnect() {
        use crate::input::{ScriptedInput, ScriptedReport};
        let device = WizardDevice::default();
        let (input, reports) = ScriptedInput::new();
        reports.send(ScriptedReport::Disconnect).unwrap();
        let format = crate::hardware::Kind::Akp03ERev2.image_format();
        let mut c = console("n\n4\nn\n0\n0\n");
        let result = super::map_connected(&mut c, &device, 3, &input, format, Log::default()).await;
        assert!(
            matches!(result, Err(super::MapError::Device(_))),
            "{result:?}"
        );
        let (out, _) = written(&c);
        assert!(out.contains("number of keys [1..255]"), "{out}");
        assert!(
            out.contains("how many buttons have screens [0..4]"),
            "{out}"
        );
        assert!(device.calls.lock().unwrap().is_empty());
    }
}
