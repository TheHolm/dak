//! The input side of a keypad connection: where raw reports come from and how one is
//! decoded.
//!
//! [`InputSource`] is to reading what [`crate::actions::ButtonDevice`] is to drawing: the
//! normal run loop (`main.rs`) and the `--map` wizard's capture loops read through it,
//! so both can be driven by a scripted fake in tests instead of a physical keypad. The
//! real implementation is mirajazz's [`DeviceStateReader`].

use mirajazz::{error::MirajazzError, state::DeviceStateReader};

/// How many bytes one raw input report is read into.
pub const REPORT_LENGTH: usize = 512;

/// The prefix every input report carrying a key/encoder event starts with (`ACK`);
/// anything else is noise to be skipped.
const ACK_PREFIX: [u8; 3] = [65, 67, 75];

/// A source of raw input reports from one connected keypad.
///
/// The `Send + Sync` supertrait lets the returned futures be combined in the run loop's
/// `select!`, exactly like [`crate::actions::ButtonDevice`]'s.
#[allow(async_fn_in_trait)]
pub trait InputSource: Send + Sync {
    /// Waits for and returns the next raw report. An error means the connection is
    /// unusable (the device went away).
    async fn read_report(&self) -> Result<Vec<u8>, MirajazzError>;
}

impl InputSource for DeviceStateReader {
    async fn read_report(&self) -> Result<Vec<u8>, MirajazzError> {
        self.raw_read_data(REPORT_LENGTH).await
    }
}

impl<T: InputSource> InputSource for std::sync::Arc<T> {
    async fn read_report(&self) -> Result<Vec<u8>, MirajazzError> {
        (**self).read_report().await
    }
}

/// Decodes one raw report into its `(code, state)` pair: the raw key/encoder code and
/// the state byte (non-zero while pressed). Reports without the `ACK` prefix, or too
/// short to hold both bytes, are noise and yield `None`.
pub fn decode_report(data: &[u8]) -> Option<(u8, u8)> {
    if !data.starts_with(&ACK_PREFIX) {
        return None;
    }
    Some((*data.get(9)?, *data.get(10)?))
}

/// Builds the raw report a keypad sends for `code` in `state`, as [`decode_report`]
/// reads it back; for test fakes (and documentation of the report layout).
pub fn encode_report(code: u8, state: u8) -> Vec<u8> {
    let mut data = vec![0u8; REPORT_LENGTH];
    data[..3].copy_from_slice(&ACK_PREFIX);
    data[9] = code;
    data[10] = state;
    data
}

/// A scripted [`InputSource`] for tests: reports are fed through a channel, a fed
/// error ends the connection, and once the channel is closed (and drained) every read
/// waits forever, like a keypad nobody touches.
#[derive(Debug)]
pub struct ScriptedInput {
    /// The reports still to be read.
    reports: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ScriptedReport>>,
}

/// One scripted read result.
#[derive(Debug)]
pub enum ScriptedReport {
    /// The read returns these bytes.
    Data(Vec<u8>),
    /// The read fails as if the device had been unplugged.
    Disconnect,
}

impl ScriptedInput {
    /// A scripted input and the sender that feeds it.
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedSender<ScriptedReport>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                reports: tokio::sync::Mutex::new(rx),
            },
            tx,
        )
    }

    /// A scripted input that returns the `(code, state)` events in order, then waits
    /// forever.
    pub fn with_events(events: &[(u8, u8)]) -> Self {
        let (input, tx) = Self::new();
        for &(code, state) in events {
            let _ = tx.send(ScriptedReport::Data(encode_report(code, state)));
        }
        input
    }
}

impl InputSource for ScriptedInput {
    async fn read_report(&self) -> Result<Vec<u8>, MirajazzError> {
        let next = self.reports.lock().await.recv().await;
        match next {
            Some(ScriptedReport::Data(data)) => Ok(data),
            Some(ScriptedReport::Disconnect) => Err(MirajazzError::DeviceNotFoundError),
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report carries its code and state at bytes 9 and 10 behind the `ACK` prefix;
    /// anything else, or a truncated report, is noise.
    #[test]
    fn reports_decode_code_and_state() {
        assert_eq!(decode_report(&encode_report(7, 1)), Some((7, 1)));
        assert_eq!(decode_report(&encode_report(37, 0)), Some((37, 0)));
        let mut noise = encode_report(7, 1);
        noise[0] = 0;
        assert_eq!(decode_report(&noise), None);
        assert_eq!(decode_report(&[65, 67, 75, 0, 0]), None, "too short");
        assert_eq!(decode_report(&[]), None);
    }

    /// The scripted input returns its reports in order, reports a disconnect as an
    /// error, and waits forever once it runs dry.
    #[tokio::test]
    async fn scripted_input_plays_back_in_order() {
        let (input, tx) = ScriptedInput::new();
        tx.send(ScriptedReport::Data(encode_report(1, 1))).unwrap();
        tx.send(ScriptedReport::Disconnect).unwrap();
        drop(tx);
        let input = std::sync::Arc::new(input);
        assert_eq!(
            decode_report(&input.read_report().await.unwrap()),
            Some((1, 1))
        );
        assert!(input.read_report().await.is_err());
        let dry =
            tokio::time::timeout(std::time::Duration::from_millis(50), input.read_report()).await;
        assert!(dry.is_err(), "a drained script waits forever");

        let events = ScriptedInput::with_events(&[(3, 0), (4, 1)]);
        assert_eq!(
            decode_report(&events.read_report().await.unwrap()),
            Some((3, 0))
        );
        assert_eq!(
            decode_report(&events.read_report().await.unwrap()),
            Some((4, 1))
        );
    }
}
