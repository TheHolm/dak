//! Interactive device-mapping wizard, started with `dak --map`.
//!
//! Unlike the normal mode this wizard never reads `config.json` and never
//! executes actions. It walks the user through picking a device, confirming
//! (or entering) the key/encoder counts, reporting how many buttons have
//! screens, capturing the raw code each physical button and encoder produces,
//! and a manual display sanity check. Counts and codes are typed as numbers,
//! confirmations as `y`/`yes`/`n`/`no`; the collected mapping is printed as
//! JSON to stdout, then the device screens are cleared and the device is shut
//! down.

use std::collections::HashSet;
use std::fmt::Debug;
use std::io::{self, Write};

use async_hid::DeviceId as HidDeviceId;
use mirajazz::{
    device::{list_devices, Device, DeviceQuery},
    error::MirajazzError,
    state::DeviceStateReader,
    types::{DeviceInput, HidDevice, ImageFormat, ImageMirroring, ImageMode, ImageRotation},
};
use serde::Serialize;

use crate::log::{Log, Subsystem};

/// True everywhere because a mapping wizard makes no sense without a device.
const QUERY: DeviceQuery = DeviceQuery::new(65440, 1, 0x0300, 0x3002);

/// Protocol version used to connect, identical to the normal mode.
const PROTOCOL_VERSION: usize = 2;

/// Default key count used to connect, identical to the normal mode. The wizard
/// then asks the user to confirm (or manually correct) the real counts for the
/// mapping itself.
const DEFAULT_KEY_COUNT: usize = 9;

/// Default encoder count used to connect, identical to the normal mode.
const DEFAULT_ENCODER_COUNT: usize = 3;

/// The image format used for painting the button-number display test,
/// identical to the one the normal mode uses for this device family.
const IMAGE_FORMAT: ImageFormat = ImageFormat {
    mode: ImageMode::JPEG,
    size: (60, 60),
    // The device's LCDs display images rotated 90 degrees clockwise.
    rotation: ImageRotation::Rot90,
    mirror: ImageMirroring::None,
};

/// Everything the wizard learned about one device.
#[derive(Debug, PartialEq, Serialize)]
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
    /// Raw codes per physical button, in the order the user pressed them.
    pub buttons: Vec<ButtonMapping>,
    /// Raw twist codes per encoder, in the order the user turned them.
    pub encoders: Vec<EncoderMapping>,
}

/// Mapping of one physical button.
#[derive(Debug, PartialEq, Serialize)]
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

/// Mapping of one encoder's twist codes.
#[derive(Debug, PartialEq, Serialize)]
pub struct EncoderMapping {
    /// Logical encoder number (1-based), the order the user turned them in.
    pub number: u8,
    /// Raw code observed for the first turn of the knob.
    pub cw: u8,
    /// Raw code observed for the second turn (the other direction).
    pub ccw: u8,
}

