//! The FileChooser's preview pane (ADR-0017): the file under the listing
//! cursor previews in a pane to the right of the directory listing when it
//! is a format with a mature, cheap decode — PNG, JPEG, GIF (first frame),
//! WebP, and BMP. Anything else keeps the full width for browsing; the
//! pane never blocks or delays the dialog.
//!
//! The pipeline is deliberately boring: a bounded decode with strict
//! dimension and allocation caps, an aspect-preserving downsample to the
//! pane's texture budget, premultiplication (the canvas image pipeline
//! samples 8-bit RGBA as premultiplied sRGB), then a one-time upload to a
//! GPU texture through the device iris owns. Decoding runs on a worker
//! thread so a slow or large image cannot stall the frame loop; results
//! cross back through the same main-thread wake the notification daemon
//! uses, keyed by path and modification time so stale decodes are dropped.
//!
//! Pure decisions (format detection, fit math, pixel transforms, size
//! formatting) live here as free functions so they test without a window.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use super::super::{DevicePtr, TextureHandle, wake_main_thread};

/// The largest texture edge (device pixels) a preview may upload. The pane
/// is 224 logical px wide; a 3× display gets a crisp 672 px edge, and the
/// budget stays under half a megabyte per texture.
pub const PREVIEW_TEXTURE_BUDGET: u32 = 672;
/// Strict decode caps: no image larger than this on an edge decodes.
pub const MAX_DECODE_EDGE: u32 = 16_382;
/// The total decode allocation cap (the `image` crate's `max_alloc`,
/// applied before pixels are materialized).
pub const MAX_DECODE_ALLOC: u64 = 96 * 1024 * 1024;
/// Files larger than this refuse to decode (a cheap stat check before the
/// reader is even opened).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// How many decoded textures stay cached (most recent first).
const CACHE_CAPACITY: usize = 24;

/// The raster formats the pane will decode, matched by extension only —
/// cheap, unguessing, and consistent with the portal's own filter
/// matching, which also works from names.
pub fn preview_format(path: &Path) -> Option<image::ImageFormat> {
    use image::ImageFormat;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "apng" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jfif" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        "bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// The downsampled size for a source image: the largest size with the
/// source's aspect that fits the [`PREVIEW_TEXTURE_BUDGET`] square, never
/// upscaling. A 1×1 image stays 1×1 (the upload path rejects zero edges).
pub fn thumbnail_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let scale = (PREVIEW_TEXTURE_BUDGET as f64 / width as f64)
        .min(PREVIEW_TEXTURE_BUDGET as f64 / height as f64)
        .min(1.0);
    let w = ((width as f64 * scale).round() as u32).max(1);
    let h = ((height as f64 * scale).round() as u32).max(1);
    (w, h)
}

/// Convert 8-bit straight-alpha RGBA to the premultiplied form the canvas
/// image pipeline samples, in place. Clamped per channel so the round trip
/// through a lookup stays exact.
pub fn premultiply_rgba(pixels: &mut [u8]) {
    // `as_chunks_mut` (clippy 1.98's chunks_exact_to_as_chunks): the slice
    // length is always a whole number of pixels from the decoder.
    let (chunks, _) = pixels.as_chunks_mut::<4>();
    for px in chunks {
        let a = px[3] as u32;
        px[0] = ((px[0] as u32 * a + 127) / 255).min(255) as u8;
        px[1] = ((px[1] as u32 * a + 127) / 255).min(255) as u8;
        px[2] = ((px[2] as u32 * a + 127) / 255).min(255) as u8;
    }
}

/// Human-readable byte size, matching the chooser's quiet caption style:
/// `1.4 MB`, `912 kB`, `42 bytes`.
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1000;
    const MB: u64 = 1000 * 1000;
    const KB_MAX: u64 = MB - 1;
    match bytes {
        0..=999 => format!("{bytes} bytes"),
        KB..=KB_MAX => format!("{:.1} kB", bytes as f64 / KB as f64),
        _ => format!("{:.1} MB", bytes as f64 / MB as f64),
    }
}

/// A decoded preview ready for upload.
pub struct DecodedPreview {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    /// Straight-alpha RGBA8 rows from the decoder.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The source image's full dimensions, before downsampling.
    pub source_size: (u32, u32),
    /// The file's size on disk (the caption reads it from the decode, not
    /// from a per-frame stat).
    pub file_bytes: u64,
}

