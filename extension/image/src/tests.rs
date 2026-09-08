//! Tests exercise public handlers with synthetic, redistributable fixtures.

use super::*;

/// Creates a BT integer for compact API calls.
fn int(value: i64) -> BtValue {
    BtValue::Int(value)
}
/// Creates a BT string for compact API calls.
fn string(value: &str) -> BtValue {
    BtValue::String(value.into())
}
/// Creates an RGBA parameter.
fn rgba(r: i64, g: i64, b: i64, a: i64) -> BtValue {
    BtValue::Array(vec![int(r), int(g), int(b), int(a)])
}
/// Creates an empty option object.
fn empty_options() -> BtValue {
    BtValue::Object(Vec::new())
}
/// Creates a distinct bound canvas for a test, releasing it on failure.
fn new_canvas(mut args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = open(vec![string("unused.png")])?;
    args.insert(0, handle.clone());
    match create(args) {
        Ok(value) => Ok(value),
        Err(error) => {
            close(vec![handle])?;
            Err(error)
        }
    }
}

/// Creates a distinct bound object for a decode test, releasing it on failure.
fn decoded_canvas(mut args: Vec<BtValue>) -> BtResult<BtValue> {
    let handle = open(vec![string("decoded.png")])?;
    args.insert(0, handle.clone());
    match decode(args) {
        Ok(value) => Ok(value),
        Err(error) => {
            close(vec![handle])?;
            Err(error)
        }
    }
}

/// Retains synthetic test pixels using the same replacement accounting as public methods.
fn retain(image: RgbaImage) -> BtResult<BtValue> {
    let handle = open(vec![string("synthetic.png")])?;
    let BtValue::ExtObject(id) = &handle else {
        unreachable!()
    };
    STATE.with(|state| replace_pixels(&mut state.borrow_mut(), id.object_id, image))?;
    Ok(handle)
}

/// Resets this test thread's object store and creates a solid fixture.
fn fixture(width: i64, height: i64, color: BtValue) -> BtValue {
    shutdown().unwrap();
    new_canvas(vec![int(width), int(height), color]).unwrap()
}
/// Reads the private retained image through a closure without cloning its pixels.
fn inspect<T>(handle: &BtValue, check: impl FnOnce(&RgbaImage) -> T) -> T {
    let BtValue::ExtObject(handle) = handle else {
        panic!("not an image")
    };
    ensure_loaded(handle).unwrap();
    STATE.with(|state| {
        check(
            state
                .borrow()
                .images
                .get(handle.object_id)
                .unwrap()
                .pixels
                .as_ref()
                .unwrap(),
        )
    })
}

/// PNG, WebP and BMP round trips preserve every RGBA channel, including transparency.
#[test]
fn lossless_roundtrip_formats() {
    let handle = fixture(19, 11, rgba(203, 45, 12, 97));
    for format in ["png", "webp", "bmp"] {
        let encoded = encode(vec![handle.clone(), string(format), empty_options()]).unwrap();
        let decoded = decoded_canvas(vec![encoded]).unwrap();
        inspect(&decoded, |image| {
            assert_eq!(image.dimensions(), (19, 11));
            assert_eq!(image.get_pixel(3, 5).0, [203, 45, 12, 97]);
        });
        close(vec![decoded]).unwrap();
    }
    close(vec![handle]).unwrap();
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
}

/// JPEG conversion composites transparent pixels on the requested opaque matte.
#[test]
fn jpeg_quality_and_matte() {
    let handle = fixture(32, 32, rgba(255, 0, 0, 0));
    let opts = object(vec![
        ("quality", int(100)),
        ("background", rgba(0, 0, 255, 255)),
    ]);
    let encoded = encode(vec![handle.clone(), string("jpeg"), opts]).unwrap();
    let decoded = decoded_canvas(vec![encoded]).unwrap();
    inspect(&decoded, |image| {
        let pixel = image.get_pixel(0, 0);
        assert!(pixel[0] <= 2 && pixel[1] <= 2 && pixel[2] >= 253 && pixel[3] == 255);
    });
    assert!(encode(vec![
        handle.clone(),
        string("jpeg"),
        object(vec![("quality", int(0))])
    ])
    .is_err());
    assert!(encode(vec![
        handle,
        string("webp"),
        object(vec![("quality", int(80))])
    ])
    .is_err());
}

