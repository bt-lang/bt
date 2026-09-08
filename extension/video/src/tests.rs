//! Regression tests exercise real FFmpeg through the same SDK protocol as WASM.

use super::*;
use std::{process::Command, thread, time::Duration};

/// Run a native media tool and retain diagnostic output on failure.
fn command(program: &str, args: &[String]) -> Vec<u8> {
    let output = Command::new(program)
        .args(args)
        .output()
        .expect("media backend must be installed");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Build a fixture with generated color bars and sine audio, without external assets.
fn fixture(path: &str, audio_only: bool) {
    let mut args = strings(&["-v", "error", "-nostdin", "-y"]);
    if !audio_only {
        args.extend(strings(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=25:duration=2",
        ]));
    }
    args.extend(strings(&[
        "-f",
        "lavfi",
        "-i",
        if audio_only {
            "sine=frequency=880:duration=0.6"
        } else {
            "sine=frequency=440:duration=2"
        },
    ]));
    if !audio_only {
        args.extend(strings(&[
            "-c:v",
            "mpeg4",
            "-q:v",
            "4",
            "-c:a",
            "aac",
            "-shortest",
        ]));
    }
    args.push(path.into());
    command("ffmpeg", &args);
}

/// Read a field from a plain BT object in assertions.
fn get<'a>(value: &'a BtValue, name: &str) -> &'a BtValue {
    let BtValue::Object(values) = value else {
        panic!("expected object: {value:?}")
    };
    &values
        .iter()
        .find(|(k, _)| k == name)
        .expect("field exists")
        .1
}