/// Decode `path` into a preview-sized bitmap, or a user-facing reason why
/// not. Runs on the worker thread; touches no UI state.
pub fn decode_preview(path: &Path) -> Result<DecodedPreview, String> {
    let meta = std::fs::metadata(path).map_err(|error| format!("{error}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {}", format_size(MAX_FILE_BYTES)));
    }
    let modified = meta.modified().ok();
    // The extension gate keeps unsupported formats out before any I/O; the
    // decode itself is format-driven from the sniffed bytes.
    preview_format(path).ok_or_else(|| "no preview for this file type".to_owned())?;

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    let reader = image::ImageReader::open(path).map_err(|error| format!("{error}"))?;
    // The content sniff refines the extension guess (`open` seeds the
    // format from the name; a successful sniff replaces it), so a renamed
    // file still decodes when its bytes identify. An unknown extension
    // never reaches here — `preview_format` gated it — and an undecodable
    // format surfaces from `into_decoder` as an Unsupported error.
    let mut reader = reader
        .with_guessed_format()
        .map_err(|error| format!("{error}"))?;
    reader.limits(limits);

    // Split the decoder out so the Exif orientation applies to the pixels
    // before any downsample (phone photos carry rotated Exif).
    let mut decoder = reader.into_decoder().map_err(|error| format!("{error}"))?;
    use image::ImageDecoder as _;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| format!("{e}"))?;
    image.apply_orientation(orientation);
    let source_size = (image.width(), image.height());
    let (tw, th) = thumbnail_size(source_size.0, source_size.1);
    let thumb = if (tw, th) != source_size {
        image.thumbnail(tw, th)
    } else {
        image
    };
    let rgba = thumb.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedPreview {
        path: path.to_path_buf(),
        modified,
        pixels: rgba.into_raw(),
        width,
        height,
        source_size,
        file_bytes: meta.len(),
    })
}

/// One cached preview texture, with the metadata that validated it.
struct CacheSlot {
    key: (PathBuf, Option<SystemTime>),
    texture: TextureHandle,
    /// Source dimensions for the caption.
    source_size: (u32, u32),
    /// The file's size on disk at decode time (the caption avoids a
    /// per-frame `stat`).
    file_bytes: u64,
    last_used: Instant,
}

/// What the pane shows for the current cursor file this frame.
pub enum PreviewState {
    /// The cursor is not on a previewable file; the pane collapses.
    Hidden,
    /// A decode is in flight; the pane shows a quiet placeholder.
    Loading,
    /// The decoded texture is ready, with the caption's file size.
    Ready {
        texture: TextureHandle,
        source_size: (u32, u32),
        file_bytes: u64,
    },
    /// The file could not be decoded; `reason` is user-facing.
    Failed { reason: String },
}

/// The pane's host-owned state: the worker channel, the texture cache, and
/// the pinned device. Created per dialog run.
pub struct PreviewPanel {
    device: Option<DevicePtr>,
    /// The path handed to the worker, with the mtime it was validated
    /// against; `None` until the first previewable cursor row.
    pending: Option<(PathBuf, Option<SystemTime>)>,
    /// A finished decode waiting for a device to upload through. Headless
    /// runs (no device) park here instead of respawning the worker every
    /// frame; a device arriving later uploads it on the next ask.
    awaiting_upload: Option<Box<DecodedPreview>>,
    results: mpsc::Receiver<DecodeOutcome>,
    cache: Vec<CacheSlot>,
    /// The failure shown until the cursor moves off the file it belongs to.
    failure: Option<((PathBuf, Option<SystemTime>), String)>,
}

/// What the worker sends back: the decode result or the fact that the
/// request was cancelled (superseded by a newer cursor target).
enum DecodeOutcome {
    Decoded(Box<DecodedPreview>),
    Failed { path: PathBuf, reason: String },
}

impl PreviewPanel {
    /// A pane with no device yet (headless tests, or a run whose start
    /// callback never fired).
    pub fn new() -> PreviewPanel {
        let (_tx, rx) = mpsc::channel();
        PreviewPanel {
            device: None,
            pending: None,
            awaiting_upload: None,
            results: rx,
            cache: Vec::new(),
            failure: None,
        }
    }

    /// Capture iris's device (called from the run's start callback).
    pub fn attach_device(&mut self, device: DevicePtr) {
        self.device = Some(device);
    }

    /// Release device-backed textures (called from the run's stop
    /// callback, before iris destroys the device).
    pub fn release(&mut self) {
        self.cache.clear();
        self.device = None;
    }

    /// Whether no decode is in flight (tests observe the worker settling).
    #[cfg(test)]
    pub fn pending_is_none(&self) -> bool {
        self.pending.is_none()
    }

