//! One sleeping worker serializes disk writes and wallpaper operations, away from rendering.
use crate::{
    core::{Collection, Settings, Store},
    platform,
};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

#[derive(Clone, Copy)]
pub struct ImportContext {
    pub epoch: u64,
    pub reveal: bool,
}

pub enum Picker {
    Images,
    Folder,
}

pub enum Command {
    Import(Vec<PathBuf>, ImportContext),
    Pick(Picker, ImportContext),
    PickedImages(Result<Option<Vec<PathBuf>>, String>, ImportContext),
    Shutdown,
    Remove(PathBuf),
    ReadStartup,
    Settings(Settings, Option<bool>),
    Apply(PathBuf, std::time::Instant),
    Current(u64),
    #[cfg(debug_assertions)]
    Capture(isize),
}

pub enum Event {
    Startup(Result<bool, String>),
    Snapshot {
        collection: Collection,
        message: Option<String>,
        reopen: bool,
        imported: Option<(usize, ImportContext)>,
    },
    Applied(Result<PathBuf, String>),
    Current {
        request: u64,
        result: Result<Option<PathBuf>, String>,
    },
    #[cfg(debug_assertions)]
    Diagnostic(Result<PathBuf, String>),
}

pub struct Worker {
    sender: Option<Sender<Command>>,
    thread: Option<thread::JoinHandle<()>>,
    pub events: Receiver<Event>,
}

