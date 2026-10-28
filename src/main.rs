//! ghost: drive Windows desktop apps in the background. UIA patterns first, posted/sent window messages
//! second. Never moves the real cursor, never injects global input, and gives the foreground back whenever a
//! target app grabs it. `ghost.exe` is a thin client; `ghost.exe --daemon` is the resident server.
mod capture;
mod cdp;
mod client;
mod daemon;
mod engine;
mod keys;
mod proto;
mod win;

fn main() {
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if args.first().map(|a| a == "--daemon").unwrap_or(false) {
        daemon::run();
        return;
    }
    std::process::exit(client::run(args));
}
