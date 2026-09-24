//! Argument parsing, and where a password may come from.
//!
//! The `vnc://[USER[:PASSWORD]@]HOST[:PORT]` target is parsed by [`parse_target`], which
//! is pure: the default port 5900, an explicit port, a bare `host:port` without the
//! scheme (people will type it), an IPv6 literal, a rejected non-`vnc` scheme, and
//! percent-encoded credentials. A password may arrive in the URL, in `--password`, in
//! `--password-file` or in `BOLIDE_PASSWORD`; [`command_line_credentials`] decides
//! between the first three, and it is an error to give two of them.
//!
//! A password given on the command line is **accepted**. argv is visible to other local
//! users (`ps`) and to shell history, and the help says so, but that is the user's call
//! to make, not bolide's. What bolide does guarantee is that it goes no further: the
//! value is wrapped in [`Password`], whose `Debug` is redacted; a parse error quoting
//! argv has it cut out ([`redact_password`]); and the daemon `connect` re-execs receives
//! it through its environment, never its argv, so a long-lived process does not show it
//! in `ps` for its whole life.

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
        /// Write the PNG here. `-` is stdout, even when stdout is a terminal; without
        /// `--out` the PNG goes to stdout only when stdout is not a terminal.
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
    /// Replace this binary with the latest release.
    Update {
        /// Only report the installed and latest versions; change nothing.
        #[arg(long)]
        check: bool,
    },
}

/// `bolide connect`'s options.
#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// `vnc://[USER[:PASSWORD]@]HOST[:PORT]`, or a bare `HOST[:PORT]`. Percent-encode
    /// an `@`, `:` or `%` in the credentials.
    pub target: String,
    /// Username, for auth schemes that take one.
    #[arg(long)]
    pub username: Option<String>,
    /// The VNC password.
    ///
    /// Anything on a command line is visible to other local users through `ps` and is
    /// kept in shell history; on a shared machine, prefer --password-file or the
    /// BOLIDE_PASSWORD environment variable. Either way bolide never writes it down, and
    /// the background daemon receives it through its environment rather than its argv.
    #[arg(
        long,
        value_name = "PASSWORD",
        allow_hyphen_values = true,
        conflicts_with = "password_file"
    )]
    pub password: Option<Password>,
    /// A file holding the VNC password. Read once, never stored.
    #[arg(long, value_name = "FILE")]
    pub password_file: Option<PathBuf>,
    /// Where the computer-use server listens. Loopback by default; any address you
    /// name is used as given.
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

/// A password, held so that it cannot be printed by accident.
///
/// `Debug` is redacted, and there is no `Display`: the only way to the value is
/// [`Password::expose`], which is greppable.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// The password itself. Call it only where the value is used.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

impl std::str::FromStr for Password {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Password, Self::Err> {
        Ok(Password(s.to_string()))
    }
}

/// Parse a command line.
///
/// `argv` includes the program name, exactly as `std::env::args()` yields it. `--help`
/// and `--version` come back as an [`CliError`] with code 0 on stdout — an early exit is
/// an early exit, and modelling them the same way keeps `main` a single `match`. A
/// parse error has every password in `argv` cut out of it before it is returned.
pub fn parse_args(argv: &[String]) -> Result<Cli, CliError> {
    Cli::try_parse_from(argv).map_err(|err| from_clap(err, argv))
}

fn from_clap(err: clap::Error, argv: &[String]) -> CliError {
    use clap::error::ErrorKind;
    let message = redact_password(err.render().to_string().trim_end(), argv);
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => CliError::printed(message),
        _ => CliError::usage(message),
    }
}

