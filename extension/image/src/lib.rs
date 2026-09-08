//! Bounded, worker-local RGBA8 image processing for the BT WASI extension ABI.
//! Pixels remain behind an opaque handle; only explicitly encoded bytes cross the ABI.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use bt_extension_sdk::{
    bt_extension, bt_extension_shutdown, bt_extension_stats, expect_arg_count,
    expect_ext_object_type, expect_int, expect_string, BtResult, BtValue, ExtObject, ObjectStore,
};
use font8x8::UnicodeFonts;
use image::{
    imageops, DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits, RgbImage,
    Rgba, RgbaImage,
};

/// Maximum pixel count of one decoded image, including temporary watermarks.
const MAX_PIXELS: u64 = 16_777_216;
/// Maximum sum of pixels retained by one WASM worker (128 MiB RGBA8).
const MAX_RETAINED_PIXELS: u64 = 33_554_432;
/// Maximum number of image handles in a worker.
const MAX_OBJECTS: usize = 32;
/// Largest accepted encoded file; files are streamed through a buffered reader.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Encoded Bytes cap leaves room for the BtValueBinary envelope and arguments.
const MAX_BYTES: usize = 16 * 1024 * 1024 - 64;
/// Maximum encoded file output, enforced before every writer operation.
const MAX_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
/// Rotating starting slot for a bounded pool of same-directory output staging files.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
/// A crashed or interrupted writer cannot accumulate more than four files per destination.
const OUTPUT_TEMP_SLOTS: u64 = 4;

/// Maximum UTF-8 bytes retained for one normalized path.
const MAX_PATH_BYTES: usize = 4096;

/// A bound path and optional pixels; construction performs no filesystem I/O.
struct Image {
    /// Host-normalized project path used only for the first lazy decode.
    path: String,
    /// Owned RGBA8 pixels, absent until a pixel operation, create or decode.
    pixels: Option<RgbaImage>,
}

/// Image objects and their aggregate pixel accounting belong to one shared worker.
struct State {
    /// Live bound objects; each retains at most one decoded pixel buffer.
    images: ObjectStore<Image>,
    /// Sum of the pixel counts in `images`.
    pixels: u64,
}

impl State {
    /// Creates an empty, bounded worker state.
    fn new() -> Self {
        Self {
            images: ObjectStore::new(MAX_OBJECTS),
            pixels: 0,
        }
    }
}

thread_local! {
    /// WASI workers have isolated linear memories and never share raw pixel pointers.
    static STATE: RefCell<State> = RefCell::new(State::new());
}

bt_extension!(1 => open, 2 => decode, 3 => create, 10 => info, 11 => resize,
    12 => crop, 13 => rotate, 14 => watermark, 15 => watermark_bytes, 16 => text,
    17 => adjust, 18 => save, 19 => encode, 20 => pixel, 21 => close);
bt_extension_shutdown!(shutdown);
bt_extension_stats!(stats);

/// Releases every image when the worker shuts down.
fn shutdown() -> BtResult<BtValue> {
    STATE.with(|state| *state.borrow_mut() = State::new());
    Ok(BtValue::Bool(true))
}

/// Reports actual retained pixel memory without traversing image buffers.
fn stats() -> BtResult<BtValue> {
    STATE.with(|state| {
        let state = state.borrow();
        Ok(object(vec![
            ("active_images", BtValue::Int(state.images.len() as i64)),
            ("pixel_bytes", BtValue::Int((state.pixels * 4) as i64)),
        ]))
    })
}

/// Builds a small plain BT object, preserving field order.
fn object(fields: Vec<(&str, BtValue)>) -> BtValue {
    BtValue::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// Checks dimensions before a decoder or transform can allocate pixels.
fn dimensions(width: u32, height: u32) -> BtResult<u64> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > 16384 || height > 16384 || pixels > MAX_PIXELS {
        return Err("image dimensions must be 1..16384 and contain at most 16777216 pixels".into());
    }
    Ok(pixels)
}

/// Reads an integer constrained to a caller-specific inclusive range.
fn integer(args: &[BtValue], index: usize, name: &str, min: i64, max: i64) -> BtResult<i64> {
    let value = expect_int(args, index, name)?;
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be in {min}..{max}"));
    }
    Ok(value)
}