/// Poll an asynchronous job to a successful terminal state, then dispose it.
fn complete(job: BtValue) -> BtValue {
    let started = Instant::now();
    loop {
        let snapshot = status(vec![job.clone()]).unwrap();
        let state = get(&snapshot, "state").as_str().unwrap();
        if state == "succeeded" {
            let result = get(&snapshot, "result").clone();
            job_close(vec![job]).unwrap();
            return result;
        }
        assert!(state == "running" || state == "probing", "{snapshot:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "job failed to finish"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Inspect and fully decode a completed file; metadata alone is insufficient.
fn inspect(path: &str) -> Value {
    command(
        "ffmpeg",
        &strings(&["-v", "error", "-i", path, "-f", "null", "-"]),
    );
    let output = command("ffprobe", &probe_args(path));
    validate_probe(serde_json::from_slice(&output).unwrap()).unwrap()
}

/// Decode a short mono PCM segment to verify actual replacement audio content.
fn pcm(path: &str, start: &str) -> Vec<i16> {
    command(
        "ffmpeg",
        &strings(&[
            "-v", "error", "-ss", start, "-i", path, "-t", "0.4", "-vn", "-ac", "1", "-ar", "8000",
            "-f", "s16le", "-",
        ]),
    )
    .chunks_exact(2)
    .map(|b| i16::from_le_bytes([b[0], b[1]]))
    .collect()
}

/// Reject oversized metadata, invalid paths, invalid options and malformed progress.
#[test]
fn validation_boundaries() {
    for value in [
        "../a.mp4",
        "/a.mp4",
        "C:/a.mp4",
        "https://host/a.mp4",
        "x%20.mp4",
        "a\nb.mp4",
    ] {
        assert!(safe_path(value).is_err());
    }
    assert!(dimension(&3.into()).is_err());
    assert!(options(&BtValue::Object(vec![("unknown".into(), true.into())])).is_err());
    assert!(number(&BtValue::Float(f64::INFINITY), "time", 0.0, 1.0).is_err());
    let oversized = json!({"format":{"duration":"1"},"streams":[{"codec_type":"video","width":10000,"height":10000,"avg_frame_rate":"25/1"}]});
    assert!(validate_probe(oversized).is_err());
    assert_eq!(
        progress("out_time_us=1200000\nprogress=continue\nout_time_us=2500000\n"),
        2.5
    );
    assert_eq!(progress("out_time_us=N/A\n"), 0.0);
    assert_eq!(demuxer("disguised.mp4"), "mov");
}

/// Verify all public operations against reproducible media and lifecycle failures.
#[test]
fn real_media_operations_and_lifecycle() {
    let base = format!("target/native-video-smoke-{}", std::process::id());
    fs::create_dir_all(&base).unwrap();
    let p = |name: &str| format!("{base}/{name}");
    fixture(&p("source.mp4"), false);
    fixture(&p("tone.wav"), true);
    let empty = BtValue::Object(vec![]);
    let bindings: Value = serde_json::from_str(include_str!("../bindings.json")).unwrap();
    assert_eq!(bindings["functions"].as_array().unwrap().len(), 1);
    assert_eq!(bindings["functions"][0]["name"], "video");
    let methods = bindings["objects"][0]["methods"].as_array().unwrap();
    let restore_binding = methods
        .iter()
        .find(|method| method["name"] == "job")
        .unwrap();
    assert_eq!(restore_binding["id"], 15);
    assert_eq!(restore_binding["returns"], "VideoJob");
    assert!(open(vec![]).unwrap_err().contains("video"));
    let src = open(vec![p("source.mp4").into(), empty.clone()]).unwrap();
    let information = complete(info(vec![src.clone()]).unwrap());
    assert_eq!(get(&information, "duration"), &2.0.into());
    let operations: Vec<(&str, BtValue)> = vec![
        (
            "transcoded.mp4",
            transcode(vec![src.clone(), p("transcoded.mp4").into(), empty.clone()]).unwrap(),
        ),
        (
            "trimmed.mp4",
            trim(vec![
                src.clone(),
                p("trimmed.mp4").into(),
                0.4.into(),
                0.8.into(),
                empty.clone(),
            ])
            .unwrap(),
        ),
    ];
    for (name, job) in operations {
        complete(job);
        inspect(&p(name));
    }
    assert!((inspect(&p("trimmed.mp4"))["duration"].as_f64().unwrap() - 0.8).abs() < 0.08);
    complete(
        concat(vec![
            src.clone(),
            BtValue::Array(vec![p("source.mp4").into()]),
            p("joined.mp4").into(),
            empty.clone(),
        ])
        .unwrap(),
    );
    assert!((inspect(&p("joined.mp4"))["duration"].as_f64().unwrap() - 4.0).abs() < 0.12);
    complete(frame(vec![src.clone(), p("frame.png").into(), 0.5.into()]).unwrap());
    assert_eq!(
        &fs::read(p("frame.png")).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );
    complete(frame(vec![src.clone(), p("frame.jpg").into(), 0.5.into()]).unwrap());
    assert_eq!(&fs::read(p("frame.jpg")).unwrap()[..2], b"\xff\xd8");
    complete(
        resize(vec![
            src.clone(),
            p("small.mp4").into(),
            160.into(),
            90.into(),
            empty.clone(),
        ])
        .unwrap(),
    );
    let small = inspect(&p("small.mp4"));
    assert_eq!(stream(&small, "video").unwrap()["width"], 160);
    for suffix in ["wav", "flac", "m4a"] {
        let name = p(&format!("audio.{suffix}"));
        complete(extract_audio(vec![src.clone(), name.clone().into()]).unwrap());
        let metadata = inspect(&name);
        assert!(stream(&metadata, "video").is_none());
        assert!(stream(&metadata, "audio").is_some());
    }
    complete(
        replace_audio(vec![
            src.clone(),
            p("tone.wav").into(),
            p("replaced.mp4").into(),
            empty.clone(),
        ])
        .unwrap(),
    );
    assert!((inspect(&p("replaced.mp4"))["duration"].as_f64().unwrap() - 2.0).abs() < 0.08);
    let replaced_pcm = pcm(&p("replaced.mp4"), "0.05");
    let crossings = replaced_pcm
        .windows(2)
        .filter(|s| (s[0] < 0) != (s[1] < 0))
        .count();
    assert!(
        (650..=750).contains(&crossings),
        "replacement must contain the 880 Hz tone, got {crossings} crossings"
    );
    assert!(
        pcm(&p("replaced.mp4"), "1.2")
            .iter()
            .all(|s| s.unsigned_abs() < 16),
        "short replacement audio must be padded with silence"
    );
    complete(transcode(vec![src.clone(), p("converted.webm").into(), empty.clone()]).unwrap());
    let webm = inspect(&p("converted.webm"));
    assert_eq!(stream(&webm, "video").unwrap()["codec_name"], "vp9");
    assert!(transcode(vec![src.clone(), p("small.mp4").into(), empty.clone()]).is_err());
    let bad = trim(vec![
        src.clone(),
        p("bad.mp4").into(),
        1.8.into(),
        1.0.into(),
        empty.clone(),
    ])
    .unwrap();
    loop {
        let s = status(vec![bad.clone()]).unwrap();
        if get(&s, "state").as_str() != Some("probing") {
            assert_eq!(get(&s, "state").as_str(), Some("failed"));
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    job_close(vec![bad]).unwrap();
    assert!(!Path::new(&p("bad.mp4")).exists());
    let cancelled = resize(vec![
        src.clone(),
        p("cancelled.mp4").into(),
        4096.into(),
        2160.into(),
        empty.clone(),
    ])
    .unwrap();
    let id = job_id(vec![cancelled.clone()]).unwrap();
    assert_eq!(
        restore_job(vec![src.clone(), id.clone()]).unwrap(),
        cancelled
    );
    let other = open(vec![p("tone.wav").into(), empty.clone()]).unwrap();
    assert!(restore_job(vec![other.clone(), id.clone()])
        .unwrap_err()
        .contains("different source path"));
    source_close(vec![other]).unwrap();
    for invalid in [0.into(), (-1).into(), 0.5.into(), "1".into()] {
        assert!(restore_job(vec![src.clone(), invalid]).is_err());
    }
    assert!(restore_job(vec![src.clone(), i64::MAX.into()]).is_err());
    let reopened = open(vec![format!("./{}", p("source.mp4")).into(), empty.clone()]).unwrap();
    let alias = restore_job(vec![reopened.clone(), id.clone()]).unwrap();
    source_close(vec![reopened.clone()]).unwrap();
    assert!(restore_job(vec![reopened, id.clone()]).is_err());
    assert_eq!(job_id(vec![alias.clone()]).unwrap(), id);
    loop {
        let s = status(vec![cancelled.clone()]).unwrap();
        if get(&s, "state").as_str() == Some("running") {
            break;
        }
        assert_eq!(get(&s, "state").as_str(), Some("probing"));
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        get(&cancel(vec![cancelled.clone()]).unwrap(), "state").as_str(),
        Some("cancelled")
    );
    job_close(vec![alias.clone()]).unwrap();
    assert!(status(vec![cancelled]).is_err());
    assert!(job_close(vec![alias]).is_err());
    assert!(restore_job(vec![src.clone(), id]).is_err());
    let short = open(vec![
        p("source.mp4").into(),
        BtValue::Object(vec![("timeout_ms".into(), 1.into())]),
    ])
    .unwrap();
    let timed = info(vec![short.clone()]).unwrap();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        get(&status(vec![timed.clone()]).unwrap(), "state").as_str(),
        Some("timed_out")
    );
    job_close(vec![timed]).unwrap();
    source_close(vec![short]).unwrap();
    // A process can finish before the caller polls it. Cancel/close must still
    // discard unacknowledged output without racing the worker's finalization.
    for (name, should_cancel) in [("unpolled_cancel.mp4", true), ("unpolled_close.mp4", false)] {
        let job = transcode(vec![src.clone(), p(name).into(), empty.clone()]).unwrap();
        loop {
            let snapshot = status(vec![job.clone()]).unwrap();
            if get(&snapshot, "state").as_str() == Some("running") {
                break;
            }
            assert_eq!(get(&snapshot, "state").as_str(), Some("probing"));
            thread::sleep(Duration::from_millis(10));
        }
        // Poll the host directly so the VideoJob deliberately remains running.
        let object_id = job.as_ext_object().unwrap().object_id;
        let process_id = JOBS.with(|jobs| {
            jobs.borrow()
                .get_required(object_id, "VideoJob")
                .unwrap()
                .process_id
                .unwrap()
        });
        loop {
            let response = host(json!({"op":"poll","id":process_id})).unwrap();
            if response["state"] == "succeeded" {
                break;
            }
            assert!(response["state"] == "running" || response["state"] == "queued");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(Path::new(&p(name)).exists());
        if should_cancel {
            cancel(vec![job.clone()]).unwrap();
        }
        job_close(vec![job]).unwrap();
        for _ in 0..100 {
            if !Path::new(&p(name)).exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !Path::new(&p(name)).exists(),
            "unpolled output survived dispose"
        );
    }
    for _ in 0..36 {
        complete(info(vec![src.clone()]).unwrap());
    }
    source_close(vec![src.clone()]).unwrap();
    assert!(info(vec![src]).is_err());
    for _ in 0..100 {
        if !Path::new(&p("cancelled.mp4")).exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!Path::new(&p("cancelled.mp4")).exists());
    shutdown().unwrap();
}