/// Runs the whole `dak --map` wizard: device selection, count confirmation,
/// screen count, button and encoder capture, display sanity check, JSON output,
/// and screen clearing/shutdown.
pub async fn run_map_wizard(log: Log) -> Result<(), MirajazzError> {
    log.info("DAK device-mapping wizard (config is not read, no actions run)");

    // Step 1: numbered list of detected devices, user picks one by number.
    let devices: Vec<HidDevice> = list_devices(&[QUERY]).await?.into_iter().collect();
    if devices.is_empty() {
        log.error("no compatible devices found");
        return Err(MirajazzError::DeviceNotFoundError);
    }
    println!("Detected devices:");
    for (index, dev) in devices.iter().enumerate() {
        let details = device_details(
            &dev.id,
            dev.vendor_id,
            dev.product_id,
            &dev.serial_number,
            &dev.name,
        );
        let mut lines = details.lines();
        if let Some(first) = lines.next() {
            println!("  {}. {first}", index + 1);
        }
        for line in lines {
            println!("     {line}");
        }
    }
    let pick = ask_number("device to work on", 1, devices.len() as u64) as usize - 1;
    let dev = &devices[pick];
    println!();

    // Step 2: connect and confirm (or manually enter) the counts.
    println!(
        "Connecting to {}...",
        device_summary(
            &dev.id,
            dev.vendor_id,
            dev.product_id,
            &dev.serial_number,
            &dev.name
        )
    );
    let device = Device::connect(
        dev,
        PROTOCOL_VERSION,
        DEFAULT_KEY_COUNT,
        DEFAULT_ENCODER_COUNT,
    )
    .await?;
    let device = device.with_supports_both_keypress_states(true);
    let device = device.with_supports_both_encoder_states(true);

    // Mirror the normal mode: these calls double as the device-initialization
    // handshake. Without it the keypad stays silent and reports nothing, which
    // would make the capture steps hang forever.
    device.set_brightness(50).await?;
    device.clear_all_button_images().await?;

    println!(
        "mirajazz reports {} keys and {} encoders.",
        device.key_count(),
        device.encoder_count()
    );
    let key_count = if confirm("is the key count correct") {
        device.key_count() as u8
    } else {
        ask_number("number of keys", 1, u8::MAX as u64) as u8
    };
    let encoder_count = if confirm("is the encoder count correct") {
        device.encoder_count() as u8
    } else {
        ask_number("number of encoders", 0, u8::MAX as u64) as u8
    };
    println!();

    // Step 3: how many buttons have screens.
    let screens = ask_number("how many buttons have screens", 0, key_count as u64) as u8;
    println!();

    let reader = device.get_reader(|_, _| Ok(DeviceInput::NoData));

    // Step 4: capture one physical button at a time, in the order the user
    // presses them. Codes already assigned to an earlier button are skipped.
    let mut buttons = Vec::new();
    let mut used: HashSet<u8> = HashSet::new();
    for number in 1..=key_count {
        println!("press button {number}: press it, hold it, then release it");
        let (press, release) = capture_press_release(&reader, &used).await?;
        println!("button {number} -> press code {press}, release code {release}");
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
    println!();

    // Step 5: capture one encoder at a time, one notch per direction.
    let mut encoders = Vec::new();
    for number in 1..=encoder_count {
        println!("turn encoder {number}: one notch clockwise, then one notch counter-clockwise");
        let (cw, ccw) = capture_encoder_twists(&reader, &used).await?;
        println!("encoder {number} -> first turn code {cw}, second turn code {ccw}");
        used.insert(cw);
        used.insert(ccw);
        encoders.push(EncoderMapping { number, cw, ccw });
    }
    println!();

    // Step 6: paint the logical button number on every button (including the
    // ones without a display, which stay dark) and ask the user to verify.
    println!("painting button numbers on every button...");
    for (index, number) in (1..=key_count).enumerate() {
        let image = match crate::text::render_text(&[number.to_string()], IMAGE_FORMAT) {
            Ok(image) => image,
            Err(error) => {
                log.error(format!("failed to render test label: {error}"));
                return Err(MirajazzError::BadData);
            }
        };
        device
            .set_button_image(index as u8, IMAGE_FORMAT, image)
            .await?;
    }
    device.flush().await?;
    println!();

    if !confirm("do the painted button numbers match your physical layout (1..N in order)") {
        // Rework the display check: for every painted number find out which
        // key shows it, then which keys are completely unlit.
        println!(
            "display recheck: for every number painted on a screen, name the key that shows it"
        );
        // shown[i] = number displayed on key `i` (0-based), None if unlit.
        let mut shown: Vec<Option<i8>> = vec![None; buttons.len()];
        for number in 1..=key_count {
            if let Some(key) = ask_key_number(
                &format!("which key number has number {number} displayed"),
                key_count,
            ) {
                let slot = &mut shown[(key - 1) as usize];
                if slot.is_none() {
                    *slot = Some(number as i8);
                } else {
                    eprintln!("key {key} already shows another number; keeping the first answer");
                }
            }
        }
        let unlit = ask_key_list(
            "enter the key numbers without any number displayed (or 'none')",
            key_count,
        );
        for key in unlit {
            shown[(key - 1) as usize] = None;
        }
        for (index, button) in buttons.iter_mut().enumerate() {
            match shown[index] {
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
    }
    println!();

    // Finished: emit the mapping as JSON, then clear the screens and end.
    let mapping = Mapping {
        device_id: format!("{:04X}:{:04X}", dev.vendor_id, dev.product_id),
        device_name: dev.name.clone(),
        serial: dev
            .serial_number
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        key_count,
        encoder_count,
        screens,
        buttons,
        encoders,
    };
    match serde_json::to_string_pretty(&mapping) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            log.error(format!("failed to serialize mapping: {error}"));
            return Err(MirajazzError::BadData);
        }
    }

    device.clear_all_button_images().await?;
    device.flush().await?;
    device.shutdown().await?;
    log.debug(Subsystem::Device, "screens cleared, device shut down");
    Ok(())
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
    let serial = serial.as_deref().unwrap_or("unknown");
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
    let serial = serial.as_deref().unwrap_or("unknown");
    let path = device_path(id);
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

    /// Pure state machine capturing the two twist codes of one encoder.
    ///
    /// Twist events arrive as `(code, 0)` reports (nothing pressed). The first
    /// fresh code in `exclude`-complement is taken as the clockwise turn, the
    /// next distinct fresh code as the counter-clockwise turn. Repeated codes
    /// (extra notches of the same direction) are ignored.
    pub struct TwistCapture {
        exclude: HashSet<u8>,
        first: Option<u8>,
    }

    impl TwistCapture {
        /// A capture that refuses to match any code in `exclude`.
        pub fn new(exclude: HashSet<u8>) -> Self {
            Self {
                exclude,
                first: None,
            }
        }

        /// Feeds one observed `(code, state)` event and returns `Some((cw, ccw))`
        /// once two distinct fresh twist codes have been seen.
        pub fn feed(&mut self, code: u8, state: u8) -> Option<(u8, u8)> {
            if state != 0 || self.exclude.contains(&code) {
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

/// Reads raw device reports until the `PressCapture` records a full
/// press-and-release for a fresh button.
async fn capture_press_release(
    reader: &DeviceStateReader,
    exclude: &HashSet<u8>,
) -> Result<(u8, u8), MirajazzError> {
    let mut capture = capture::PressCapture::new(exclude.clone());
    loop {
        let (code, state) = next_event(reader).await?;
        if let Some(done) = capture.feed(code, state) {
            return Ok(done);
        }
    }
}

/// Reads raw device reports until the `TwistCapture` records two distinct
/// twist codes for one encoder.
async fn capture_encoder_twists(
    reader: &DeviceStateReader,
    exclude: &HashSet<u8>,
) -> Result<(u8, u8), MirajazzError> {
    let mut capture = capture::TwistCapture::new(exclude.clone());
    loop {
        let (code, state) = next_event(reader).await?;
        if let Some(done) = capture.feed(code, state) {
            return Ok(done);
        }
    }
}

/// Turns a raw input report into a `(code, state)` pair; reports that do not
/// carry the expected ACK prefix are noise and yield `None`.
fn raw_event(data: &[u8]) -> Option<(u8, u8)> {
    data.starts_with(&[65, 67, 75]).then(|| (data[9], data[10]))
}

/// Reads one report from the device and extracts its `(code, state)` pair,
/// skipping non-ACK noise reports. Every observed event is echoed to stdout so
/// the user sees that the device is responding during a capture.
async fn next_event(reader: &DeviceStateReader) -> Result<(u8, u8), MirajazzError> {
    loop {
        let data = reader.raw_read_data(512).await?;
        if let Some(event) = raw_event(&data) {
            println!("  observed key {:>3}, state {}", event.0, event.1);
            return Ok(event);
        }
    }
}

/// Asks the user to type a number in `min..=max` and keeps asking until a
/// valid one is entered.
fn ask_number(prompt: &str, min: u64, max: u64) -> u64 {
    loop {
        print!("{prompt} [{min}..{max}]: ");
        if let Err(error) = io::stdout().flush() {
            eprintln!("failed to flush stdout: {error}");
        }
        let mut line = String::new();
        let input = match io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => parse_number(&line),
            Err(error) => {
                eprintln!("failed to read input: {error}");
                None
            }
        };
        match input {
            Some(number) if (min..=max).contains(&number) => return number,
            _ => eprintln!("enter a number from {min} to {max}"),
        }
    }
}

/// Asks a yes/no question. The user answers with `y`/`yes` or `n`/`no`;
/// the numeric `1`/`0` forms are accepted as well and the prompt repeats
/// until a valid answer is typed.
fn confirm(prompt: &str) -> bool {
    loop {
        print!("{prompt}: enter y/yes or n/no: ");
        if let Err(error) = io::stdout().flush() {
            eprintln!("failed to flush stdout: {error}");
        }
        let mut line = String::new();
        let valid = match io::stdin().read_line(&mut line) {
            Ok(_) => parse_yes_no(&line),
            Err(error) => {
                eprintln!("failed to read input: {error}");
                None
            }
        };
        match valid {
            Some(answer) => return answer,
            None => eprintln!("enter y/yes or n/no"),
        }
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

/// Asks for a single key number in `1..=max`; answering "no" (or `n`, `none`,
/// `0`) means the number in question is not displayed on any screen.
fn ask_key_number(prompt: &str, max: u8) -> Option<u8> {
    loop {
        print!("{prompt} [key number, or 'no' if not on any button]: ");
        if let Err(error) = io::stdout().flush() {
            eprintln!("failed to flush stdout: {error}");
        }
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(_) => {
                if let Some(key) = parse_key_number(&line, max) {
                    return Some(key);
                }
                if is_no(&line) {
                    return None;
                }
                eprintln!("enter a key number from 1 to {max} or 'no'");
            }
            Err(error) => {
                eprintln!("failed to read input: {error}");
            }
        }
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

/// Asks for a comma/space-separated list of key numbers in `1..=max`;
/// answering "none" yields an empty list.
fn ask_key_list(prompt: &str, max: u8) -> Vec<u8> {
    loop {
        print!("{prompt}: ");
        if let Err(error) = io::stdout().flush() {
            eprintln!("failed to flush stdout: {error}");
        }
        let mut line = String::new();
        let keys = match io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => parse_key_list(&line, max),
            Err(error) => {
                eprintln!("failed to read input: {error}");
                None
            }
        };
        match keys {
            Some(keys) => return keys,
            None => eprintln!("enter key numbers from 1 to {max}, separated by spaces (or 'none')"),
        }
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
    use super::HidDeviceId;
    use super::{
        device_details, device_summary, is_no, parse_key_list, parse_key_number, parse_number,
        parse_yes_no, raw_event,
    };
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

    /// `raw_event` extracts `(data[9], data[10])` from an ACK-prefixed report
    /// and rejects reports without the ACK prefix.
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

    /// Twist capture takes the first two distinct fresh state-0 codes as the
    /// two directions and ignores repeats and pressed-state events.
    #[test]
    fn twist_capture_records_two_directions() {
        let mut capture = TwistCapture::new(HashSet::new());
        assert_eq!(capture.feed(0x90, 1), None); // a pressed-state event
        assert_eq!(capture.feed(0x90, 0), None); // first notch
        assert_eq!(capture.feed(0x90, 0), None); // second notch, same direction
        assert_eq!(capture.feed(0x91, 0), Some((0x90, 0x91)));
    }

    /// Twist capture never accepts codes that belong to buttons already mapped.
    #[test]
    fn twist_capture_skips_button_codes() {
        let mut capture = TwistCapture::new(HashSet::from([0x61]));
        assert_eq!(capture.feed(0x61, 0), None);
        assert_eq!(capture.feed(0x60, 0), None);
        assert_eq!(capture.feed(0x33, 0), Some((0x60, 0x33)));
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
            }],
        };
        let value = serde_json::to_value(&mapping).unwrap();
        assert_eq!(value["device_id"], "0300:3002");
        assert_eq!(value["key_count"], 9);
        assert_eq!(value["screens"], 6);
        assert_eq!(value["buttons"][0]["press"], 1);
        assert_eq!(value["buttons"][0]["screen"], true);
        assert_eq!(value["buttons"][0]["draw_id"], 1);
        assert_eq!(value["buttons"][1]["screen"], false);
        assert_eq!(value["buttons"][1]["draw_id"], -1);
        assert_eq!(value["encoders"][0]["cw"], 0x90);
        assert_eq!(value["encoders"][0]["ccw"], 0x91);
    }
}