/// Crop coordinates and clockwise rotation map asymmetric pixels exactly.
#[test]
fn geometry_and_invalid_arguments() {
    let handle = fixture(4, 3, rgba(10, 20, 30, 255));
    let mark = new_canvas(vec![int(1), int(1), rgba(255, 0, 0, 255)]).unwrap();
    let data = encode(vec![mark.clone(), string("png"), empty_options()]).unwrap();
    watermark_bytes(vec![handle.clone(), data, int(1), int(1), int(1)]).unwrap();
    let result = crop(vec![handle.clone(), int(1), int(0), int(2), int(3)]).unwrap();
    assert_eq!(result, handle);
    rotate(vec![handle.clone(), int(90)]).unwrap();
    inspect(&handle, |image| {
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.get_pixel(1, 0).0, [255, 0, 0, 255]);
    });
    assert!(crop(vec![handle.clone(), int(2), int(0), int(2), int(1)]).is_err());
    assert!(rotate(vec![handle.clone(), int(45)]).is_err());
    assert!(resize(vec![handle.clone(), int(0), int(5), string("nearest")]).is_err());
    assert!(resize(vec![handle.clone(), int(3), int(5), string("bad")]).is_err());
    assert_eq!(
        pixel(vec![handle.clone(), int(-1), int(0)]).unwrap(),
        BtValue::Empty
    );
    for filter in ["nearest", "triangle", "catmull_rom", "gaussian", "lanczos3"] {
        resize(vec![handle.clone(), int(7), int(5), string(filter)]).unwrap();
        inspect(&handle, |image| assert_eq!(image.dimensions(), (7, 5)));
    }
    rotate(vec![handle.clone(), int(180)]).unwrap();
    rotate(vec![handle, int(270)]).unwrap();
}

/// Filtering transparent hidden blue next to opaque red never creates a blue fringe.
#[test]
fn resize_premultiplies_alpha() {
    let handle = fixture(2, 1, rgba(0, 0, 255, 0));
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let BtValue::ExtObject(handle) = &handle else {
            unreachable!()
        };
        state
            .images
            .get_mut(handle.object_id)
            .unwrap()
            .pixels
            .as_mut()
            .unwrap()
            .put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    });
    resize(vec![handle.clone(), int(7), int(1), string("triangle")]).unwrap();
    inspect(&handle, |image| {
        for p in image.pixels() {
            assert_eq!(p[2], 0);
            if p[3] != 0 {
                assert!(p[0] >= 254);
            }
        }
    });
}

/// Watermarks apply opacity and clip negative coordinates using straight-alpha composition.
#[test]
fn watermark_alpha_and_clipping() {
    let handle = fixture(5, 5, rgba(0, 0, 255, 255));
    let mark = new_canvas(vec![int(2), int(2), rgba(255, 0, 0, 255)]).unwrap();
    let data = encode(vec![mark, string("png"), empty_options()]).unwrap();
    watermark_bytes(vec![
        handle.clone(),
        data.clone(),
        int(-1),
        int(-1),
        BtValue::Float(0.5),
    ])
    .unwrap();
    inspect(&handle, |image| {
        assert_eq!(image.get_pixel(0, 0).0, [128, 0, 128, 255]);
        assert_eq!(image.get_pixel(1, 1).0, [0, 0, 255, 255]);
    });
    assert!(watermark_bytes(vec![handle, data, int(0), int(0), BtValue::Float(1.1)]).is_err());
}