/// Reads a finite numeric option, accepting either BT numeric type.
fn number(value: &BtValue, name: &str, min: f64, max: f64) -> BtResult<f64> {
    let value = match value {
        BtValue::Int(v) => *v as f64,
        BtValue::Float(v) => *v,
        _ => return Err(format!("{name} must be a number")),
    };
    if !value.is_finite() || value < min || value > max {
        return Err(format!("{name} must be in {min}..{max}"));
    }
    Ok(value)
}

/// Validates a four-component straight-alpha sRGB color.
fn color(value: &BtValue) -> BtResult<Rgba<u8>> {
    let BtValue::Array(values) = value else {
        return Err("color must be [red, green, blue, alpha]".into());
    };
    if values.len() != 4 {
        return Err("color must contain exactly four integers".into());
    }
    let mut channels = [0u8; 4];
    for (index, channel) in channels.iter_mut().enumerate() {
        *channel = integer(values, index, "color channel", 0, 255)? as u8;
    }
    Ok(Rgba(channels))
}

/// Recognizes only codecs deliberately included in the package.
fn format(name: &str) -> BtResult<ImageFormat> {
    match name {
        "png" => Ok(ImageFormat::Png),
        "jpeg" | "jpg" => Ok(ImageFormat::Jpeg),
        "webp" => Ok(ImageFormat::WebP),
        "bmp" => Ok(ImageFormat::Bmp),
        _ => Err("unsupported image format; expected png, jpeg, webp or bmp".into()),
    }
}

/// Returns a borrowed encoded buffer without copying it again inside the worker.
fn bytes(value: &BtValue) -> BtResult<&[u8]> {
    let BtValue::Bytes(data) = value else {
        return Err("data must be Bytes".into());
    };
    if data.len() > MAX_BYTES {
        return Err("encoded Bytes exceed 16777152 bytes".into());
    }
    Ok(data)
}

/// Decodes a supported still image with dimensions and decoder allocation limits.
fn read_image(reader: impl std::io::BufRead + Seek) -> BtResult<RgbaImage> {
    let mut reader = ImageReader::new(reader)
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Bmp) => (),
        _ => return Err("unsupported image input; expected PNG, JPEG, WebP or BMP".into()),
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|e| format!("image decode failed: {e}"))?;
    let (width, height) = decoder.dimensions();
    dimensions(width, height)?;
    // Metadata is deliberately discarded; no ICC conversion or EXIF orientation is applied.
    DynamicImage::from_decoder(decoder)
        .map(|image| image.into_rgba8())
        .map_err(|e| format!("image decode failed: {e}"))
}

/// Opens a regular file and rejects encoded-file growth beyond the byte cap.
fn read_file(path: &str) -> BtResult<RgbaImage> {
    let file = File::open(path).map_err(|e| format!("image open failed: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err("image input must be a regular file of at most 64 MiB".into());
    }
    read_image(BufReader::new(BoundedRead {
        inner: file,
        position: 0,
    }))
}

/// A seekable file view capped even if another process grows the file after metadata inspection.
struct BoundedRead {
    /// File owned exclusively by this decoder call.
    inner: File,
    /// Current absolute stream position.
    position: u64,
}
impl Read for BoundedRead {
    /// Refuses reads beyond the encoded input budget.
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let available = MAX_FILE_BYTES.saturating_sub(self.position) as usize;
        let length = buffer.len().min(available);
        let read = self.inner.read(&mut buffer[..length])?;
        self.position += read as u64;
        Ok(read)
    }
}
impl Seek for BoundedRead {
    /// Records seeks and rejects positions outside the bounded file view.
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        let next = self.inner.seek(position)?;
        if next > MAX_FILE_BYTES {
            return Err(std::io::Error::other("image input exceeds 64 MiB"));
        }
        self.position = next;
        Ok(next)
    }
}

/// Checks aggregate retained pixels while allowing an existing buffer to be replaced.
fn replacement_budget(state: &State, id: u64, pixels: u64) -> BtResult<u64> {
    let old = state.images.get_required(id, "Image")?.pixels.as_ref();
    let old_pixels = old.map_or(0, |image| {
        u64::from(image.width()) * u64::from(image.height())
    });
    let total = state.pixels - old_pixels + pixels;
    if total > MAX_RETAINED_PIXELS {
        return Err("image worker pixel budget exceeded; close unused images".into());
    }
    Ok(total)
}

