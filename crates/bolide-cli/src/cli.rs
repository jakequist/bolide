//! Argument parsing, and the one argument bolide refuses to accept.
//!
//! The `vnc://host[:port]` target is parsed by [`parse_target`], which is pure: the
//! default port 5900, an explicit port, a bare `host:port` without the scheme (people
//! will type it), an IPv6 literal, a rejected non-`vnc` scheme, and — the one that
//! matters — **a URL with a password in it (`vnc://user:pw@host`) is refused**, for
//! exactly the argv reason `--password` is.

use std::net::SocketAddr;
use std::path::PathBuf;

use bolide_server::wire::{ComputerAction, ScrollDirection};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::error::CliError;

/// The whole command line.
#[derive(Debug, Parser)]
#[command(
    name = "bolide",
    version,
    about = "Point an agent at any VNC desktop.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Refused: argv is not private. Use --password-file or BOLIDE_PASSWORD.
    ///
    /// `ps` shows a command line, the shell records it in history, and CI logs print
    /// it. bolide will not take a password this way.
    //
    // The flag is in the parser only so the refusal can be specific: its value is never
    // read — `parse_args` fails the moment the flag appears.
    #[arg(long, global = true, num_args = 0..=1, value_name = "REFUSED", hide_short_help = true)]
    pub password: Option<Option<String>>,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// One subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Connect to a desktop and start the computer-use server.
    Connect(ConnectArgs),
    /// What is connected, and where the server is listening.
    Status,
    /// Stop the server and close the RFB session.
    Disconnect,
    /// The current screen as PNG.
    Screenshot {
        /// Write the PNG here instead of to stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Click at a point.
    Click {
        /// X, in framebuffer pixels.
        x: i32,
        /// Y, in framebuffer pixels.
        y: i32,
        /// Which button.
        #[arg(long, value_enum, default_value_t = Button::Left)]
        button: Button,
    },
    /// Move the pointer.
    Move {
        /// X, in framebuffer pixels.
        x: i32,
        /// Y, in framebuffer pixels.
        y: i32,
    },
    /// Type literal text.
    Type {
        /// The text.
        text: String,
    },
    /// Press a key or chord — `Return`, `ctrl+c`, `cmd+space`, `Page_Down`.
    Key {
        /// The chord, in `xdotool` spelling.
        chord: String,
    },
    /// Wheel notches under the pointer.
    Scroll {
        /// X, in framebuffer pixels.
        x: i32,
        /// Y, in framebuffer pixels.
        y: i32,
        /// Which way. RFB has no horizontal wheel.
        #[arg(long, value_enum)]
        dir: Dir,
        /// How many notches.
        #[arg(long, default_value_t = 1)]
        amount: u32,
    },
    /// The remote clipboard.
    Clipboard {
        /// Push or pull.
        #[command(subcommand)]
        what: ClipboardCommand,
    },
}

/// `bolide connect`'s options.
#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// `vnc://HOST[:PORT]`, or a bare `HOST[:PORT]`.
    pub target: String,
    /// Username, for auth schemes that take one.
    #[arg(long)]
    pub username: Option<String>,
    /// A file holding the VNC password. Read once, never stored.
    #[arg(long, value_name = "FILE")]
    pub password_file: Option<PathBuf>,
    /// Where the computer-use server listens.
    #[arg(long, default_value = "127.0.0.1:0", value_name = "ADDR")]
    pub listen: SocketAddr,
    /// Require `Authorization: Bearer T` on every route but `/healthz`.
    #[arg(long, value_name = "T")]
    pub token: Option<String>,
    /// Stay attached instead of daemonising.
    #[arg(long)]
    pub foreground: bool,
}

/// `bolide clipboard`'s two halves.
#[derive(Debug, Subcommand)]
pub enum ClipboardCommand {
    /// Set the remote clipboard.
    Push {
        /// The text. Omit it and pass `--stdin` instead.
        text: Option<String>,
        /// Read the text from stdin.
        #[arg(long)]
        stdin: bool,
    },
    /// The last clipboard the desktop sent.
    Pull,
}

/// Which mouse button.
///
/// This enum is the *single* statement of the button vocabulary: clap validates
/// `--button` against it, and [`crate::api::action_for_click`] resolves through it, so a
/// name that parses cannot fall through to a different click than the one asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Button {
    /// Button 1.
    Left,
    /// Button 3.
    Right,
    /// Button 2.
    Middle,
}

impl Button {
    /// The spelling `--button` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }

    /// The button of that name, if there is one.
    pub fn from_name(name: &str) -> Option<Button> {
        [Button::Left, Button::Right, Button::Middle]
            .into_iter()
            .find(|b| b.as_str() == name)
    }