/// ASCII text draws pixels while unsupported characters fail before modifying the image.
#[test]
fn text_and_validation() {
    let handle = fixture(80, 40, rgba(0, 0, 0, 0));
    assert!(text(vec![
        handle.clone(),
        string("中文"),
        int(0),
        int(0),
        int(1),
        rgba(255, 0, 0, 255)
    ])
    .is_err());
    inspect(&handle, |image| assert!(image.pixels().all(|p| p[3] == 0)));
    text(vec![
        handle.clone(),
        string("BT\n1.0"),
        int(0),
        int(0),
        int(2),
        rgba(255, 255, 255, 128),
    ])
    .unwrap();
    inspect(&handle, |image| {
        assert!(image.pixels().any(|p| p[3] == 128))
    });
    assert!(text(vec![
        handle,
        string("BT"),
        int(0),
        int(0),
        int(17),
        rgba(1, 2, 3, 4)
    ])
    .is_err());
}

/// Each color operation changes RGB as specified without altering alpha.
#[test]
fn adjustments_and_option_validation() {
    let handle = fixture(3, 3, rgba(30, 60, 90, 123));
    adjust(vec![handle.clone(), object(vec![("brightness", int(10))])]).unwrap();
    assert_eq!(
        pixel(vec![handle.clone(), int(0), int(0)]).unwrap(),
        rgba(40, 70, 100, 123)
    );
    adjust(vec![
        handle.clone(),
        object(vec![("invert", BtValue::Bool(true))]),
    ])
    .unwrap();
    assert_eq!(
        pixel(vec![handle.clone(), int(0), int(0)]).unwrap(),
        rgba(215, 185, 155, 123)
    );
    adjust(vec![
        handle.clone(),
        object(vec![
            ("grayscale", BtValue::Bool(true)),
            ("gamma", BtValue::Float(2.0)),
            ("contrast", BtValue::Float(1.2)),
            ("saturation", BtValue::Float(0.5)),
        ]),
    ])
    .unwrap();
    inspect(&handle, |image| {
        let p = image.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        assert_eq!(p[3], 123);
    });
    assert!(adjust(vec![handle.clone(), object(vec![("gamam", int(1))])]).is_err());
    assert!(adjust(vec![handle.clone(), object(vec![("gamma", int(0))])]).is_err());
    assert!(adjust(vec![handle.clone(), object(vec![("invert", int(1))])]).is_err());
    assert!(adjust(vec![
        handle,
        object(vec![("gamma", int(1)), ("gamma", int(2))])
    ])
    .is_err());
}

/// File outputs are valid and atomically replace old contents; failed output leaves no staging files.
#[test]
fn file_io_and_compression() {
    let handle = fixture(40, 30, rgba(12, 34, 56, 78));
    let folder = std::env::temp_dir().join(format!(
        "bt-image-test-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&folder).unwrap();
    let path = folder.join("fixture.png");
    for compression in ["fast", "default", "best"] {
        save(vec![
            handle.clone(),
            string(path.to_str().unwrap()),
            string("png"),
            object(vec![("compression", string(compression))]),
        ])
        .unwrap();
        let decoded = open(vec![string(path.to_str().unwrap())]).unwrap();
        inspect(&decoded, |image| {
            assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56, 78])
        });
        close(vec![decoded]).unwrap();
    }
    watermark(vec![
        handle.clone(),
        string(path.to_str().unwrap()),
        int(0),
        int(0),
        int(1),
    ])
    .unwrap();
    let original = fs::read(&path).unwrap();
    for slot in 0..OUTPUT_TEMP_SLOTS {
        fs::write(
            folder.join(format!(".fixture.png.bt-image-{slot}.tmp")),
            b"owned",
        )
        .unwrap();
    }
    assert!(save(vec![
        handle.clone(),
        string(path.to_str().unwrap()),
        string("png"),
        empty_options()
    ])
    .unwrap_err()
    .contains("temporary file"));
    assert_eq!(fs::read(&path).unwrap(), original);
    for slot in 0..OUTPUT_TEMP_SLOTS {
        let staging = folder.join(format!(".fixture.png.bt-image-{slot}.tmp"));
        assert_eq!(fs::read(&staging).unwrap(), b"owned");
        fs::remove_file(staging).unwrap();
    }
    let bad_destination = folder.join("directory");
    fs::create_dir(&bad_destination).unwrap();
    assert!(save(vec![
        handle,
        string(bad_destination.to_str().unwrap()),
        string("png"),
        empty_options()
    ])
    .is_err());
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 2);
    fs::remove_file(path).unwrap();
    fs::remove_dir(bad_destination).unwrap();
    fs::remove_dir(folder).unwrap();
}

