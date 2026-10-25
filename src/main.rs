use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("-V" | "--version") => {
            println!("agent86 {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("-h" | "--help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!(
                "agent86: `{command}` is not implemented yet; the Rust port is at the core-types stage"
            );
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "agent86 {}\n\
         Experimental Rust implementation of the agent86 harness.\n\n\
         Usage: agent86 [--help | --version]\n\n\
         The agent runtime is not implemented yet.",
        env!("CARGO_PKG_VERSION")
    );
}
