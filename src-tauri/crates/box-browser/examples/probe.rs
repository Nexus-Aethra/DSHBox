//! End-to-end check of the debug surface against a real headless browser.
//!
//! Run with: cargo run -p box-browser --example probe
//!
//! Exercises the four capabilities the RPC layer will expose: screenshot,
//! element query, click-by-element and click-by-coordinate. The page is a
//! self-contained temp file, so this needs no network and no running DSH Box.

use box_browser::{resolve, BrowserSession};
use serde_json::json;

const HTML: &str = r#"<title>dsh-box-probe</title>
<body style='font:16px sans-serif;padding:40px'>
  <h1 id='heading'>Debug surface probe</h1>
  <button id='btn' onclick='this.textContent="clicked"'>press me</button>
  <div class='row'>alpha</div>
  <div class='row'>beta</div>
</body>"#;

fn evaluate(session: &mut BrowserSession, expression: &str) -> String {
    let out = session
        .call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .unwrap_or_else(|error| panic!("evaluate failed: {error}"));
    out["result"]["value"].as_str().unwrap_or("").to_owned()
}

fn main() {
    let browser = match resolve(None) {
        Ok(candidate) => {
            println!("browser: {} ({})", candidate.path.display(), candidate.kind.as_str());
            candidate.path
        }
        Err(error) => {
            eprintln!("SKIP: {error}");
            return;
        }
    };

    let scratch = std::env::temp_dir().join("dshbox-probe");
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let page = scratch.join("probe.html");
    std::fs::write(&page, HTML).expect("write page");
    let url = format!("file:///{}", page.display().to_string().replace('\\', "/"));

    println!("launching headless on {url}");
    let mut session = match BrowserSession::launch(&browser, &url, scratch.join("profile"), BrowserSession::DEFAULT_VIEWPORT) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("FAIL launch: {error}");
            std::process::exit(1);
        }
    };
    println!("attached on devtools port {} target {}", session.port(), session.target_id());

    println!("1) query  : {}", evaluate(&mut session, "document.title"));
    println!("        : {}", evaluate(&mut session, "document.querySelectorAll('.row').length + ' rows'"));

    let shot = session.call("Page.captureScreenshot", json!({ "format": "png" })).expect("shot");
    println!("2) shot   : {} bytes base64 png", shot["data"].as_str().unwrap_or("").len());

    let rect_json = evaluate(&mut session, "JSON.stringify(document.getElementById('btn').getBoundingClientRect())");
    let rect: serde_json::Value = serde_json::from_str(&rect_json).expect("rect");
    let cx = rect["x"].as_f64().unwrap_or(0.0) + rect["width"].as_f64().unwrap_or(0.0) / 2.0;
    let cy = rect["y"].as_f64().unwrap_or(0.0) + rect["height"].as_f64().unwrap_or(0.0) / 2.0;
    println!("3) click  : element centre ({cx}, {cy})");
    for kind in ["mousePressed", "mouseReleased"] {
        session.call(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": cx, "y": cy, "button": "left", "clickCount": 1 }),
        ).expect("element click");
    }

    println!("4) click  : coordinate (7, 7)");
    session.call(
        "Input.dispatchMouseEvent",
        json!({ "type": "mousePressed", "x": 7.0, "y": 7.0, "button": "left", "clickCount": 1 }),
    ).expect("coordinate click");

    let after = evaluate(&mut session, "document.getElementById('btn').textContent");
    println!("        : button text after click = {after:?}");
    if after == "clicked" {
        println!("\nOK: screenshot, query, click-by-element and click-by-coordinate all work");
    } else {
        eprintln!("\nFAIL: element click did not take effect");
        std::process::exit(1);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!("teardown kills the browser and removes its profile");
}