    /// The pane's state for `target` this frame: hides for non-previewable
    /// files, loads for uncached ones (spawning the worker on first ask),
    /// and serves cached textures immediately.
    pub fn state_for(&mut self, target: Option<&Path>) -> PreviewState {
        let Some(path) = target.filter(|path| preview_format(path).is_some()) else {
            self.pending = None;
            return PreviewState::Hidden;
        };
        self.drain_results();
        let modified = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok();
        let key = (path.to_path_buf(), modified);
        if let Some(slot) = self.cache.iter_mut().find(|slot| slot.key == key) {
            slot.last_used = Instant::now();
            return ready_from_slot(slot);
        }
        if let Some(((fail_path, fail_mtime), reason)) = self.failure.clone()
            && (fail_path, fail_mtime) == key
        {
            return PreviewState::Failed { reason };
        }
        // A finished decode waiting on a device: try the upload now (a
        // device may have arrived since); a decode for a different target
        // (the cursor moved) is dropped as stale.
        if let Some(decoded) = self.awaiting_upload.take() {
            if (decoded.path.clone(), decoded.modified) != key {
                return PreviewState::Loading;
            }
            match self.upload(decoded) {
                Ok(()) => {
                    let slot = self.cache.iter_mut().find(|slot| slot.key == key);
                    if let Some(slot) = slot {
                        slot.last_used = Instant::now();
                        return ready_from_slot(slot);
                    }
                }
                // No device yet (headless): park it again — the pane keeps
                // its loading state, one decode per file, no respawn
                // storm, and a later device uploads it.
                Err(decoded) => self.awaiting_upload = Some(decoded),
            }
            return PreviewState::Loading;
        }
        if self.pending.as_ref() != Some(&key) {
            self.spawn_decode(key.clone());
            self.pending = Some(key);
        }
        PreviewState::Loading
    }

    /// Pull finished decodes off the worker channel. A decode that no
    /// longer matches the pending target is dropped; a failure is recorded
    /// so the pane can show it (the cursor may have moved on already, in
    /// which case the next `state_for` hides it).
    fn drain_results(&mut self) {
        while let Ok(outcome) = self.results.try_recv() {
            let stale = match &outcome {
                DecodeOutcome::Decoded(decoded) => {
                    let key = (decoded.path.clone(), decoded.modified);
                    self.pending.as_ref() != Some(&key)
                }
                DecodeOutcome::Failed { path, .. } => {
                    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                    self.pending.as_ref() != Some(&(path.clone(), modified))
                }
            };
            if stale {
                continue;
            }
            match outcome {
                DecodeOutcome::Decoded(decoded) => {
                    // The decode finished; the upload itself may have to
                    // wait for a device, so park it either way and clear
                    // the pending target (state_for retries the upload).
                    self.awaiting_upload = Some(decoded);
                    self.pending = None;
                }
                DecodeOutcome::Failed { path, reason } => {
                    let modified = std::fs::metadata(path.as_path())
                        .and_then(|m| m.modified())
                        .ok();
                    self.failure = Some(((path, modified), reason));
                    self.pending = None;
                }
            }
        }
    }

    /// Upload a decode and admit it to the cache. `Err` hands the decode
    /// back when there is no device to upload through yet (the headless
    /// case), so the caller can park it for a later retry.
    fn upload(&mut self, decoded: Box<DecodedPreview>) -> Result<(), Box<DecodedPreview>> {
        let Some(device) = self.device else {
            return Err(decoded);
        };
        let DecodedPreview {
            path,
            modified,
            mut pixels,
            width,
            height,
            source_size,
            file_bytes,
        } = *decoded;
        premultiply_rgba(&mut pixels);
        let Ok(texture) = TextureHandle::from_premultiplied_rgba(&device, width, height, &pixels)
        else {
            return Ok(());
        };
        if self.cache.len() >= CACHE_CAPACITY
            && let Some(oldest) = self
                .cache
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(index, _)| index)
        {
            self.cache.remove(oldest);
        }
        log::debug!(
            "preview texture {width}×{height} uploaded for {}",
            path.display()
        );
        self.cache.push(CacheSlot {
            key: (path, modified),
            texture,
            source_size,
            file_bytes,
            last_used: Instant::now(),
        });
        Ok(())
    }

    /// Hand `key` to a fresh worker thread. The channel is per request:
    /// superseded results arrive on an older channel and are dropped by
    /// the receiver swap below.
    fn spawn_decode(&mut self, key: (PathBuf, Option<SystemTime>)) {
        let (tx, rx) = mpsc::channel();
        self.results = rx;
        let (path, _modified) = key;
        std::thread::Builder::new()
            .name("preview-decode".to_owned())
            .spawn(move || {
                let outcome = match decode_preview(&path) {
                    Ok(decoded) => DecodeOutcome::Decoded(Box::new(decoded)),
                    Err(reason) => DecodeOutcome::Failed {
                        path: path.clone(),
                        reason,
                    },
                };
                // The send fails only when the pane was dropped with the
                // dialog; the worker then simply exits.
                let _ = tx.send(outcome);
                // Nudge the run loop so the finished decode is observed
                // without waiting for unrelated input.
                wake_main_thread();
            })
            .map(|_| ())
            .unwrap_or_else(|error| {
                log::debug!("preview worker spawn failed: {error}");
            });
    }
}

