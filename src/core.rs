//! Local references and crash-safe preferences. Original images are never copied or deleted.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Windows virtual-key code (A–Z or 0–9 in the preferences UI).
    pub key: u32,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key: u32::from(b'W'),
        }
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        match char::from_u32(self.key) {
            Some(c) if c.is_ascii_alphanumeric() => write!(f, "{c}"),
            _ => write!(f, "Key {}", self.key),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayAppearance {
    #[default]
    Automatic,
    Light,
    Dark,
}

impl<'de> Deserialize<'de> for TrayAppearance {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::Automatic,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub hotkey: Hotkey,
    pub glass: bool,
    pub tray_appearance: TrayAppearance,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: Hotkey::default(),
            glass: true,
            tray_appearance: TrayAppearance::Automatic,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Collection {
    pub paths: Vec<PathBuf>,
    pub settings: Settings,
}

pub struct Store {
    pub root: PathBuf,
    pub collection: Collection,
}

#[derive(Default, Debug)]
pub struct ImportReport {
    pub added: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub errors: Vec<String>,
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} added", self.added)?;
        if self.duplicates > 0 {
            write!(f, " · {} already in collection", self.duplicates)?;
        }
        if self.rejected > 0 {
            write!(f, " · {} unavailable or unsupported", self.rejected)?;
        }
        Ok(())
    }
}

impl Store {
    pub fn open(optional_root: Option<PathBuf>) -> Result<Self, String> {
        let root = match optional_root {
            Some(root) => root,
            None => PathBuf::from(
                std::env::var_os("LOCALAPPDATA")
                    .ok_or("Windows did not provide a local application data folder")?,
            )
            .join("wall"),
        };
        fs::create_dir_all(&root).map_err(|e| format!("Cannot create wall data folder: {e}"))?;
        let root =
            fs::canonicalize(&root).map_err(|e| format!("Cannot resolve wall data folder: {e}"))?;
        let config = root.join("collection.json");
        let collection = match fs::File::open(&config) {
            Ok(file) => {
                // Bound memory even if another program replaced the settings file.
                let mut bytes = Vec::new();
                file.take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("Cannot read collection: {e}"))?;
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(
                        "Collection exceeds the 16 MiB safety limit; the file has been preserved"
                            .into(),
                    );
                }
                let mut collection: Collection = serde_json::from_slice(&bytes).map_err(|e| {
                    format!("Cannot read collection.json; it has been preserved: {e}")
                })?;
                let mut seen = HashSet::new();
                collection
                    .paths
                    .retain(|path| seen.insert(path_identity(path)));
                if !(collection.settings.hotkey.ctrl || collection.settings.hotkey.alt)
                    || !matches!(collection.settings.hotkey.key, 0x30..=0x39 | 0x41..=0x5a)
                {
                    collection.settings.hotkey = Hotkey::default();
                }
                collection
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Collection::default(),
            Err(e) => return Err(format!("Cannot open collection: {e}")),
        };
        Ok(Self { root, collection })
    }

    pub fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.collection)
            .map_err(|e| format!("Cannot encode collection: {e}"))?;
        atomic_write(&self.root.join("collection.json"), &bytes)
            .map_err(|e| format!("Cannot save collection: {e}"))
    }

    /// Run imports on a worker: canonicalization and header reads can wait on a disk.
    pub fn import(&mut self, paths: Vec<PathBuf>) -> ImportReport {
        let mut report = ImportReport::default();
        let mut seen: HashSet<String> = self
            .collection
            .paths
            .iter()
            .map(|p| path_identity(p))
            .collect();
        for path in paths {
            let result = validate_reference(&path);
            match result {
                Ok(path) => {
                    if seen.insert(path_identity(&path)) {
                        self.collection.paths.push(path);
                        report.added += 1;
                    } else {
                        report.duplicates += 1;
                    }
                }
                Err(error) => {
                    report.rejected += 1;
                    // Do not retain thousands of duplicate diagnostics from a large drop.
                    if report.errors.len() < 8 {
                        report.errors.push(error);
                    }
                }
            }
        }
        report
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.collection.paths.len() {
            self.collection.paths.remove(index);
        }
    }

    pub fn thumbnail_dir(&self) -> PathBuf {
        self.root.join("thumbnails-v2")
    }
}

