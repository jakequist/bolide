//! What each command does — and, separately, the decisions it makes on the way.
//!
//! Everything in this module that could be wrong is a pure function over its inputs
//! ([`require_session`], [`screenshot_sink`], [`resolve_password`], [`clipboard_text`],
//! [`status_lines`]); what is left is `await`s and `println!`s. The seam that makes the
//! interesting half testable is that "is stdout a terminal" is a **parameter**, not a
//! call to `IsTerminal` buried three frames down.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bolide_rfb::Session;
use bolide_server::wire::{ComputerAction, StatusResponse};

use crate::api::{self, Client};
use crate::cli::{Cli, ClipboardCommand, Command, ConnectArgs};
use crate::daemon;
use crate::error::{CliError, Stream, EXIT_OK};
use crate::state::{self, SessionState};

/// The environment variable a password may arrive in.
pub const PASSWORD_ENV: &str = "BOLIDE_PASSWORD";

/// Parse, run, print, and return the process exit code.
///
/// `argv` is `std::env::args()` in full, program name included.
pub fn main(argv: &[String]) -> u8 {
    let cli = match crate::cli::parse_args(argv) {
        Ok(cli) => cli,
        Err(stop) => return report(stop),
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return report(CliError::failure(format!("could not start a runtime: {e}"))),
    };
    match runtime.block_on(run(cli, argv)) {
        Ok(()) => EXIT_OK,
        Err(stop) => report(stop),
    }
}

fn report(stop: CliError) -> u8 {
    if !stop.message.is_empty() {
        match stop.stream {
            Stream::Out => println!("{}", stop.message),
            Stream::Err => eprintln!("{}", stop.message),
        }
    }
    stop.code
}

/// Run one command.
pub async fn run(cli: Cli, argv: &[String]) -> Result<(), CliError> {
    match cli.command {
        Command::Connect(args) => connect(args, argv).await,
        Command::Status => status().await,
        Command::Disconnect => disconnect(),
        Command::Screenshot { out } => screenshot(out.as_deref()).await,
        Command::Click { x, y, button } => act(api::action_for_click(x, y, button.as_str())).await,
        Command::Move { x, y } => act(api::action_for_move(x, y)).await,
        Command::Type { text } => act(api::action_for_type(&text)).await,
        Command::Key { chord } => act(api::action_for_key(&chord)).await,
        Command::Scroll { x, y, dir, amount } => {
            act(api::action_for_scroll(x, y, dir, amount)).await
        }
        Command::Clipboard { what } => clipboard(what).await,
    }
}

// ---------------------------------------------------------------- pure decisions

/// The live session, or the exit-3 error that says how to get one.
pub fn require_session(found: Option<SessionState>) -> Result<SessionState, CliError> {
    found.ok_or_else(|| {
        CliError::not_connected(
            "not connected: run `bolide connect vnc://HOST` first (`bolide status` once it is up)",
        )
    })
}

/// Where a screenshot's bytes go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    /// A file named with `--out`.
    File(PathBuf),
    /// Standard output, which is not a terminal.
    Stdout,
}

/// Decide where a screenshot goes, refusing to spray PNG across a terminal.
pub fn screenshot_sink(out: Option<&Path>, stdout_is_terminal: bool) -> Result<Sink, CliError> {
    match out {
        Some(path) => Ok(Sink::File(path.to_path_buf())),
        None if stdout_is_terminal => Err(CliError::usage(
            "refusing to write PNG bytes to a terminal: pass `--out FILE.png`, or pipe \
             this command somewhere",
        )),
        None => Ok(Sink::Stdout),
    }
}

/// Where the password comes from: `--password-file` first, then `BOLIDE_PASSWORD`.
///
/// A trailing newline is the editor's, not the password's, so it is trimmed — but
/// nothing else is, because leading whitespace could be real.
pub fn resolve_password(
    file: Option<&Path>,
    env: Option<String>,
) -> Result<Option<String>, CliError> {
    if let Some(path) = file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            CliError::failure(format!(
                "cannot read the password file {}: {e}",
                path.display()
            ))
        })?;
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        return Ok(Some(trimmed.to_string()));
    }
    Ok(env.filter(|v| !v.is_empty()))
}

