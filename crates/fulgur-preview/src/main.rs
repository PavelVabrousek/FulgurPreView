#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::path::PathBuf;

fn create_app_icon() -> egui::IconData {
    let png_bytes = include_bytes!("../assets/fulgurpreview.png");
    let decoded = image::load_from_memory(png_bytes)
        .expect("The embedded FulgurPreView application icon must be a valid PNG")
        .into_rgba8();
    let (width, height) = decoded.dimensions();
    egui::IconData {
        rgba: decoded.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result<()> {
    fulgur_preview::crash::install_panic_hook();

    #[cfg(windows)]
    {
        let identity: Vec<u16> = "FulgurPreView.Desktop\0".encode_utf16().collect();
        // SAFETY: identity is a valid null-terminated UTF-16 string.
        let _ = unsafe {
            windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                identity.as_ptr(),
            )
        };
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--smoke-test") {
        let target = args.get(1).map(PathBuf::from);
        match fulgur_preview::smoke::run_m1_verification(target.as_deref()) {
            Ok(report) => {
                if let Ok(json) = serde_json::to_string_pretty(&report) {
                    println!("{json}");
                }
                if report.status != "PASS" {
                    std::process::exit(1);
                }
                return Ok(());
            }
            Err(err) => {
                eprintln!("Smoke test failed: {err}");
                std::process::exit(1);
            }
        }
    }

    let initial_target = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("FulgurPreView {}", fulgur_preview::VERSION))
        .with_decorations(false)
        .with_icon(create_app_icon())
        .with_inner_size([1360.0, 840.0])
        .with_min_inner_size([860.0, 520.0])
        .with_drag_and_drop(true);

    eframe::run_native(
        "FulgurPreView",
        eframe::NativeOptions {
            viewport,
            centered: true,
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(fulgur_preview::app::FulgurPreViewApp::new(
                cc,
                initial_target,
            )))
        }),
    )
}