fn path_identity(path: &Path) -> String {
    // Windows paths are case insensitive by default. Store the original canonical spelling.
    let value = path.to_string_lossy().to_lowercase();
    if let Some(unc) = value.strip_prefix(r"\\?\unc\") {
        format!(r"\\{unc}")
    } else {
        value.trim_start_matches(r"\\?\").to_owned()
    }
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    path_identity(a) == path_identity(b)
}

pub(crate) fn safe_image_dimensions(width: u32, height: u32) -> bool {
    (1..=32768).contains(&width)
        && (1..=32768).contains(&height)
        && u64::from(width) * u64::from(height) <= 100_000_000
}

fn validate_reference(path: &Path) -> Result<PathBuf, String> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") {
        return Err(format!("{}: use JPEG, PNG or WebP", path.display()));
    }
    let canonical = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !canonical.is_file() {
        return Err(format!("{} is not an image file", path.display()));
    }
    let reader = image::ImageReader::open(&canonical)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP)
    ) {
        return Err(format!("{}: unsupported image contents", path.display()));
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !safe_image_dimensions(width, height) {
        return Err(format!(
            "{}: image exceeds safe dimensions (100 megapixels)",
            path.display()
        ));
    }
    Ok(canonical)
}

/// Flush a sibling temporary file, then atomically replace the destination on Windows.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Both buffers are terminated and remain valid for this synchronous call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "wall-core-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn imports_references_deduplicates_and_rejects_invalid_files() {
        let temp = Temp::new();
        let image = temp.0.join("sample.png");
        image::RgbaImage::new(12, 8).save(&image).unwrap();
        let corrupt = temp.0.join("broken.png");
        fs::write(&corrupt, b"not a picture").unwrap();
        let mut store = Store::open(Some(temp.0.join("data"))).unwrap();
        let report = store.import(vec![
            image.clone(),
            image.clone(),
            corrupt,
            temp.0.join("missing.jpg"),
            temp.0.join("file.txt"),
        ]);
        assert_eq!(
            (report.added, report.duplicates, report.rejected),
            (1, 1, 3)
        );
        assert_eq!(store.collection.paths[0], fs::canonicalize(&image).unwrap());
        store.remove(0);
        assert!(
            image.exists(),
            "Removing a reference must not delete the original"
        );
    }

    #[test]
    fn atomic_save_replaces_existing_settings_and_round_trips() {
        let temp = Temp::new();
        let mut store = Store::open(Some(temp.0.clone())).unwrap();
        store.save().unwrap();
        store.collection.settings.glass = false;
        store.collection.settings.tray_appearance = TrayAppearance::Dark;
        store.save().unwrap();
        let loaded = Store::open(Some(temp.0.clone())).unwrap();
        assert!(!loaded.collection.settings.glass);
        assert_eq!(
            loaded.collection.settings.tray_appearance,
            TrayAppearance::Dark
        );
        assert!(
            !temp
                .0
                .join(format!("collection.tmp-{}", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn missing_or_invalid_tray_appearance_keeps_legacy_surface_and_collection() {
        for json in [
            r#"{"paths":["sample.jpg"],"settings":{"glass":false}}"#,
            r#"{"paths":["sample.jpg"],"settings":{"glass":false,"tray_appearance":"unknown"}}"#,
            r#"{"paths":["sample.jpg"],"settings":{"glass":false,"tray_appearance":42}}"#,
        ] {
            let collection: Collection = serde_json::from_str(json).unwrap();
            assert_eq!(collection.paths, [PathBuf::from("sample.jpg")]);
            assert!(!collection.settings.glass);
            assert_eq!(
                collection.settings.tray_appearance,
                TrayAppearance::Automatic
            );
        }
    }

    #[test]
    fn corrupt_settings_are_reported_and_preserved() {
        let temp = Temp::new();
        let path = temp.0.join("collection.json");
        fs::write(&path, "{oops").unwrap();
        assert!(Store::open(Some(temp.0.clone())).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{oops");
    }

    #[test]
    fn unsafe_or_unusable_hotkey_is_reset() {
        let temp = Temp::new();
        let mut store = Store::open(Some(temp.0.clone())).unwrap();
        store.collection.settings.hotkey = Hotkey {
            ctrl: false,
            alt: false,
            shift: true,
            key: 0,
        };
        store.save().unwrap();
        assert_eq!(
            Store::open(Some(temp.0.clone()))
                .unwrap()
                .collection
                .settings
                .hotkey,
            Hotkey::default()
        );
        assert_eq!(Hotkey::default().to_string(), "Alt+W");
    }
}
