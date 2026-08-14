// Release builds are GUI-only on Windows: no console window should appear.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod desktop;

fn main() {
    // WebView2 arguments applied globally through the official environment
    // variable: every webview picks the same arguments up, so all windows
    // share one consistent environment. Passing additional_browser_args to a
    // single window instead creates a second environment with different
    // options on the same user data dir, which breaks webview navigation
    // (tauri-apps/tauri#11144). Set it before anything else runs so the
    // WebView2 loader sees it when the first environment is created.
    //
    // --no-proxy-server is the critical one: system proxy tools can silently
    // swallow WebView2 network navigations (the initial navigation of an
    // external-URL webview never fires, leaving a blank window), while the
    // main window is unaffected because tauri serves it through virtual host
    // mapping instead of the network stack. Reproduced with a minimal lab
    // app: adding this flag makes external pages render.
    // --disable-gpu keeps windows from going black after a fullscreen round
    // trip on some GPU drivers.
    #[cfg(target_os = "windows")]
    if std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none() {
        // SAFETY: very first statement of the process; no other threads exist
        // yet, so mutating the environment is safe.
        unsafe {
            std::env::set_var(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                "--disable-gpu --no-proxy-server",
            );
        }
    }
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