impl Default for PreviewPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Duplicate a cache entry's texture without giving up ownership: the C
/// side is refcounted, so a retain keeps both handles valid.
fn clone_texture(texture: &TextureHandle) -> TextureHandle {
    TextureHandle::from_retained(texture)
}

/// Hand a cache slot out as this frame's [`PreviewState::Ready`], with the
/// caption data the slot captured at decode time.
fn ready_from_slot(slot: &mut CacheSlot) -> PreviewState {
    let texture = clone_texture(&slot.texture);
    PreviewState::Ready {
        texture,
        source_size: slot.source_size,
        file_bytes: slot.file_bytes,
    }
}

/// A one-time probe so tests can confirm the decode caps are consistent.
#[cfg(test)]
pub fn decode_caps() -> (u32, u64, u64) {
    (MAX_DECODE_EDGE, MAX_FILE_BYTES, MAX_DECODE_ALLOC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_formats_match_by_extension() {
        use image::ImageFormat;
        assert_eq!(
            preview_format(Path::new("/a/b.PNG")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            preview_format(Path::new("/a/b.jpg")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            preview_format(Path::new("/a/b.jpeg")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            preview_format(Path::new("/a/b.gif")),
            Some(ImageFormat::Gif)
        );
        assert_eq!(
            preview_format(Path::new("/a/b.webp")),
            Some(ImageFormat::WebP)
        );
        assert_eq!(
            preview_format(Path::new("/a/b.bmp")),
            Some(ImageFormat::Bmp)
        );
        assert_eq!(preview_format(Path::new("/a/b.txt")), None);
        assert_eq!(preview_format(Path::new("/a/b.svg")), None);
        assert_eq!(preview_format(Path::new("/a/b")), None);
    }

    #[test]
    fn thumbnail_size_never_upscales_and_keeps_aspect() {
        // Landscape downsizes to the budget, aspect kept.
        assert_eq!(thumbnail_size(4000, 2000), (672, 336));
        // Portrait likewise.
        assert_eq!(thumbnail_size(2000, 4000), (336, 672));
        // Small images stay exact.
        assert_eq!(thumbnail_size(100, 50), (100, 50));
        // Tiny images stay 1×1 minimum, never zero.
        assert_eq!(thumbnail_size(1, 1), (1, 1));
        // Zero dims are invalid.
        assert_eq!(thumbnail_size(0, 10), (0, 0));
    }

    #[test]
    fn premultiply_is_exact_for_corners() {
        let mut opaque = [255, 128, 0, 255];
        premultiply_rgba(&mut opaque);
        assert_eq!(opaque, [255, 128, 0, 255]);
        let mut transparent = [255, 128, 0, 0];
        premultiply_rgba(&mut transparent);
        assert_eq!(transparent, [0, 0, 0, 0]);
        let mut half = [200, 100, 10, 128];
        premultiply_rgba(&mut half);
        // (200*128+127)/255 = 100.7 → 100; halves round, never clip past.
        assert_eq!(half[0], 100);
        assert_eq!(half[1], 50);
        assert_eq!(half[2], 5);
        // Overflow clamps: straight alpha >255-equivalent cannot exceed a.
        let mut hot = [255, 255, 255, 254];
        premultiply_rgba(&mut hot);
        assert_eq!(hot[0], 254);
        assert_eq!(hot[3], 254);
    }

    #[test]
    fn sizes_format_quietly() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(42), "42 bytes");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1000), "1.0 kB");
        assert_eq!(format_size(912_640), "912.6 kB");
        assert_eq!(format_size(1_433_196), "1.4 MB");
    }

    #[test]
    fn caps_stay_bounded() {
        let (edge, file, alloc) = decode_caps();
        assert!(edge >= PREVIEW_TEXTURE_BUDGET);
        assert!(file <= 64 * 1024 * 1024);
        assert!(alloc <= 128 * 1024 * 1024);
    }
    #[test]
    fn fit_keeps_aspect_inside_the_box() {
        use crate::ui::fit_within;
        assert_eq!(fit_within(672.0, 336.0, 224.0, 224.0), (224.0, 112.0));
        assert_eq!(fit_within(336.0, 672.0, 224.0, 224.0), (112.0, 224.0));
        assert_eq!(fit_within(100.0, 50.0, 224.0, 224.0), (224.0, 112.0));
        assert_eq!(fit_within(0.0, 50.0, 224.0, 224.0), (0.0, 0.0));
    }
}