/// Malformed input, unsupported codecs, allocation budgets and stale handles have explicit errors.
#[test]
fn failures_and_release() {
    let handle = fixture(1, 1, rgba(1, 2, 3, 4));
    assert!(decoded_canvas(vec![BtValue::Bytes(vec![0, 1, 2])]).is_err());
    assert!(encode(vec![handle.clone(), string("tiff"), empty_options()]).is_err());
    assert!(new_canvas(vec![int(16384), int(16384), rgba(0, 0, 0, 0)]).is_err());
    for _ in 1..MAX_OBJECTS {
        new_canvas(vec![int(1), int(1), rgba(0, 0, 0, 0)]).unwrap();
    }
    assert!(new_canvas(vec![int(1), int(1), rgba(0, 0, 0, 0)]).is_err());
    close(vec![handle.clone()]).unwrap();
    assert!(info(vec![handle.clone()]).is_err());
    assert!(close(vec![handle]).is_err());
    shutdown().unwrap();
    let mut writer = BoundedWrite {
        inner: Vec::new(),
        remaining: 2,
    };
    assert!(writer.write_all(&[1, 2, 3]).is_err());
    assert!(writer.inner.is_empty());
    for _ in 0..128 {
        let image = new_canvas(vec![int(128), int(128), rgba(1, 2, 3, 4)]).unwrap();
        close(vec![image]).unwrap();
    }
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
}

