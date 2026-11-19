//! One sleeping decoder worker, a bounded request queue and an LRU of GPU thumbnails.
//! Filesystem work and full image decoding never run in the UI frame.
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::UNIX_EPOCH,
};

const MAX_TEXTURES: usize = 16;
const GALLERY_TEXTURES: usize = 24;
const MAX_QUEUED: usize = 12;
const THUMB_WIDTH: u32 = 1280;
const THUMB_HEIGHT: u32 = 720;
const DISK_BUDGET: u64 = 256 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
pub enum ThumbState {
    Ready {
        texture: TextureHandle,
        width: u32,
        height: u32,
    },
    Pending,
    Failed(String),
}

struct Entry {
    id: u64,
    touched: u64,
    generation: u64,
    fingerprint: Option<PathBuf>,
    state: ThumbState,
}
struct Request {
    path: PathBuf,
    id: u64,
    fingerprint: Option<PathBuf>,
}
struct Queue {
    requests: VecDeque<Request>,
    shutdown: bool,
}
struct Decoded {
    image: image::RgbaImage,
    width: u32,
    height: u32,
}
struct Reply {
    path: PathBuf,
    id: u64,
    result: Result<Validated, String>,
}

struct Validated {
    fingerprint: PathBuf,
    /// None means the existing GPU texture still matches the source file.
    decoded: Option<Decoded>,
}

pub struct Thumbnails {
    ctx: egui::Context,
    entries: HashMap<PathBuf, Entry>,
    capacity: usize,
    queue: Arc<(Mutex<Queue>, Condvar)>,
    replies: mpsc::Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
    clock: u64,
    generation: u64,
}