/// The text `clipboard push` will send: the argument, or stdin, and exactly one of them.
pub fn clipboard_text(
    text: Option<String>,
    stdin: bool,
    read_stdin: &dyn Fn() -> std::io::Result<String>,
) -> Result<String, CliError> {
    match (text, stdin) {
        (Some(_), true) => Err(CliError::usage(
            "give `clipboard push` either TEXT or --stdin, not both",
        )),
        (None, false) => Err(CliError::usage(
            "`clipboard push` needs TEXT, or --stdin to read it from a pipe",
        )),
        (Some(text), false) => Ok(text),
        (None, true) => {
            read_stdin().map_err(|e| CliError::failure(format!("could not read stdin: {e}")))
        }
    }
}

/// `bolide status`, as lines. One fact per line.
pub fn status_lines(status: &StatusResponse, state: &SessionState) -> Vec<String> {
    let mut lines = Vec::new();
    if status.connected {
        lines.push(format!(
            "connected to {} ({}x{}, {:?})",
            status.remote, status.width, status.height, status.desktop_name
        ));
        if !status.painted {
            lines.push(
                "screen not painted yet (the framebuffer is black because nothing has \
                 arrived, not because the desktop is)"
                    .to_string(),
            );
        }
    } else {
        lines.push(format!(
            "not connected to {} (the RFB session is gone)",
            status.remote
        ));
    }
    lines.push(format!("computer-use on {}", state.endpoint));
    lines.push(format!("daemon pid {}", state.pid));
    if let Some(username) = &state.username {
        lines.push(format!("username {username}"));
    }
    lines.push(format!(
        "token {}",
        if state.token.is_some() {
            "required"
        } else {
            "none"
        }
    ));
    lines.push(format!("since {}", state.started_at));
    lines.push(format!("bolide {}", status.version));
    lines
}

// ---------------------------------------------------------------- the commands

async fn client() -> Result<(Client, SessionState), CliError> {
    let state = require_session(state::load())?;
    let client = Client::for_session(&state)?;
    Ok((client, state))
}

async fn act(action: ComputerAction) -> Result<(), CliError> {
    let (client, _) = client().await?;
    let response = client.computer(&action).await?;
    if let Some(coordinate) = response.coordinate {
        println!("{} {}", coordinate[0], coordinate[1]);
    }
    Ok(())
}

async fn status() -> Result<(), CliError> {
    let (client, state) = client().await?;
    let status = client.status().await?;
    for line in status_lines(&status, &state) {
        println!("{line}");
    }
    Ok(())
}

async fn screenshot(out: Option<&Path>) -> Result<(), CliError> {
    let sink = screenshot_sink(out, std::io::stdout().is_terminal())?;
    let (client, _) = client().await?;
    let png = client.screenshot().await?;
    match sink {
        Sink::File(path) => {
            std::fs::write(&path, &png)
                .map_err(|e| CliError::failure(format!("cannot write {}: {e}", path.display())))?;
            let (w, h) = png_size(&png);
            println!("wrote {} ({w}x{h}, {} bytes)", path.display(), png.len());
        }
        Sink::Stdout => {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&png)
                .and_then(|()| stdout.flush())
                .map_err(|e| CliError::failure(format!("cannot write the PNG: {e}")))?;
        }
    }
    Ok(())
}

/// Width and height out of a PNG's IHDR, so the CLI can say what it wrote without
/// decoding the image. `(0, 0)` when the bytes are not a PNG.
fn png_size(png: &[u8]) -> (u32, u32) {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return (0, 0);
    }
    let be = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    (be(16), be(20))
}

async fn clipboard(what: ClipboardCommand) -> Result<(), CliError> {
    let (client, _) = client().await?;
    match what {
        ClipboardCommand::Pull => match client.clipboard().await?.text {
            Some(text) => println!("{text}"),
            None => {
                return Err(CliError::failure(
                    "the desktop has not sent a clipboard: RFB has no \"what is on your \
                     clipboard\" message, so something on the far side has to copy first",
                ))
            }
        },
        ClipboardCommand::Push { text, stdin } => {
            let text = clipboard_text(text, stdin, &|| {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                Ok(buf)
            })?;
            client.set_clipboard(&text).await?;
        }
    }
    Ok(())
}

