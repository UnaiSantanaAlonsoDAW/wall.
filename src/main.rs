#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod carousel;
mod core;
mod gallery;
mod platform;
mod startup;
mod thumbnails;
mod worker;

use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("wall: {error}");
        if !std::env::args().any(|arg| {
            matches!(
                arg.as_str(),
                "--validate-wallpaper" | "--validate-ui" | "--quit"
            )
        }) {
            rfd::MessageDialog::new()
                .set_title("wall.")
                .set_description(&error)
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut data_dir = None;
    let mut background = false;
    let mut validate = None;
    let mut quit = false;
    let mut validate_ui = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => {
                data_dir = Some(PathBuf::from(
                    args.next().ok_or("--data-dir requires a folder")?,
                ))
            }
            Some("--background") => background = true,
            Some("--quit") => quit = true,
            Some("--validate-ui") => validate_ui = true,
            Some("--validate-wallpaper") => {
                validate = Some(PathBuf::from(
                    args.next()
                        .ok_or("--validate-wallpaper requires a generated test image")?,
                ))
            }
            _ => return Err("Usage: wall.exe [--background] [--data-dir FOLDER]".into()),
        }
    }
    if quit {
        platform::quit_existing()?;
        return Ok(());
    }
    let Some(_instance) = platform::Instance::acquire(!background && validate.is_none())? else {
        return if validate.is_some() {
            Err("Close wall. before running wallpaper validation".into())
        } else {
            Ok(())
        };
    };
    let store = core::Store::open(data_dir)?;
    if let Some(path) = validate {
        let result = platform::validate_wallpaper_roundtrip(&path, &store.root.join("applied"));
        let report = match &result {
            Ok(()) => "PASS: test wallpaper applied and original desktop restored.".to_owned(),
            Err(e) => format!("FAIL: {e}"),
        };
        std::fs::write(store.root.join("wallpaper-validation.txt"), report)
            .map_err(|e| e.to_string())?;
        return result;
    }
    let validation_root = store.root.clone();
    let guard = if validate_ui {
        Some(platform::WallpaperValidation::begin(
            &store.root.join("ui-validation"),
        )?)
    } else {
        None
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("wall.")
            .with_inner_size([1160.0, 500.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_taskbar(false)
            .with_visible(false)
            .with_window_level(eframe::egui::WindowLevel::AlwaysOnTop)
            .with_icon(app::icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let result = eframe::run_native(
        "wall.",
        options,
        Box::new(move |cc| {
            app::WallApp::new(cc, store, background, validate_ui)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(Box::<dyn std::error::Error + Send + Sync>::from)
        }),
    )
    .map_err(|e| e.to_string());
    if let Some(guard) = guard {
        let restored = guard.finish();
        let report = match &restored {
            Ok(()) => "PASS: original monitor wallpapers, fit and enabled state restored.",
            Err(error) => error.as_str(),
        };
        std::fs::write(validation_root.join("ui-wallpaper-validation.txt"), report)
            .map_err(|e| e.to_string())?;
        restored?;
    }
    result
}