/// Commits an already decoded buffer without changing object identity or bound path.
fn replace_pixels(state: &mut State, id: u64, image: RgbaImage) -> BtResult<()> {
    let total = replacement_budget(state, id, dimensions(image.width(), image.height())?)?;
    state.images.get_mut_required(id, "Image")?.pixels = Some(image);
    state.pixels = total;
    Ok(())
}

/// Binds a host-normalized path without opening it; nonexistent image files are allowed.
fn open(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "image")?;
    let path = expect_string(&args, 0, "path")?;
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err("image path must contain 1..4096 UTF-8 bytes without NUL".into());
    }
    STATE.with(|state| {
        let id = state
            .borrow_mut()
            .images
            .insert(Image { path, pixels: None })?;
        Ok(BtValue::ExtObject(ExtObject::new(1, id, "Image")))
    })
}

/// Replaces this object's pixels from explicit ABI Bytes without reading its bound path.
fn decode(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = receiver(&args, 2, "Image.decode")?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.images.get_required(handle.object_id, "Image")?;
        let image = read_image(Cursor::new(bytes(&args[1])?))?;
        replace_pixels(&mut state, handle.object_id, image)?;
        Ok(BtValue::ExtObject(handle))
    })
}

/// Replaces this object's canvas after validation and budget checks, without reading or saving.
fn create(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = receiver(&args, 4, "Image.create")?;
    let width = integer(&args, 1, "width", 1, 16384)? as u32;
    let height = integer(&args, 2, "height", 1, 16384)? as u32;
    let pixels = dimensions(width, height)?;
    let color = color(&args[3])?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let total = replacement_budget(&state, handle.object_id, pixels)?;
        state
            .images
            .get_mut_required(handle.object_id, "Image")?
            .pixels = Some(RgbaImage::from_pixel(width, height, color));
        state.pixels = total;
        Ok(BtValue::ExtObject(handle))
    })
}

/// Loads pixels exactly once; a failed decode leaves the object uninitialized and retryable.
fn ensure_loaded(handle: &ExtObject) -> BtResult<()> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let image = state.images.get_required(handle.object_id, "Image")?;
        if image.pixels.is_none() {
            let decoded = read_file(&image.path)?;
            replace_pixels(&mut state, handle.object_id, decoded)?;
        }
        Ok(())
    })
}

/// Validates the receiver without accepting arbitrary object types.
fn receiver(args: &[BtValue], count: usize, label: &str) -> BtResult<ExtObject> {
    expect_arg_count(args, count, label)?;
    expect_ext_object_type(args, 0, "self", 1, "Image")
}

/// Validates a pixel-method receiver and loads its bound file only when needed.
fn loaded_receiver(args: &[BtValue], count: usize, label: &str) -> BtResult<ExtObject> {
    let handle = receiver(args, count, label)?;
    ensure_loaded(&handle)?;
    Ok(handle)
}

/// Returns image dimensions and the normalized in-memory representation.
fn info(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 1, "Image.info")?;
    STATE.with(|state| {
        let state = state.borrow();
        let image = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        Ok(object(vec![
            ("width", BtValue::Int(image.width() as i64)),
            ("height", BtValue::Int(image.height() as i64)),
            ("channels", BtValue::Int(4)),
            ("pixel_bytes", BtValue::Int(image.as_raw().len() as i64)),
            ("color_space", BtValue::String("srgb".into())),
            ("alpha", BtValue::String("straight".into())),
        ]))
    })
}

/// Atomically replaces a retained pixel buffer, accounting for dimensions first.
fn transform(
    handle: ExtObject,
    width: u32,
    height: u32,
    operation: impl FnOnce(&RgbaImage) -> RgbaImage,
) -> BtResult<BtValue> {
    let pixels = dimensions(width, height)?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let old = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        let old_pixels = u64::from(old.width()) * u64::from(old.height());
        if state.pixels - old_pixels + pixels > MAX_RETAINED_PIXELS {
            return Err("image worker pixel budget exceeded; close unused images".into());
        }
        let new = operation(old);
        *state
            .images
            .get_mut_required(handle.object_id, "Image")?
            .pixels
            .as_mut()
            .ok_or("image pixels are not loaded")? = new;
        state.pixels = state.pixels - old_pixels + pixels;
        Ok(BtValue::ExtObject(handle))
    })
}

