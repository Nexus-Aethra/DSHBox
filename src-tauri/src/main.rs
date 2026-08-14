mod cli;
mod desktop;

fn main() {
    if let Some(code) = cli::run() {
        std::process::exit(code);
    }
    desktop::run();
}
