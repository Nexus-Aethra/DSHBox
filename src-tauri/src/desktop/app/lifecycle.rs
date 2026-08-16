use super::*;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

#[tauri::command]
pub(crate) fn enqueue_container_start(
    id: String,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_start",
        serde_json::json!({ "id": id }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_container_stop(
    id: String,
    _manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    // The daemon owns the host process; close the local front window as soon
    // as the stop request is accepted (the host dies moments later).
    if let Some(window) = app.get_webview_window(&format!("dsh-front-{id}")) {
        let _ = window.close();
    }
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_stop",
        serde_json::json!({ "id": id }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

#[tauri::command]
pub(crate) fn enqueue_container_rebuild(
    id: String,
    _manager: tauri::State<TaskManager>,
    _app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    let client = connect()?;
    let value = call(
        &client,
        "enqueue_container_rebuild",
        serde_json::json!({ "id": id }),
    )?;
    serde_json::from_value(value)
        .map_err(|error| format!("invalid task record: {error}"))
}

/// Best-effort display name for a container window title, falling back to
/// the container id when the metadata cannot be read.
fn container_display_name(id: &str) -> String {
    let Ok(config) = read_config() else {
        return id.to_owned();
    };
    let Some(root) = config.runtime_directory else {
        return id.to_owned();
    };
    let Ok(metadata) = fs::read_to_string(container_directory(&root, id).join("container.json"))
    else {
        return id.to_owned();
    };
    serde_json::from_str::<serde_json::Value>(&metadata)
        .ok()
        .and_then(|value| value["name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| id.to_owned())
}

#[tauri::command]
// IMPORTANT: this command MUST be `async`. Building a webview window from
// a synchronous command handler deadlocks on Windows (wry#583): the native
// window gets created but `build()` never returns, leaving a blank window
// and silently skipping every statement after it. In an async command the
// body runs on the tokio runtime, where `build()` can safely block while
// the main event loop pumps the WebView2 controller creation.
pub(crate) async fn open_dsh_front(
    id: String,
    _manager: tauri::State<'_, ContainerManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // The daemon owns the host process, so its URL comes from the daemon;
    // window management stays local (WebView2 details, focus, retries).
    let client = connect()?;
    let value = call(&client, "container_url", serde_json::json!({ "id": id }))?;
    let url = value["url"]
        .as_str()
        .ok_or("invalid container url response")?
        .to_owned();
    write_startup_log(&format!("open_dsh_front called for {id}: {url}"));
    let label = format!("dsh-front-{id}");
    let window_title = format!("{} - DSH", container_display_name(&id));
    if let Some(window) = app.get_webview_window(&label) {
        write_startup_log("open_dsh_front: window exists, showing");
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        // A reused window may still show the previous host's URL: Stop closes
        // the window best-effort, and a close that lags (or is dropped) leaves
        // the stale page on screen while the old host process is already gone.
        // Force a navigation whenever the current URL differs, otherwise the
        // page keeps fetching the dead port and reports "Failed to fetch".
        let stale = window
            .url()
            .map(|current| current.as_str() != url.as_str())
            .unwrap_or(true);
        if stale {
            let target: tauri::Url = url
                .parse()
                .map_err(|error| format!("DSH front invalid url {url}: {error}"))?;
            let _ = window.navigate(target);
        }
        return Ok(());
    }
    let probe_app = app.clone();
    let probe_label = label.clone();
    let probe_url = url.clone();
    // Open the DSH host URL directly. IMPORTANT: do NOT set
    // additional_browser_args here — it forces a second WebView2
    // environment with different options but the same user data dir,
    // which leaves webviews in a broken state (blank window, navigation
    // never issued; tauri-apps/tauri#11144).
    let target: tauri::Url = url
        .parse()
        .map_err(|error| format!("DSH front invalid url {url}: {error}"))?;
    let window = WebviewWindowBuilder::new(
        &app,
        label,
        WebviewUrl::External(target),
    )
    .title(&window_title)
    .build()
    .map_err(|error| {
        write_startup_log(&format!("DSH front open failed: {error}"));
        format!("DSH front open failed: {error}")
    })?;
    write_startup_log("open_dsh_front: window built");
    // WebView2 stalls the initial navigation/rendering of a webview whose
    // window is not visible in the foreground (confirmed with a lab app:
    // the same window renders as soon as it is brought to the front and
    // stays blank while created in the background). Force the new window
    // to the front right after building it, then re-focus and re-trigger
    // the navigation from a background thread: the main window tends to
    // steal focus back immediately, and WebView2 only starts navigating
    // once the controller sees its window in the foreground.
    let _ = window.show();
    let _ = window.set_focus();
    let retry_app = app.clone();
    let retry_app_inner = app.clone();
    let retry_label = probe_label.clone();
    let retry_url = url.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        // Navigation calls must run on the main thread; hop back over.
        let _ = retry_app.run_on_main_thread(move || {
            if let Some(window) = retry_app_inner.get_webview_window(&retry_label) {
                let _ = window.show();
                let _ = window.set_focus();
                // Only re-trigger the navigation while the window has not
                // reached the target URL yet: a loaded page would just be
                // reloaded needlessly.
                let still_blank = window
                    .url()
                    .map(|current| current.as_str() != retry_url.as_str())
                    .unwrap_or(true);
                if still_blank {
                    if let Ok(target) = retry_url.parse() {
                        let _ = window.navigate(target);
                    }
                }
            }
        });
    });
    write_startup_log(&format!("DSH front opened: {url}"));
    // Diagnostics only: the page title probe is no longer used to open the
    // system browser automatically. WebView2 navigation is fixed by the
    // no-proxy + foreground focus handling above, and the DSH page keeps its
    // default title while the in-app notice modal is showing, so the probe
    // used to misjudge successful loads and pop a browser window on every
    // open. The manual open_dsh_front_browser command stays for fallback.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(15));
        let loaded = probe_app
            .get_webview_window(&probe_label)
            .map(|window| {
                window
                    .url()
                    .map(|current| current.as_str() == probe_url.as_str())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !loaded {
            write_startup_log(&format!(
                "DSH front did not reach {probe_url} after 15s"
            ));
        }
    });
    Ok(())
}

#[tauri::command]
pub(crate) fn open_dsh_front_browser(
    id: String,
    _manager: tauri::State<ContainerManager>,
) -> Result<(), String> {
    if !is_safe_identifier(&id) {
        return Err("invalid container id".to_owned());
    }
    let client = connect()?;
    let value = call(&client, "container_url", serde_json::json!({ "id": id }))?;
    let url = value["url"]
        .as_str()
        .ok_or("invalid container url response")?
        .to_owned();
    webbrowser::open(&url).map_err(|error| format!("cannot open system browser: {error}"))
}