/// Resizes to exact dimensions with alpha-correct filtering in encoded sRGB space.
fn resize(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 4, "Image.resize")?;
    let width = integer(&args, 1, "width", 1, 16384)? as u32;
    let height = integer(&args, 2, "height", 1, 16384)? as u32;
    let filter = match expect_string(&args, 3, "filter")?.as_str() {
        "nearest" => imageops::FilterType::Nearest,
        "triangle" => imageops::FilterType::Triangle,
        "catmull_rom" => imageops::FilterType::CatmullRom,
        "gaussian" => imageops::FilterType::Gaussian,
        "lanczos3" => imageops::FilterType::Lanczos3,
        _ => {
            return Err(
                "filter must be nearest, triangle, catmull_rom, gaussian or lanczos3".into(),
            )
        }
    };
    transform(handle, width, height, |source| {
        if filter == imageops::FilterType::Nearest {
            return imageops::resize(source, width, height, filter);
        }
        // Premultiplication prevents hidden RGB in transparent pixels creating colored fringes.
        let mut premultiplied = source.clone();
        for pixel in premultiplied.pixels_mut() {
            for channel in 0..3 {
                pixel[channel] =
                    ((u16::from(pixel[channel]) * u16::from(pixel[3]) + 127) / 255) as u8;
            }
        }
        let mut result = imageops::resize(&premultiplied, width, height, filter);
        for pixel in result.pixels_mut() {
            for channel in 0..3 {
                pixel[channel] = if pixel[3] == 0 {
                    0
                } else {
                    ((u32::from(pixel[channel]) * 255 + u32::from(pixel[3]) / 2)
                        / u32::from(pixel[3]))
                    .min(255) as u8
                };
            }
        }
        result
    })
}

/// Crops a fully contained integer rectangle without implicit padding or clipping.
fn crop(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 5, "Image.crop")?;
    let x = integer(&args, 1, "x", 0, 16384)? as u32;
    let y = integer(&args, 2, "y", 0, 16384)? as u32;
    let width = integer(&args, 3, "width", 1, 16384)? as u32;
    let height = integer(&args, 4, "height", 1, 16384)? as u32;
    STATE.with(|state| {
        let state = state.borrow();
        let image = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        if x + width > image.width() || y + height > image.height() {
            return Err("crop rectangle is outside image bounds".into());
        }
        Ok::<(), String>(())
    })?;
    transform(handle, width, height, |image| {
        imageops::crop_imm(image, x, y, width, height).to_image()
    })
}

/// Applies a lossless clockwise right-angle rotation.
fn rotate(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 2, "Image.rotate")?;
    let degrees = expect_int(&args, 1, "degrees")?;
    if ![90, 180, 270].contains(&degrees) {
        return Err("degrees must be 90, 180 or 270 clockwise".into());
    }
    let (width, height) = STATE.with(|state| {
        let state = state.borrow();
        state
            .images
            .get_required(handle.object_id, "Image")
            .and_then(|image| {
                image
                    .pixels
                    .as_ref()
                    .map(|pixels| pixels.dimensions())
                    .ok_or_else(|| "image pixels are not loaded".into())
            })
    })?;
    let (width, height) = if degrees == 180 {
        (width, height)
    } else {
        (height, width)
    };
    transform(handle, width, height, |image| match degrees {
        90 => imageops::rotate90(image),
        180 => imageops::rotate180(image),
        _ => imageops::rotate270(image),
    })
}

/// Composites one straight-alpha pixel using Porter-Duff source-over in sRGB.
fn blend(target: &mut Rgba<u8>, source: Rgba<u8>, opacity: f64) {
    let source_alpha = f64::from(source[3]) / 255.0 * opacity;
    let target_alpha = f64::from(target[3]) / 255.0;
    let alpha = source_alpha + target_alpha * (1.0 - source_alpha);
    if alpha == 0.0 {
        return;
    }
    for channel in 0..3 {
        target[channel] = ((f64::from(source[channel]) * source_alpha
            + f64::from(target[channel]) * target_alpha * (1.0 - source_alpha))
            / alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    target[3] = (alpha * 255.0).round() as u8;
}

/// Blends a separately decoded watermark, clipping its edges to the target.
fn overlay(args: &[BtValue], handle: ExtObject, mark: RgbaImage) -> BtResult<BtValue> {
    let x = integer(args, 2, "x", -16384, 16384)?;
    let y = integer(args, 3, "y", -16384, 16384)?;
    let opacity = number(&args[4], "opacity", 0.0, 1.0)?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let target = state
            .images
            .get_mut_required(handle.object_id, "Image")?
            .pixels
            .as_mut()
            .ok_or("image pixels are not loaded")?;
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + i64::from(mark.width())).min(i64::from(target.width()));
        let bottom = (y + i64::from(mark.height())).min(i64::from(target.height()));
        for ty in top..bottom {
            for tx in left..right {
                blend(
                    target.get_pixel_mut(tx as u32, ty as u32),
                    *mark.get_pixel((tx - x) as u32, (ty - y) as u32),
                    opacity,
                );
            }
        }
        Ok(BtValue::ExtObject(handle))
    })
}

