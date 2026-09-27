//! The program's exit statuses.
//!
//! Each failure class the program can end with has its own status, so a service manager
//! (systemd's `RestartPreventExitStatus=`, a session startup script) or a caller can tell a
//! broken configuration, which restarting will not fix, apart from a keypad that is
//! merely unplugged or busy. [`EXIT_CODES`] is the list `tests/man_pages.rs` requires the
//! `EXIT STATUS` section of `dak(1)` to document.

/// Normal termination: the program was asked to stop (Ctrl-C, `SIGTERM`) and every
/// device was cleaned up, or the `--map` wizard finished.
pub const SUCCESS: u8 = 0;
/// Any failure without a more specific status below, e.g. a device that could not be
/// opened, or a second stop signal forcing an exit without cleanup.
pub const FAILURE: u8 = 1;
/// The command line could not be parsed (clap's own usage-error status).
pub const USAGE: u8 = 2;
/// The configuration could not be read or failed validation.
pub const CONFIG: u8 = 3;
/// No device defined in the configuration was found (or every one was lost and given
/// up on), and the program is not running as a service that waits for one.
pub const NO_DEVICE: u8 = 4;
/// Every device defined in the configuration that was found is held by another
/// running `dak`.
pub const DEVICE_BUSY: u8 = 5;

/// Every exit status with a one-line description, in ascending order.
pub const EXIT_CODES: &[(u8, &str)] = &[
    (SUCCESS, "normal termination"),
    (FAILURE, "unspecified failure"),
    (USAGE, "invalid command line"),
    (CONFIG, "configuration error"),
    (NO_DEVICE, "no configured device found"),
    (DEVICE_BUSY, "every found device is held by another dak"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented list is sorted, starts at success and has no duplicate status.
    #[test]
    fn exit_codes_are_sorted_and_unique() {
        assert_eq!(EXIT_CODES[0].0, SUCCESS);
        for pair in EXIT_CODES.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{pair:?} not strictly ascending");
        }
    }

    /// Clap exits with 2 on a usage error; [`USAGE`] must agree so the documented
    /// status is the one users actually see.
    #[test]
    fn usage_matches_clap() {
        use clap::Parser;
        let error = crate::cli::Cli::try_parse_from(["dak", "--no-such-flag"]).unwrap_err();
        assert_eq!(error.exit_code(), USAGE as i32);
    }
}
