// Hide the console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod download;
mod history;
mod logger;
mod naming;
mod settings;
mod zoom;

use std::time::Duration;

slint::include_modules!();

fn main() {
    logger::init(settings::log_file());
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("Crash: {info}");
        default_hook(info);
    }));

    if let Err(e) = run() {
        log::error!("Fatal: {e:#}");
        // Release builds have no console on Windows, so surface the error visually.
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Zoom Downloader")
            .set_description(format!("Zoom Downloader could not start:\n\n{e:#}"))
            .show();
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zoom-worker")
        .build()?;

    // Slint creates winit windows as transparent, which makes the macOS title bar see-through.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|attrs| {
            attrs.with_transparent(false).with_theme(Some(slint::winit_030::winit::window::Theme::Dark))
        })
        .select()?;

    let ui = AppWindow::new()?;
    let _app = app::App::install(&ui, rt.handle().clone());
    #[cfg(target_os = "macos")]
    slint::Timer::single_shot(Duration::ZERO, set_dock_icon);
    ui.run()?;

    // Partial downloads are kept as .part files and resume on the next run.
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// A bare (unbundled) binary has no icon on macOS, so set the Dock icon at runtime.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}