/// Loads and composites a watermark path without retaining a second handle.
fn watermark(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 5, "Image.watermark")?;
    let mark = read_file(&expect_string(&args, 1, "path")?)?;
    overlay(&args, handle, mark)
}

/// Composites watermark Bytes without any cross-extension object dependency.
fn watermark_bytes(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 5, "Image.watermark_bytes")?;
    let mark = read_image(Cursor::new(bytes(&args[1])?))?;
    overlay(&args, handle, mark)
}

/// Draws a bounded ASCII text watermark with the built-in 8x8 bitmap font.
fn text(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 6, "Image.text")?;
    let text = expect_string(&args, 1, "text")?;
    if text.len() > 1024 || !text.bytes().all(|c| (32..=126).contains(&c) || c == b'\n') {
        return Err(
            "text supports at most 1024 ASCII bytes (printable characters and newline)".into(),
        );
    }
    let x = integer(&args, 2, "x", -16384, 16384)?;
    let y = integer(&args, 3, "y", -16384, 16384)?;
    let scale = integer(&args, 4, "scale", 1, 16)?;
    let color = color(&args[5])?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let image = state
            .images
            .get_mut_required(handle.object_id, "Image")?
            .pixels
            .as_mut()
            .ok_or("image pixels are not loaded")?;
        let (mut tx, mut ty) = (x, y);
        for character in text.chars() {
            if character == '\n' {
                tx = x;
                ty += 8 * scale;
                continue;
            }
            if tx < i64::from(image.width())
                && tx + 8 * scale > 0
                && ty < i64::from(image.height())
                && ty + 8 * scale > 0
            {
                let glyph = font8x8::BASIC_FONTS
                    .get(character)
                    .ok_or("unsupported text character")?;
                for (row, bits) in glyph.iter().enumerate() {
                    for column in 0..8 {
                        if bits & (1 << column) == 0 {
                            continue;
                        }
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let px = tx + column * scale + dx;
                                let py = ty + row as i64 * scale + dy;
                                if px >= 0
                                    && py >= 0
                                    && px < i64::from(image.width())
                                    && py < i64::from(image.height())
                                {
                                    blend(image.get_pixel_mut(px as u32, py as u32), color, 1.0);
                                }
                            }
                        }
                    }
                }
            }
            tx += 8 * scale;
        }
        Ok(BtValue::ExtObject(handle))
    })
}

/// Validates plain option objects, rejecting unknown and duplicate fields.
fn options<'a>(value: &'a BtValue, allowed: &[&str]) -> BtResult<&'a [(String, BtValue)]> {
    let BtValue::Object(fields) = value else {
        return Err("options must be an object".into());
    };
    for (index, (key, _)) in fields.iter().enumerate() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown image option: {key}"));
        }
        if fields[..index].iter().any(|(previous, _)| previous == key) {
            return Err(format!("duplicate image option: {key}"));
        }
    }
    Ok(fields)
}