fn disconnect() -> Result<(), CliError> {
    let state = require_session(state::load())?;
    let dir = state::require_state_dir().map_err(|e| CliError::failure(e.to_string()))?;

    // SAFETY: SIGTERM to a pid this process wrote down itself.
    let rc = unsafe { libc::kill(state.pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(CliError::failure(format!(
                "could not stop the bolide daemon (pid {}): {err}",
                state.pid
            )));
        }
    }
    for _ in 0..200 {
        if !state::pid_is_alive(state.pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    state::clear_in(&dir)
        .map_err(|e| CliError::failure(format!("could not clear the state file: {e}")))?;
    println!("disconnected from {}", state.remote);
    Ok(())
}

async fn connect(args: ConnectArgs, argv: &[String]) -> Result<(), CliError> {
    let target = crate::cli::parse_target(&args.target).map_err(CliError::usage)?;
    let dir = state::require_state_dir().map_err(|e| CliError::failure(e.to_string()))?;

    if let Some(existing) = state::load_in(&dir) {
        return Err(CliError::failure(format!(
            "already connected to {} (pid {}); run `bolide disconnect` first",
            existing.remote, existing.pid
        )));
    }

    if args.foreground {
        serve_foreground(target, args, &dir).await
    } else {
        spawn_and_wait(argv, &dir)
    }
}

/// The parent half of daemonising: re-exec detached, then wait for the state file.
fn spawn_and_wait(argv: &[String], dir: &Path) -> Result<(), CliError> {
    let exe = daemon::current_exe()?;
    let log = state::log_path(dir);
    let child_args = daemon::child_args(&argv[1..]);

    let mut child = daemon::spawn_detached(&exe, &child_args, &log)
        .map_err(|e| CliError::failure(format!("could not start the bolide daemon: {e}")))?;
    let pid = child.id();

    let result = daemon::await_startup(
        dir,
        pid,
        &mut || daemon::status_of(&mut child),
        &|| std::thread::sleep(daemon::NAP),
        daemon::STARTUP_ATTEMPTS,
        &log,
    );
    match result {
        Ok(state) => {
            println!("connected to {}", state.remote);
            println!("computer-use on {}", state.endpoint);
            Ok(())
        }
        Err(stop) => {
            // Do not leave an orphan holding somebody's desktop.
            let _ = child.kill();
            let _ = child.wait();
            Err(stop)
        }
    }
}

/// The child half — or what `--foreground` does when a person asks for it directly.
async fn serve_foreground(
    target: crate::cli::Target,
    args: ConnectArgs,
    dir: &Path,
) -> Result<(), CliError> {
    init_tracing();

    let password = resolve_password(
        args.password_file.as_deref(),
        std::env::var(PASSWORD_ENV).ok(),
    )?;
    // The password lives exactly here: read from a file or the environment, handed to
    // the handshake, and dropped. It is not put in `args`, the state file or a log.
    let config = bolide_rfb::Config {
        username: args.username.clone(),
        password,
        ..Default::default()
    };

    let remote = target.to_string();
    let session = bolide_rfb::connect(&remote, config)
        .await
        .map_err(|e| CliError::failure(format!("cannot connect to {remote}: {e}")))?;

    let (width, height) = session.size();
    let desktop_name = session.desktop_name();
    let session: Arc<dyn bolide_rfb::Session> = Arc::new(session);

    let running = bolide_server::serve(
        Arc::clone(&session),
        bolide_server::ServerConfig {
            bind: args.listen,
            token: args.token.clone(),
            remote: remote.clone(),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| CliError::failure(format!("cannot listen on {}: {e}", args.listen)))?;

    let endpoint = format!("http://{}", running.addr);
    let state = SessionState {
        endpoint: endpoint.clone(),
        pid: std::process::id(),
        remote: remote.clone(),
        username: args.username.clone(),
        token: args.token.clone(),
        started_at: state::now_rfc3339(),
    };
    state::save_in(dir, &state)
        .map_err(|e| CliError::failure(format!("cannot write the state file: {e}")))?;

    println!("connected to {remote} ({width}x{height}, {desktop_name:?})");
    println!("computer-use on {endpoint}");
    let _ = std::io::stdout().flush();

    wait_for_the_end(Arc::clone(&session)).await;

    running.shutdown().await;
    let _ = session.close().await;
    let _ = state::clear_in(dir);
    Ok(())
}

/// Until the desktop goes away, or somebody asks us to stop.
async fn wait_for_the_end(session: Arc<dyn bolide_rfb::Session>) {
    let mut events = session.events();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    loop {
        let terminated = async {
            match sigterm.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            _ = terminated => return,
            event = events.recv() => match event {
                Ok(bolide_rfb::ServerEvent::Disconnected { reason }) => {
                    if let Some(reason) = reason {
                        eprintln!("the desktop disconnected: {reason}");
                    }
                    return;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            },
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("BOLIDE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