/// Header-declared decompression bombs are rejected before pixel allocation.
#[test]
fn oversized_header_is_rejected() {
    let mut encoded = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut encoded)
        .write_image(&[0, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    encoded[18..22].copy_from_slice(&16384u32.to_le_bytes());
    encoded[22..26].copy_from_slice(&16384u32.to_le_bytes());
    assert!(decoded_canvas(vec![BtValue::Bytes(encoded)]).is_err());
}

/// Aggregate accounting refuses one extra pixel at the exact worker limit and recovers after close.
#[test]
fn retained_pixel_budget_recovers() {
    let first = fixture(4096, 4096, rgba(0, 0, 0, 0));
    let second = new_canvas(vec![int(4096), int(4096), rgba(0, 0, 0, 0)]).unwrap();
    assert!(new_canvas(vec![int(1), int(1), rgba(0, 0, 0, 0)])
        .unwrap_err()
        .contains("pixel budget"));
    close(vec![first]).unwrap();
    let recovered = new_canvas(vec![int(1), int(1), rgba(0, 0, 0, 0)]).unwrap();
    close(vec![recovered]).unwrap();
    close(vec![second]).unwrap();
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
}

/// Large BMP byte encoding fails within the ABI cap and leaves the live source intact.
#[test]
fn encoded_byte_budget_preserves_source() {
    let handle = fixture(2048, 2048, rgba(1, 2, 3, 4));
    assert!(encode(vec![handle.clone(), string("bmp"), empty_options()])
        .unwrap_err()
        .contains("output byte limit"));
    inspect(&handle, |image| {
        assert_eq!(image.dimensions(), (2048, 2048));
        assert_eq!(image.get_pixel(0, 0).0, [1, 2, 3, 4]);
    });
    close(vec![handle]).unwrap();
}

/// JPEG quality and PNG effort are wired to the codecs and affect nontrivial synthetic output size.
#[test]
fn compression_parameters_are_effective() {
    shutdown().unwrap();
    let handle = retain(RgbaImage::from_fn(128, 128, |x, y| {
        Rgba([
            (x * 71 % 256) as u8,
            (y * 41 % 256) as u8,
            ((x ^ y) * 13 % 256) as u8,
            255,
        ])
    }))
    .unwrap();
    let low = encode(vec![
        handle.clone(),
        string("jpeg"),
        object(vec![("quality", int(10))]),
    ])
    .unwrap();
    let high = encode(vec![
        handle.clone(),
        string("jpeg"),
        object(vec![("quality", int(95))]),
    ])
    .unwrap();
    assert!(bytes(&low).unwrap().len() < bytes(&high).unwrap().len());
    let fast = encode(vec![
        handle.clone(),
        string("png"),
        object(vec![("compression", string("fast"))]),
    ])
    .unwrap();
    let best = encode(vec![
        handle.clone(),
        string("png"),
        object(vec![("compression", string("best"))]),
    ])
    .unwrap();
    assert!(bytes(&best).unwrap().len() <= bytes(&fast).unwrap().len());
    close(vec![handle]).unwrap();
}

/// Reproducible release-mode timing includes a full 1920x1080 decode/transform/encode pipeline.
#[test]
#[ignore = "run explicitly with --release --ignored --nocapture for performance measurements"]
fn representative_pipeline() {
    shutdown().unwrap();
    let source = RgbaImage::from_fn(1920, 1080, |x, y| {
        Rgba([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8, 255])
    });
    let handle = retain(source).unwrap();
    let data = encode(vec![handle.clone(), string("png"), empty_options()]).unwrap();
    close(vec![handle]).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..10 {
        let handle = decoded_canvas(vec![data.clone()]).unwrap();
        resize(vec![handle.clone(), int(960), int(540), string("triangle")]).unwrap();
        adjust(vec![
            handle.clone(),
            object(vec![
                ("brightness", int(8)),
                ("saturation", BtValue::Float(1.1)),
            ]),
        ])
        .unwrap();
        text(vec![
            handle.clone(),
            string("BT image"),
            int(8),
            int(8),
            int(2),
            rgba(255, 255, 255, 200),
        ])
        .unwrap();
        encode(vec![
            handle.clone(),
            string("jpeg"),
            object(vec![("quality", int(85))]),
        ])
        .unwrap();
        close(vec![handle]).unwrap();
    }
    println!("1920x1080 PNG decode -> 960x540 triangle resize -> adjust/text -> JPEG85; iterations=10; elapsed_ms={}; stats={:?}", start.elapsed().as_millis(), stats().unwrap());
}

/// Path binding defers I/O, and create bypasses absent or malformed disk contents.
#[test]
fn bound_paths_are_lazy_and_create_never_writes() {
    shutdown().unwrap();
    let folder = std::env::temp_dir().join(format!(
        "bt-image-lazy-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&folder).unwrap();
    let missing = folder.join("missing.png");
    let malformed = folder.join("malformed.png");
    fs::write(&malformed, b"not an image").unwrap();
    for path in [&missing, &malformed] {
        let handle = open(vec![string(path.to_str().unwrap())]).unwrap();
        assert_eq!(STATE.with(|state| state.borrow().pixels), 0);
        assert!(info(vec![handle.clone()]).is_err());
        assert_eq!(STATE.with(|state| state.borrow().pixels), 0);
        assert_eq!(
            create(vec![handle.clone(), int(7), int(3), rgba(10, 20, 30, 40)]).unwrap(),
            handle
        );
        assert_eq!(
            pixel(vec![handle.clone(), int(0), int(0)]).unwrap(),
            rgba(10, 20, 30, 40)
        );
        close(vec![handle]).unwrap();
    }
    assert!(!missing.exists());
    assert_eq!(fs::read(&malformed).unwrap(), b"not an image");
    let unloaded = open(vec![string(missing.to_str().unwrap())]).unwrap();
    close(vec![unloaded]).unwrap();
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
    fs::remove_file(malformed).unwrap();
    fs::remove_dir(folder).unwrap();
}

/// Loading retries after failure, then reuses decoded pixels even if the source is removed.
#[test]
fn lazy_decode_retries_and_then_reuses_pixels() {
    shutdown().unwrap();
    let path = std::env::temp_dir().join(format!(
        "bt-image-retry-{}-{}.png",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let handle = open(vec![string(path.to_str().unwrap())]).unwrap();
    assert!(pixel(vec![handle.clone(), int(0), int(0)]).is_err());
    let source = new_canvas(vec![int(2), int(3), rgba(22, 33, 44, 55)]).unwrap();
    save(vec![
        source.clone(),
        string(path.to_str().unwrap()),
        string("png"),
        empty_options(),
    ])
    .unwrap();
    close(vec![source]).unwrap();
    assert_eq!(
        pixel(vec![handle.clone(), int(1), int(2)]).unwrap(),
        rgba(22, 33, 44, 55)
    );
    fs::remove_file(&path).unwrap();
    inspect(&handle, |image| assert_eq!(image.height(), 3));
    resize(vec![handle.clone(), int(4), int(6), string("nearest")]).unwrap();
    close(vec![handle]).unwrap();
    assert_eq!(STATE.with(|state| state.borrow().pixels), 0);
}

/// Decode/create replace the same object atomically, release prior pixels and preserve its path.
#[test]
fn replacement_identity_atomicity_and_accounting() {
    let handle = fixture(1, 1, rgba(11, 22, 33, 44));
    let source = new_canvas(vec![int(8192), int(1), rgba(66, 77, 88, 99)]).unwrap();
    let data = encode(vec![source.clone(), string("png"), empty_options()]).unwrap();
    close(vec![source]).unwrap();
    let large = new_canvas(vec![int(4096), int(4096), rgba(0, 0, 0, 0)]).unwrap();
    let almost_large = new_canvas(vec![int(4096), int(4095), rgba(0, 0, 0, 0)]).unwrap();
    let original_total = STATE.with(|state| state.borrow().pixels);
    assert!(
        create(vec![handle.clone(), int(8192), int(1), rgba(1, 2, 3, 4)])
            .unwrap_err()
            .contains("pixel budget")
    );
    assert!(decode(vec![handle.clone(), data.clone()])
        .unwrap_err()
        .contains("pixel budget"));
    assert!(decode(vec![handle.clone(), BtValue::Bytes(vec![0, 1, 2])]).is_err());
    assert!(create(vec![handle.clone(), int(1), int(1), rgba(256, 0, 0, 0)]).is_err());
    assert_eq!(STATE.with(|state| state.borrow().pixels), original_total);
    assert_eq!(
        pixel(vec![handle.clone(), int(0), int(0)]).unwrap(),
        rgba(11, 22, 33, 44)
    );
    close(vec![large]).unwrap();
    close(vec![almost_large]).unwrap();
    for _ in 0..128 {
        assert_eq!(decode(vec![handle.clone(), data.clone()]).unwrap(), handle);
        assert_eq!(STATE.with(|state| state.borrow().pixels), 8192);
        assert_eq!(
            create(vec![handle.clone(), int(1), int(1), rgba(11, 22, 33, 44)]).unwrap(),
            handle
        );
        assert_eq!(STATE.with(|state| state.borrow().pixels), 1);
    }
    let BtValue::ExtObject(id) = &handle else {
        unreachable!()
    };
    STATE.with(|state| {
        assert_eq!(
            state.borrow().images.get(id.object_id).unwrap().path,
            "unused.png"
        )
    });
    close(vec![handle.clone()]).unwrap();
    assert!(create(vec![handle.clone(), int(1), int(1), rgba(0, 0, 0, 0)]).is_err());
    assert!(decode(vec![handle, data]).is_err());
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
}

/// Unloaded objects retain bounded path metadata and consume handle slots without pixel memory.
#[test]
fn unloaded_metadata_is_bounded() {
    shutdown().unwrap();
    assert!(open(vec![string("")]).is_err());
    assert!(open(vec![string("bad\0path")]).is_err());
    assert!(open(vec![string(&"x".repeat(MAX_PATH_BYTES + 1))]).is_err());
    let mut handles = Vec::new();
    for _ in 0..MAX_OBJECTS {
        handles.push(open(vec![string(&"x".repeat(MAX_PATH_BYTES))]).unwrap());
    }
    assert!(open(vec![string("one-too-many.png")]).is_err());
    assert_eq!(STATE.with(|state| state.borrow().pixels), 0);
    for handle in handles {
        close(vec![handle]).unwrap();
    }
    assert_eq!(
        stats().unwrap(),
        object(vec![("active_images", int(0)), ("pixel_bytes", int(0))])
    );
}