impl Worker {
    pub fn new(mut store: Store, ctx: egui::Context, validate_ui: bool) -> Result<Self, String> {
        let (sender, commands) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let picker_sender = sender.clone();
        let thread = thread::Builder::new()
            .name("wall-files".into())
            .spawn(move || {
                let mut wallpaper = platform::WallpaperClient::default();
                while let Ok(command) = commands.recv() {
                    let event = match command {
                        Command::Shutdown => break,
                        Command::ReadStartup => Event::Startup(crate::startup::enabled()),
                        #[cfg(debug_assertions)]
                        Command::Capture(hwnd) => {
                            let path = store.root.join("desktop-capture.png");
                            Event::Diagnostic(
                                platform::capture_own_window(hwnd, &path).map(|()| path),
                            )
                        }
                        Command::Pick(picker, context) => {
                            let sender = picker_sender.clone();
                            let result = thread::Builder::new()
                                .name("wall-file-dialog".into())
                                .spawn(move || {
                                    let dialog = rfd::FileDialog::new();
                                    let paths = match picker {
                                        Picker::Images => Ok(dialog
                                            .set_title("Add wallpapers")
                                            .add_filter(
                                                "Wallpapers",
                                                &["jpg", "jpeg", "png", "webp"],
                                            )
                                            .pick_files()),
                                        Picker::Folder => dialog
                                            .set_title("Import wallpaper folder")
                                            .pick_folder()
                                            .map(|folder| crate::core::folder_images(&folder))
                                            .transpose(),
                                    };
                                    let _ = sender.send(Command::PickedImages(paths, context));
                                });
                            match result {
                                Ok(_) => continue,
                                Err(error) => Event::Snapshot {
                                    collection: store.collection.clone(),
                                    message: Some(format!("Cannot open chooser: {error}")),
                                    reopen: true,
                                    imported: None,
                                },
                            }
                        }
                        Command::Current(request) => {
                            Event::Current {
                                request,
                                result: wallpaper.current().map(|current| {
                                    current.map(|current| {
                                        let direct = store
                                            .collection
                                            .paths
                                            .iter()
                                            .find(|path| crate::core::same_path(path, &current))
                                            .cloned();
                                        direct
                                            .or_else(|| {
                                                let cache = store.root.join("applied");
                                                if !current.parent().is_some_and(|parent| {
                                                    crate::core::same_path(parent, &cache)
                                                }) {
                                                    return None;
                                                }
                                                // Only the currently applied conversion is examined; no
                                                // filesystem sweep across a large collection on opening.
                                                use std::io::Read;
                                                let file = std::fs::File::open(
                                                    current.with_extension("source.json"),
                                                )
                                                .ok()?;
                                                let mut bytes = Vec::new();
                                                file.take(32769).read_to_end(&mut bytes).ok()?;
                                                if bytes.len() > 32768 {
                                                    return None;
                                                }
                                                let original: PathBuf =
                                                    serde_json::from_slice(&bytes).ok()?;
                                                store
                                                    .collection
                                                    .paths
                                                    .iter()
                                                    .find(|path| {
                                                        crate::core::same_path(path, &original)
                                                            && platform::wallpaper_matches(
                                                                path, &current, &cache,
                                                            )
                                                    })
                                                    .cloned()
                                            })
                                            .unwrap_or(current)
                                    })
                                }),
                            }
                        }
                        Command::Apply(path, requested) => {
                            let queued = requested.elapsed();
                            let result = wallpaper.apply(&path, &store.root.join("applied"));
                            if validate_ui {
                                use std::io::Write;
                                let total = requested.elapsed();
                                if let Ok(mut file) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(store.root.join("ui-application-events.txt"))
                                {
                                    match &result {
                                        Ok(applied) => {
                                            let _ = writeln!(
                                                file,
                                                "APPLIED {}",
                                                applied.timing_line(queued, total)
                                            );
                                        }
                                        Err(error) => {
                                            let _ = writeln!(file, "FAILED {error}");
                                        }
                                    }
                                }
                            }
                            Event::Applied(result.map(|_| path))
                        }
                        Command::Settings(settings, startup) => {
                            let previous =
                                std::mem::replace(&mut store.collection.settings, settings);
                            let message = match store.save() {
                                Err(error) => {
                                    store.collection.settings = previous;
                                    error
                                }
                                Ok(()) => match startup.map(crate::startup::set_enabled) {
                                    Some(Err(error)) => {
                                        format!("Preferences saved. Startup: {error}")
                                    }
                                    _ => "Preferences saved".into(),
                                },
                            };
                            Event::Snapshot {
                                collection: store.collection.clone(),
                                message: Some(message),
                                reopen: false,
                                imported: None,
                            }
                        }
                        command => {
                            let before = store.collection.clone();
                            let reopen = matches!(command, Command::PickedImages(..));
                            let mut changed = true;
                            let mut imported = None;
                            let message = match command {
                                Command::Import(paths, context)
                                | Command::PickedImages(Ok(Some(paths)), context) => {
                                    let start = store.collection.paths.len();
                                    let empty = paths.is_empty();
                                    let report = store.import(paths);
                                    changed = report.added > 0;
                                    if changed {
                                        imported = Some((start, context));
                                    }
                                    Some(if empty {
                                        "No JPEG, PNG or WebP images found in this folder".into()
                                    } else {
                                        report.to_string()
                                    })
                                }
                                Command::PickedImages(result, _) => {
                                    changed = false;
                                    result.err()
                                }
                                Command::Remove(path) => {
                                    if let Some(index) =
                                        store.collection.paths.iter().position(|p| p == &path)
                                    {
                                        store.remove(index);
                                    }
                                    Some("Removed from collection. Original file kept.".into())
                                }
                                _ => unreachable!(),
                            };
                            let message = if changed {
                                match store.save() {
                                    Ok(()) => message,
                                    Err(error) => {
                                        store.collection = before;
                                        imported = None;
                                        Some(error)
                                    }
                                }
                            } else {
                                message
                            };
                            Event::Snapshot {
                                collection: store.collection.clone(),
                                message,
                                reopen,
                                imported,
                            }
                        }
                    };
                    if events_tx.send(event).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .map_err(|e| format!("Cannot start file worker: {e}"))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            events,
        })
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        self.sender
            .as_ref()
            .ok_or("File worker stopped")?
            .send(command)
            .map_err(|_| "File worker stopped".into())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        eprintln!("wall: stopping file worker");
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Command::Shutdown);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        #[cfg(debug_assertions)]
        eprintln!("wall: file worker stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_image_returns_an_apply_error_without_creating_a_conversion() {
        let root = std::env::temp_dir().join(format!("wall-missing-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let store = Store::open(Some(root.clone())).unwrap();
        let worker = Worker::new(store, egui::Context::default(), false).unwrap();
        worker
            .send(Command::Apply(
                root.join("absent.png"),
                std::time::Instant::now(),
            ))
            .unwrap();
        let event = worker
            .events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(event, Event::Applied(Err(message)) if message.contains("unavailable")));
        assert!(!root.join("applied").exists());
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_persistence_rolls_back_the_published_preferences() {
        let root = std::env::temp_dir().join(format!(
            "wall-worker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(Some(root.clone())).unwrap();
        // A directory at the destination makes atomic replacement fail reliably.
        std::fs::create_dir(root.join("collection.json")).unwrap();
        let worker = Worker::new(store, egui::Context::default(), false).unwrap();
        worker
            .send(Command::Settings(
                Settings {
                    glass: false,
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        match worker
            .events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
        {
            Event::Snapshot {
                collection,
                message,
                ..
            } => {
                assert!(collection.settings.glass);
                assert!(message.unwrap().contains("Cannot save collection"));
            }
            _ => panic!("Expected a collection result"),
        }
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }
}
