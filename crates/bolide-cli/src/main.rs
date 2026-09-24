//! `bolide` — wiring only. Every decision worth testing lives in the library next door.

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    std::process::ExitCode::from(bolide_cli::run::main(&argv))
}
