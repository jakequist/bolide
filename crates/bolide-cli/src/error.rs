//! How a command stops, and with which exit code.

use std::fmt;

/// Success.
pub const EXIT_OK: u8 = 0;
/// Something failed.
pub const EXIT_FAILURE: u8 = 1;
/// The command line was wrong, or carried a password.
pub const EXIT_USAGE: u8 = 2;
/// There is no bolide server to talk to.
pub const EXIT_NOT_CONNECTED: u8 = 3;

/// Which stream a message belongs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output — `--help`, `--version`.
    Out,
    /// Standard error — everything else.
    Err,
}

/// The reason a command stopped early.
///
/// Usually a failure, but `--help` and `--version` are early exits too and they carry
/// code 0 on [`Stream::Out`]; modelling them as the same thing is what keeps `main` a
/// single `match` rather than three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    /// The process exit code.
    pub code: u8,
    /// What to print, without a trailing newline.
    pub message: String,
    /// Where to print it.
    pub stream: Stream,
}

impl CliError {
    /// A general failure — exit 1.
    pub fn failure(message: impl Into<String>) -> CliError {
        CliError {
            code: EXIT_FAILURE,
            message: message.into(),
            stream: Stream::Err,
        }
    }

    /// A usage error — exit 2.
    pub fn usage(message: impl Into<String>) -> CliError {
        CliError {
            code: EXIT_USAGE,
            message: message.into(),
            stream: Stream::Err,
        }
    }

    /// No live session — exit 3.
    pub fn not_connected(message: impl Into<String>) -> CliError {
        CliError {
            code: EXIT_NOT_CONNECTED,
            message: message.into(),
            stream: Stream::Err,
        }
    }

    /// `--help` or `--version`: print and exit 0.
    pub fn printed(message: impl Into<String>) -> CliError {
        CliError {
            code: EXIT_OK,
            message: message.into(),
            stream: Stream::Out,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