/// `message` with every password `argv` carries replaced by `<redacted>`: the value
/// after `--password`, the value of `--password=…`, and the password in any
/// `scheme://user:password@host` — both as typed and percent-decoded.
pub fn redact_password(message: &str, argv: &[String]) -> String {
    let mut secrets: Vec<String> = Vec::new();
    for (i, arg) in argv.iter().enumerate() {
        if arg == "--password" {
            if let Some(value) = argv.get(i + 1) {
                secrets.push(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--password=") {
            secrets.push(value.to_string());
        } else if let Some((userinfo, _)) = arg.rsplit_once('@') {
            let userinfo = userinfo.split_once("://").map_or(userinfo, |(_, u)| u);
            if let Some((_, raw)) = userinfo.split_once(':') {
                secrets.push(raw.to_string());
                if let Some(decoded) = percent_decode(raw) {
                    secrets.push(decoded);
                }
            }
        }
    }
    // Longest first, so a secret that contains another is cut out whole.
    secrets.retain(|s| !s.is_empty());
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = message.to_string();
    for secret in &secrets {
        out = replace_whole(&out, secret);
    }
    out
}

/// Replace each occurrence of `secret` that is not part of a longer word — so a
/// one-letter password does not take the letter out of every word it appears in.
fn replace_whole(haystack: &str, secret: &str) -> String {
    let is_word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.find(secret) {
        let before = rest[..at].chars().last().or_else(|| out.chars().last());
        let after = rest[at + secret.len()..].chars().next();
        out.push_str(&rest[..at]);
        if is_word(before) || is_word(after) {
            out.push_str(secret);
        } else {
            out.push_str("<redacted>");
        }
        rest = &rest[at + secret.len()..];
    }
    out.push_str(rest);
    out
}

/// `%XX` escapes decoded, or `None` when an escape is malformed or the result is not
/// UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A parsed connection target.
///
/// Its `Display` is `host:port` and nothing else — that is what becomes `remote` in the
/// state file and in `/status` — and its `Debug` redacts the password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Hostname or IP literal.
    pub host: String,
    /// Port; 5900 when the target did not say.
    pub port: u16,
    /// The URL's username, percent-decoded. `None` when absent or empty.
    pub username: Option<String>,
    /// The URL's password, percent-decoded. `None` when absent or empty.
    pub password: Option<Password>,
}

impl Target {
    /// A target with no credentials.
    pub fn new(host: impl Into<String>, port: u16) -> Target {
        Target {
            host: host.into(),
            port,
            username: None,
            password: None,
        }
    }
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

/// Parse `vnc://[user[:password]@]host[:port]`, or a bare `host[:port]`.
///
/// The error is a sentence for a person, not a parser trace — and it never echoes a
/// password back.
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

    // The *last* `@` ends the userinfo, so an unencoded `@` in a password still parses.
    let (userinfo, rest) = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, rest),
    };
    if userinfo.is_some() && rest.is_empty() {
        return Err(
            "no host after the credentials; expected vnc://[USER[:PASSWORD]@]HOST[:PORT]"
                .to_string(),
        );
    }
    let (username, password) = match userinfo {
        None => (None, None),
        Some(userinfo) => {
            let malformed = || {
                "the credentials in the target have a malformed %-escape (write a literal \
                 % as %25)"
                    .to_string()
            };
            let (user, pass) = match userinfo.split_once(':') {
                Some((user, pass)) => (user, Some(pass)),
                None => (userinfo, None),
            };
            let user = percent_decode(user).ok_or_else(malformed)?;
            let pass = match pass {
                Some(p) => Some(percent_decode(p).ok_or_else(malformed)?),
                None => None,
            };
            (
                Some(user).filter(|u| !u.is_empty()),
                pass.filter(|p| !p.is_empty()).map(Password),
            )
        }
    };

    let (host, port) = split_host_port(rest)?;
    if host.is_empty() {
        return Err("no host in the target; expected vnc://HOST[:PORT]".to_string());
    }
    Ok(Target {
        host: host.to_string(),
        port,
        username,
        password,
    })
}

/// The username and password the command line settled on, from the URL and the flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// From the URL or `--username`.
    pub username: Option<String>,
    /// From the URL or `--password`. `--password-file` and `BOLIDE_PASSWORD` are read
    /// later, by whichever process makes the connection.
    pub password: Option<Password>,
}

/// Combine the URL's credentials with `--username`, `--password` and `--password-file`.
///
/// Two *different* usernames, or a URL password alongside either password flag, is a
/// usage error rather than a silent choice: the person said two things, and bolide will
/// not guess which one they meant. Neither error names a password.
pub fn command_line_credentials(
    target: &Target,
    args: &ConnectArgs,
) -> Result<Credentials, CliError> {
    let username = match (&target.username, &args.username) {
        (Some(url), Some(flag)) if url != flag => {
            return Err(CliError::usage(format!(
                "the target says user {url:?} and --username says {flag:?}; give one"
            )))
        }
        (url, flag) => url.clone().or_else(|| flag.clone()),
    };
    if target.password.is_some() && (args.password.is_some() || args.password_file.is_some()) {
        return Err(CliError::usage(
            "the target URL carries a password and so does --password or --password-file; \
             give one",
        ));
    }
    Ok(Credentials {
        username,
        password: target.password.clone().or_else(|| args.password.clone()),
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