    /// The action a click with this button becomes.
    pub fn click_at(self, x: i32, y: i32) -> ComputerAction {
        let coordinate = [x, y];
        match self {
            Button::Left => ComputerAction::LeftClick { coordinate },
            Button::Right => ComputerAction::RightClick { coordinate },
            Button::Middle => ComputerAction::MiddleClick { coordinate },
        }
    }
}

/// Which way the wheel turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Dir {
    /// Wheel up.
    Up,
    /// Wheel down.
    Down,
}

impl From<Dir> for ScrollDirection {
    fn from(dir: Dir) -> ScrollDirection {
        match dir {
            Dir::Up => ScrollDirection::Up,
            Dir::Down => ScrollDirection::Down,
        }
    }
}

/// The message the `--password` refusal prints. Also used for a password in the URL, so
/// the two refusals say the same thing.
pub const PASSWORD_REFUSAL: &str = "bolide refuses a password on the command line: argv is \
not private — `ps` shows it, the shell records it in history, and CI logs print it. Use \
`--password-file FILE` (read once, never stored) or the BOLIDE_PASSWORD environment \
variable.";

/// Parse a command line, refusing a password before anything else can happen to it.
///
/// `argv` includes the program name, exactly as `std::env::args()` yields it. `--help`
/// and `--version` come back as an [`CliError`] with code 0 on stdout — an early exit is
/// an early exit, and modelling them the same way keeps `main` a single `match`.
pub fn parse_args(argv: &[String]) -> Result<Cli, CliError> {
    match Cli::try_parse_from(argv) {
        Ok(cli) => {
            if cli.password.is_some() {
                return Err(CliError::usage(PASSWORD_REFUSAL));
            }
            Ok(cli)
        }
        Err(err) => Err(from_clap(err)),
    }
}

fn from_clap(err: clap::Error) -> CliError {
    use clap::error::ErrorKind;
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            CliError::printed(err.render().to_string().trim_end().to_string())
        }
        _ => CliError::usage(err.render().to_string().trim_end().to_string()),
    }
}

/// A parsed connection target. Never carries a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Hostname or IP literal.
    pub host: String,
    /// Port; 5900 when the target did not say.
    pub port: u16,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// The RFB port everything defaults to.
pub const DEFAULT_PORT: u16 = 5900;

/// Parse `vnc://host[:port]`, or a bare `host[:port]`.
///
/// The error is a sentence for a person, not a parser trace — and it never echoes a
/// password back, because the one thing that could be in there is the thing we are
/// refusing to handle.
pub fn parse_target(s: &str) -> std::result::Result<Target, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("no desktop given; expected vnc://HOST[:PORT]".to_string());
    }

    let rest = match s.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("vnc") {
                return Err(format!(
                    "bolide speaks RFB, so the target must be vnc://HOST[:PORT]; \
                     {scheme}:// is something else"
                ));
            }
            rest
        }
        None => s,
    };
    let rest = rest.trim_end_matches('/');

    let rest = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => {
            if userinfo.contains(':') {
                return Err(format!(
                    "{PASSWORD_REFUSAL} (a password in the URL is argv too)"
                ));
            }
            if hostport.is_empty() {
                return Err("no desktop given; expected vnc://HOST[:PORT]".to_string());
            }
            return Err(format!(
                "put the username in --username, not in the URL: \
                 bolide connect vnc://{hostport} --username {userinfo}"
            ));
        }
        None => rest,
    };

    let (host, port) = split_host_port(rest)?;
    if host.is_empty() {
        return Err("no host in the target; expected vnc://HOST[:PORT]".to_string());
    }
    Ok(Target {
        host: host.to_string(),
        port,
    })
}

fn split_host_port(rest: &str) -> std::result::Result<(&str, u16), String> {
    if let Some(after_bracket) = rest.strip_prefix('[') {
        let end = after_bracket
            .find(']')
            .ok_or_else(|| format!("unterminated IPv6 literal in {rest:?}"))?;
        let host = &after_bracket[..end];
        return match &after_bracket[end + 1..] {
            "" => Ok((host, DEFAULT_PORT)),
            tail => match tail.strip_prefix(':') {
                Some(port) => Ok((host, parse_port(port)?)),
                None => Err(format!(
                    "{tail:?} is not a port; expected vnc://[HOST]:PORT"
                )),
            },
        };
    }
    match rest.rsplit_once(':') {
        // An unbracketed IPv6 literal: the colons are the address, not a port.
        Some((head, _)) if head.contains(':') => Ok((rest, DEFAULT_PORT)),
        Some((host, port)) => Ok((host, parse_port(port)?)),
        None => Ok((rest, DEFAULT_PORT)),
    }
}

fn parse_port(s: &str) -> std::result::Result<u16, String> {
    s.parse::<u16>()
        .map_err(|_| format!("{s:?} is not a TCP port (1–65535)"))
        .and_then(|p| {
            if p == 0 {
                Err("port 0 is not a desktop".to_string())
            } else {
                Ok(p)
            }
        })
}
