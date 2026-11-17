//! The web UI built into `web/dist` is compiled into the binary.

use foundryd::api::assets::WebAssets;

#[test]
fn the_built_web_ui_is_embedded() {
    let index = WebAssets::get("index.html").expect("index.html is embedded");
    let html = String::from_utf8_lossy(&index.data);

    assert!(
        !html.contains("/src/main.tsx"),
        "index.html is the unbuilt source: {html}"
    );

    let script = html
        .split("src=\"/")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("index.html loads a script");
    assert!(
        WebAssets::get(script).is_some(),
        "the script index.html loads, {script}, is embedded"
    );
}
