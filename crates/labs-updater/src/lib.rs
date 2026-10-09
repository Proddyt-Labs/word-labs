//! Proddyt Switch updates (LABS-156): the app's fork publishes GitHub releases; at start this asks
//! when a newer one is out — Update, Not now, or Skip this version. Update downloads the installer
//! (or the portable archive), asks to close the app, and a small PowerShell helper installs it and
//! reopens the app.
//!
//! Only copies made by the release workflow take part: they carry a `VERSION` file (the release
//! tag) next to the program, and the installed ones also its uninstaller. Builds from source, the
//! web build and other systems never ask. Nothing is downloaded before the user says yes.
//!
//! One call per frame, from the app's `eframe::App::logic`:
//! `labs_updater::frame(ctx, "photo-labs", "Photo Labs");`

#[cfg(all(windows, not(target_arch = "wasm32")))]
mod imp;
mod version;

pub use version::{is_newer, newer};

/// Drive the updater: the start-up check, the question and the download (cheap when idle).
pub fn frame(ctx: &egui::Context, repo: &'static str, name: &'static str) {
    #[cfg(all(windows, not(target_arch = "wasm32")))]
    imp::frame(ctx, repo, name);
    #[cfg(not(all(windows, not(target_arch = "wasm32"))))]
    let _ = (ctx, repo, name);
}
