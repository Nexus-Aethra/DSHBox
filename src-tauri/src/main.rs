// Release builds are GUI-only on Windows: no console window should appear.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod desktop;

fn main() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        desktop::write_startup_log(&format!("panic: {info}"));
        previous_hook(info);
    }));
    desktop::write_startup_log(&format!(
        "process started (os: {}, arch: {}, pid: {})",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id()
    ));
    if let Some(code) = cli::run() {
        desktop::write_startup_log(&format!("CLI exited with code {code}"));
        std::process::exit(code);
    }
    desktop::run();
    desktop::write_startup_log("desktop run returned");
}
