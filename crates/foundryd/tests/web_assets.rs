//! The web UI built into `web/dist` is compiled into the binary.

use foundryd::api::assets::WebAssets;

#[test]
fn the_built_web_ui_is_embedded() {
    let index = WebAssets::get("index.html").expect("index.html is embedded");
    let html = String::from_utf8_lossy(&index.data);

    assert!(html.contains("<div id=\"root\">"), "index.html: {html}");
}