/// Applies brightness, contrast, saturation, gamma, grayscale and inversion in one pixel pass.
fn adjust(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 2, "Image.adjust")?;
    let fields = options(
        &args[1],
        &[
            "brightness",
            "contrast",
            "saturation",
            "gamma",
            "grayscale",
            "invert",
        ],
    )?;
    let (mut brightness, mut contrast, mut saturation, mut gamma) = (0.0, 1.0, 1.0, 1.0);
    let (mut grayscale, mut invert) = (false, false);
    for (key, value) in fields {
        match key.as_str() {
            "brightness" => brightness = number(value, key, -255.0, 255.0)?,
            "contrast" => contrast = number(value, key, 0.0, 4.0)?,
            "saturation" => saturation = number(value, key, 0.0, 4.0)?,
            "gamma" => gamma = number(value, key, 0.1, 10.0)?,
            "grayscale" | "invert" => {
                let BtValue::Bool(flag) = value else {
                    return Err(format!("{key} must be bool"));
                };
                if key == "grayscale" {
                    grayscale = *flag;
                } else {
                    invert = *flag;
                }
            }
            _ => unreachable!(),
        }
    }
    // Gamma/contrast/brightness use a fixed lookup table instead of per-pixel exponentiation.
    let mut lookup = [0.0f64; 256];
    for (index, output) in lookup.iter_mut().enumerate() {
        *output = (((index as f64 - 127.5) * contrast + 127.5 + brightness).clamp(0.0, 255.0)
            / 255.0)
            .powf(1.0 / gamma)
            * 255.0;
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let image = state
            .images
            .get_mut_required(handle.object_id, "Image")?
            .pixels
            .as_mut()
            .ok_or("image pixels are not loaded")?;
        for pixel in image.pixels_mut() {
            let rgb = [
                lookup[pixel[0] as usize],
                lookup[pixel[1] as usize],
                lookup[pixel[2] as usize],
            ];
            let luma = rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
            for channel in 0..3 {
                let value = if grayscale {
                    luma
                } else {
                    luma + (rgb[channel] - luma) * saturation
                };
                pixel[channel] = (if invert { 255.0 - value } else { value })
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
        Ok(BtValue::ExtObject(handle))
    })
}

/// Validated encoder configuration, independent from the image object's pixels.
struct EncodeOptions {
    /// JPEG quality, inclusive 1..100; never claims lossless JPEG compression.
    quality: u8,
    /// PNG effort; this affects size and speed but never changes pixels.
    compression: image::codecs::png::CompressionType,
    /// Opaque RGB matte for JPEG's absent alpha channel.
    background: Rgba<u8>,
}
impl EncodeOptions {
    /// Rejects options that the chosen codec cannot honor.
    fn parse(value: &BtValue, format: ImageFormat) -> BtResult<Self> {
        let fields = options(value, &["quality", "compression", "background"])?;
        let mut result = Self {
            quality: 85,
            compression: image::codecs::png::CompressionType::Default,
            background: Rgba([255; 4]),
        };
        for (key, value) in fields {
            match key.as_str() {
                "quality" if format == ImageFormat::Jpeg => {
                    let BtValue::Int(quality) = value else {
                        return Err("quality must be an integer in 1..100".into());
                    };
                    if !(1..=100).contains(quality) {
                        return Err("quality must be an integer in 1..100".into());
                    }
                    result.quality = *quality as u8;
                }
                "compression" if format == ImageFormat::Png => {
                    result.compression = match value.as_str() {
                        Some("fast") => image::codecs::png::CompressionType::Fast,
                        Some("default") => image::codecs::png::CompressionType::Default,
                        Some("best") => image::codecs::png::CompressionType::Best,
                        _ => return Err("compression must be fast, default or best".into()),
                    };
                }
                "background" if format == ImageFormat::Jpeg => {
                    result.background = color(value)?;
                    if result.background[3] != 255 {
                        return Err("JPEG background alpha must be 255".into());
                    }
                }
                _ => return Err(format!("{key} is not supported for this output format")),
            }
        }
        Ok(result)
    }
}

