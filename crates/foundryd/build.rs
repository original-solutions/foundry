//! Rebuilds `foundryd` when `web/dist` changes, including when it first appears: the embed in
//! `src/api/assets.rs` compiles without the folder, and nothing else tells cargo to look again.

fn main() {
    println!("cargo:rerun-if-changed=../../web/dist");
}