impl Thumbnails {
    pub fn new(ctx: egui::Context, cache_dir: PathBuf) -> Self {
        let queue = Arc::new((
            Mutex::new(Queue {
                requests: VecDeque::new(),
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let (tx, replies) = mpsc::channel();
        let worker_queue = Arc::clone(&queue);
        let worker_ctx = ctx.clone();
        let worker = thread::Builder::new()
            .name("wall-thumbnails".into())
            .spawn(move || {
                let _ = fs::create_dir_all(&cache_dir);
                prune_cache(&cache_dir, DISK_BUDGET);
                let mut writes = 0_u32;
                loop {
                    let request = {
                        let (mutex, wake) = &*worker_queue;
                        let Ok(mut queue) = mutex.lock() else {
                            break;
                        };
                        while queue.requests.is_empty() && !queue.shutdown {
                            let Ok(next) = wake.wait(queue) else {
                                return;
                            };
                            queue = next;
                        }
                        if queue.shutdown {
                            break;
                        }
                        // Recent, visible requests take precedence during fast scrolling.
                        queue.requests.pop_back()
                    };
                    let Some(request) = request else {
                        continue;
                    };
                    let result = std::panic::catch_unwind(|| {
                        validate_thumbnail(
                            &request.path,
                            &cache_dir,
                            request.fingerprint.as_deref(),
                        )
                    })
                    .unwrap_or_else(|_| Err("This image could not be decoded safely".into()));
                    if tx
                        .send(Reply {
                            path: request.path,
                            id: request.id,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    worker_ctx.request_repaint();
                    writes += 1;
                    if writes.is_multiple_of(32) {
                        prune_cache(&cache_dir, DISK_BUDGET);
                    }
                }
            })
            .ok();
        Self {
            ctx,
            entries: HashMap::new(),
            capacity: MAX_TEXTURES,
            queue,
            replies,
            worker,
            clock: 0,
            generation: 0,
        }
    }

    pub fn poll(&mut self) {
        while let Ok(reply) = self.replies.try_recv() {
            let Some(entry) = self.entries.get_mut(&reply.path) else {
                continue;
            };
            if entry.id != reply.id {
                continue;
            }
            match reply.result {
                Ok(validated) => {
                    entry.fingerprint = Some(validated.fingerprint);
                    let Some(decoded) = validated.decoded else {
                        continue;
                    };
                    let size = [
                        decoded.image.width() as usize,
                        decoded.image.height() as usize,
                    ];
                    let texture = self.ctx.load_texture(
                        format!("wall-thumb-{}", reply.id),
                        ColorImage::from_rgba_unmultiplied(size, decoded.image.as_raw()),
                        TextureOptions::LINEAR,
                    );
                    entry.state = ThumbState::Ready {
                        texture,
                        width: decoded.width,
                        height: decoded.height,
                    };
                }
                Err(error) => {
                    entry.fingerprint = None;
                    entry.state = ThumbState::Failed(error);
                }
            }
        }
    }

    pub fn get(&mut self, path: &Path) -> ThumbState {
        self.poll();
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(path) {
            entry.touched = self.clock;
            if entry.generation == self.generation {
                return entry.state.clone();
            }
        }
        if self.worker.is_none() {
            return ThumbState::Failed("The image worker could not start".into());
        }
        let (mutex, wake) = &*self.queue;
        let Ok(mut queue) = mutex.lock() else {
            return ThumbState::Failed("The image worker is unavailable".into());
        };
        if queue.requests.len() >= MAX_QUEUED {
            return self
                .entries
                .get(path)
                .map(|e| e.state.clone())
                .unwrap_or(ThumbState::Pending);
        }
        let id = self.clock;
        if !self.entries.contains_key(path) {
            evict_lru(&mut self.entries, self.capacity);
            self.entries.insert(
                path.to_path_buf(),
                Entry {
                    id,
                    touched: self.clock,
                    generation: self.generation,
                    fingerprint: None,
                    state: ThumbState::Pending,
                },
            );
        }
        let Some(entry) = self.entries.get_mut(path) else {
            return ThumbState::Pending;
        };
        entry.id = id;
        entry.generation = self.generation;
        queue.requests.push_back(Request {
            path: path.to_path_buf(),
            id,
            fingerprint: entry.fingerprint.clone(),
        });
        wake.notify_one();
        entry.state.clone()
    }

    /// Call when reopening the overlay. Cached files are revalidated against source
    /// metadata on the worker, including previously missing or corrupt originals.
    /// Unchanged images retain GPU textures and need no decode or upload.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.poll();
    }

    /// Stop prefetching when the overlay is hidden. Retain resident images for
    /// instant reopening; release pending decoded pixels without GPU uploads.
    /// At most the single decode already in progress can finish in the worker.
    pub fn set_gallery(&mut self, expanded: bool) {
        self.capacity = if expanded {
            GALLERY_TEXTURES
        } else {
            MAX_TEXTURES
        };
        while self.entries.len() > self.capacity {
            evict_lru(&mut self.entries, self.capacity);
        }
    }

    /// Cancel queued work left behind by a gallery scroll. The active decode
    /// can finish normally, and any resident texture stays available.
    pub fn retain_queued(&mut self, visible: &[PathBuf]) {
        let (mutex, _) = &*self.queue;
        let Ok(mut queue) = mutex.lock() else { return };
        queue.requests.retain(|request| {
            if visible.contains(&request.path) {
                return true;
            }
            if let Some(entry) = self.entries.get_mut(&request.path)
                && entry.id == request.id
            {
                entry.generation = self.generation.wrapping_sub(1);
            }
            false
        });
    }

    pub fn suspend(&mut self) {
        if let Ok(mut queue) = self.queue.0.lock() {
            queue.requests.clear();
        }
        self.generation = self.generation.wrapping_add(1);
        self.entries.retain(|_, entry| {
            // IDs begin at one. Invalidating IDs also rejects a late reply from
            // a revalidation that was running when the overlay disappeared.
            entry.id = 0;
            !matches!(entry.state, ThumbState::Pending)
        });
        while self.replies.try_recv().is_ok() {}
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        eprintln!("wall: stopping thumbnail worker");
        if let Ok(mut queue) = self.queue.0.lock() {
            queue.shutdown = true;
            queue.requests.clear();
            self.queue.1.notify_one();
        }
        // At most the current bounded decode finishes; no polling or stranded thread.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(debug_assertions)]
        eprintln!("wall: thumbnail worker stopped");
    }
}

fn evict_lru(entries: &mut HashMap<PathBuf, Entry>, capacity: usize) {
    if entries.len() >= capacity {
        let oldest = entries
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(path, _)| path.clone());
        if let Some(oldest) = oldest {
            entries.remove(&oldest);
        }
    }
}

fn cache_path(path: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("Image unavailable: {e}"))?;
    if !metadata.is_file() {
        return Err("This reference is not a file".into());
    }
    let mut hash = DefaultHasher::new();
    "wall-thumbnail-v2-1280x720".hash(&mut hash);
    path.hash(&mut hash);
    metadata.len().hash(&mut hash);
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .hash(&mut hash);
    Ok(cache_dir.join(format!("{:016x}.thumb", hash.finish())))
}

fn validate_thumbnail(
    path: &Path,
    cache_dir: &Path,
    known_fingerprint: Option<&Path>,
) -> Result<Validated, String> {
    let cached = cache_path(path, cache_dir)?;
    let decoded = if known_fingerprint == Some(cached.as_path()) {
        None
    } else {
        Some(load_thumbnail_at(path, &cached)?)
    };
    Ok(Validated {
        fingerprint: cached,
        decoded,
    })
}

#[cfg(test)]
fn load_thumbnail(path: &Path, cache_dir: &Path) -> Result<Decoded, String> {
    load_thumbnail_at(path, &cache_path(path, cache_dir)?)
}

fn load_thumbnail_at(path: &Path, cached: &Path) -> Result<Decoded, String> {
    if let Some(decoded) = read_cache(cached) {
        return Ok(decoded);
    }
    let mut reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| format!("Cannot read image: {e}"))?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP)
    ) {
        return Err("Use a JPEG, PNG or WebP image".into());
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let original = reader
        .decode()
        .map_err(|e| format!("Cannot decode image: {e}"))?;
    let (width, height) = (original.width(), original.height());
    let thumbnail = if width <= THUMB_WIDTH && height <= THUMB_HEIGHT {
        original.into_rgba8()
    } else {
        original.thumbnail(THUMB_WIDTH, THUMB_HEIGHT).into_rgba8()
    };
    // Cache failures do not make readable originals unusable (e.g. disk full).
    let mut bytes = b"WTH2".to_vec();
    bytes.extend(width.to_le_bytes());
    bytes.extend(height.to_le_bytes());
    let mut encoded = Cursor::new(Vec::new());
    if thumbnail
        .write_to(&mut encoded, image::ImageFormat::Png)
        .is_ok()
    {
        bytes.extend(encoded.into_inner());
        let _ = crate::core::atomic_write(cached, &bytes);
    }
    Ok(Decoded {
        image: thumbnail,
        width,
        height,
    })
}

fn read_cache(path: &Path) -> Option<Decoded> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() < 12 || &bytes[..4] != b"WTH2" {
        return None;
    }
    let width = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if width == 0 || height == 0 || width > 32768 || height > 32768 {
        return None;
    }
    let mut reader =
        image::ImageReader::with_format(Cursor::new(&bytes[12..]), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(THUMB_WIDTH);
    limits.max_image_height = Some(THUMB_HEIGHT);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?.to_rgba8();
    Some(Decoded {
        image,
        width,
        height,
    })
}

fn prune_cache(directory: &Path, budget: u64) {
    let Ok(files) = fs::read_dir(directory) else {
        return;
    };
    let mut total = 0_u64;
    let mut entries = Vec::new();
    for file in files.flatten() {
        let path = file.path();
        if path.extension().and_then(|x| x.to_str()) != Some("thumb") {
            continue;
        }
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        total = total.saturating_add(size);
        entries.push((metadata.modified().unwrap_or(UNIX_EPOCH), path, size));
    }
    entries.sort_unstable_by_key(|entry| entry.0);
    for (_, path, size) in entries {
        if total <= budget {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
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
                "wall-thumbs-{}-{}",
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
    fn thumbnail_keeps_aspect_and_cache_recovers_from_corruption() {
        let temp = Temp::new();
        let source = temp.0.join("source.png");
        image::RgbImage::new(1600, 900).save(&source).unwrap();
        let first = load_thumbnail(&source, &temp.0).unwrap();
        assert_eq!((first.width, first.height), (1600, 900));
        assert_eq!((first.image.width(), first.image.height()), (1280, 720));
        let cache = cache_path(&source, &temp.0).unwrap();
        let cached = read_cache(&cache).unwrap();
        assert_eq!(cached.image, first.image);
        assert!(
            validate_thumbnail(&source, &temp.0, Some(&cache))
                .unwrap()
                .decoded
                .is_none(),
            "An unchanged resident thumbnail must reuse its GPU texture"
        );
        fs::write(&cache, b"broken").unwrap();
        assert!(load_thumbnail(&source, &temp.0).is_ok());
        assert!(read_cache(&cache).is_some());
    }

    #[test]
    fn changed_file_metadata_changes_cache_key() {
        let temp = Temp::new();
        let source = temp.0.join("source.png");
        fs::write(&source, b"a").unwrap();
        let first = cache_path(&source, &temp.0).unwrap();
        fs::write(&source, b"longer").unwrap();
        assert_ne!(first, cache_path(&source, &temp.0).unwrap());
    }

    #[test]
    fn malformed_and_missing_images_are_errors() {
        let temp = Temp::new();
        let source = temp.0.join("broken.png");
        fs::write(&source, b"no image").unwrap();
        assert!(load_thumbnail(&source, &temp.0).is_err());
        assert!(load_thumbnail(&temp.0.join("missing.jpg"), &temp.0).is_err());
    }

    #[test]
    fn suspend_keeps_ready_textures_and_rejects_late_decoder_replies() {
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "resident",
            ColorImage::from_rgba_unmultiplied([1, 1], &[12, 34, 56, 255]),
            TextureOptions::LINEAR,
        );
        let texture_id = texture.id();
        let ready = PathBuf::from("resident.png");
        let pending = PathBuf::from("pending.png");
        let (tx, replies) = mpsc::channel();
        let mut thumbnails = Thumbnails {
            ctx,
            capacity: MAX_TEXTURES,
            entries: HashMap::from([
                (
                    ready.clone(),
                    Entry {
                        id: 1,
                        touched: 1,
                        generation: 0,
                        fingerprint: Some(PathBuf::from("key.thumb")),
                        state: ThumbState::Ready {
                            texture,
                            width: 1,
                            height: 1,
                        },
                    },
                ),
                (
                    pending.clone(),
                    Entry {
                        id: 2,
                        touched: 2,
                        generation: 0,
                        fingerprint: None,
                        state: ThumbState::Pending,
                    },
                ),
            ]),
            queue: Arc::new((
                Mutex::new(Queue {
                    requests: VecDeque::from([Request {
                        path: pending.clone(),
                        id: 2,
                        fingerprint: None,
                    }]),
                    shutdown: false,
                }),
                Condvar::new(),
            )),
            replies,
            worker: None,
            clock: 2,
            generation: 0,
        };
        tx.send(Reply {
            path: pending.clone(),
            id: 2,
            result: Err("Discard this completed decode".into()),
        })
        .unwrap();
        thumbnails.suspend();
        assert!(thumbnails.queue.0.lock().unwrap().requests.is_empty());
        assert!(!thumbnails.entries.contains_key(&pending));
        assert!(thumbnails.replies.try_recv().is_err());
        tx.send(Reply {
            path: ready.clone(),
            id: 1,
            result: Err("Late in-flight result".into()),
        })
        .unwrap();
        thumbnails.poll();
        match &thumbnails.entries[&ready].state {
            ThumbState::Ready { texture, .. } => assert_eq!(texture.id(), texture_id),
            _ => panic!("A stale reply must not replace a retained texture"),
        }
    }

    #[test]
    fn cache_pruning_and_lru_are_bounded() {
        let temp = Temp::new();
        fs::write(temp.0.join("a.thumb"), [0; 20]).unwrap();
        fs::write(temp.0.join("b.thumb"), [0; 20]).unwrap();
        fs::write(temp.0.join("unrelated.png"), [0; 20]).unwrap();
        prune_cache(&temp.0, 20);
        assert_eq!(
            fs::read_dir(&temp.0)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|f| f.path().extension().is_some_and(|e| e == "thumb"))
                .count(),
            1
        );
        assert!(temp.0.join("unrelated.png").exists());
        let mut entries = HashMap::new();
        for i in 0..100 {
            evict_lru(&mut entries, MAX_TEXTURES);
            entries.insert(
                PathBuf::from(i.to_string()),
                Entry {
                    id: i,
                    touched: i,
                    generation: 0,
                    fingerprint: None,
                    state: ThumbState::Pending,
                },
            );
        }
        assert_eq!(entries.len(), MAX_TEXTURES);
        assert!(!entries.contains_key(Path::new("0")));
        assert!(entries.contains_key(Path::new("99")));
    }
}