/// A writer budget prevents encoding from allocating or emitting unbounded output.
struct BoundedWrite<W> {
    /// Caller-owned stream or Bytes accumulator.
    inner: W,
    /// Remaining encoded bytes before this writer returns an error.
    remaining: usize,
}
impl<W: Write> Write for BoundedWrite<W> {
    /// Reserves the full requested write before passing data to the inner writer.
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.remaining {
            return Err(std::io::Error::other(
                "encoded image exceeds output byte limit",
            ));
        }
        let count = self.inner.write(data)?;
        self.remaining -= count;
        Ok(count)
    }
    /// Flushes the underlying output.
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Encodes directly to a bounded stream; only JPEG requires an RGB matte buffer.
fn write_image(
    image: &RgbaImage,
    format: ImageFormat,
    options: &EncodeOptions,
    writer: impl Write,
) -> BtResult<()> {
    let (width, height) = image.dimensions();
    let result =
        match format {
            ImageFormat::Png => image::codecs::png::PngEncoder::new_with_quality(
                writer,
                options.compression,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            ),
            ImageFormat::WebP => image::codecs::webp::WebPEncoder::new_lossless(writer)
                .write_image(
                    image.as_raw(),
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                ),
            ImageFormat::Bmp => image::codecs::bmp::BmpEncoder::new(&mut { writer }).write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            ),
            ImageFormat::Jpeg => {
                let mut rgb = RgbImage::new(width, height);
                for (source, target) in image.pixels().zip(rgb.pixels_mut()) {
                    for channel in 0..3 {
                        target[channel] = ((u32::from(source[channel]) * u32::from(source[3])
                            + u32::from(options.background[channel])
                                * (255 - u32::from(source[3]))
                            + 127)
                            / 255) as u8;
                    }
                }
                image::codecs::jpeg::JpegEncoder::new_with_quality(writer, options.quality)
                    .write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            }
            _ => return Err("unsupported output format".into()),
        };
    result.map_err(|e| format!("image encode failed: {e}"))
}

/// Writes an image through a same-directory temporary file then atomically replaces the destination.
fn save(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 4, "Image.save")?;
    let path = expect_string(&args, 1, "path")?;
    let format = format(&expect_string(&args, 2, "format")?)?;
    let options = EncodeOptions::parse(&args[3], format)?;
    let path = Path::new(&path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid output filename")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = None;
    for _ in 0..OUTPUT_TEMP_SLOTS {
        let candidate = parent.join(format!(
            ".{name}.bt-image-{}.tmp",
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed) % OUTPUT_TEMP_SLOTS
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("image output failed: {error}")),
        }
    }
    let (temporary_path, file) = temporary.ok_or("cannot reserve image output temporary file")?;
    let result = STATE.with(|state| {
        let state = state.borrow();
        let image = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        let mut writer = BoundedWrite {
            inner: BufWriter::new(file),
            remaining: MAX_OUTPUT_BYTES,
        };
        write_image(image, format, &options, &mut writer)?;
        writer.flush().map_err(|e| e.to_string())?;
        drop(writer);
        fs::rename(&temporary_path, path).map_err(|e| format!("image output replace failed: {e}"))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result?;
    Ok(BtValue::ExtObject(handle))
}

/// Encodes explicitly requested Bytes while preserving the source pixels and handle.
fn encode(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 3, "Image.encode")?;
    let format = format(&expect_string(&args, 1, "format")?)?;
    let options = EncodeOptions::parse(&args[2], format)?;
    STATE.with(|state| {
        let state = state.borrow();
        let image = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        let mut writer = BoundedWrite {
            inner: Vec::new(),
            remaining: MAX_BYTES,
        };
        write_image(image, format, &options, &mut writer)?;
        Ok(BtValue::Bytes(writer.inner))
    })
}

/// Returns one RGBA pixel for inspection, or `empty` for an out-of-bounds position.
fn pixel(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = loaded_receiver(&args, 3, "Image.pixel")?;
    let x = expect_int(&args, 1, "x")?;
    let y = expect_int(&args, 2, "y")?;
    STATE.with(|state| {
        let state = state.borrow();
        let image = state
            .images
            .get_required(handle.object_id, "Image")?
            .pixels
            .as_ref()
            .ok_or("image pixels are not loaded")?;
        if x < 0 || y < 0 || x >= i64::from(image.width()) || y >= i64::from(image.height()) {
            return Ok(BtValue::Empty);
        }
        Ok(BtValue::Array(
            image
                .get_pixel(x as u32, y as u32)
                .0
                .iter()
                .map(|v| BtValue::Int(i64::from(*v)))
                .collect(),
        ))
    })
}

/// Releases the retained pixel buffer immediately and invalidates the public handle.
fn close(args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = receiver(&args, 1, "Image.close")?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let image = state.images.remove_required(handle.object_id, "Image")?;
        state.pixels -= image.pixels.as_ref().map_or(0, |pixels| {
            u64::from(pixels.width()) * u64::from(pixels.height())
        });
        Ok(BtValue::Bool(true))
    })
}

#[cfg(test)]
mod tests;
