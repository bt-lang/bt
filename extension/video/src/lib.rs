//! File-based video operations; native codecs run outside the WASM worker.
//!
//! Jobs advance from metadata preflight to encoding during nonblocking status calls.
//! Only bounded metadata crosses the ABI; encoded packets and frames stay in FFmpeg.

use bt_extension_sdk::{
    bt_extension, bt_extension_shutdown, expect_arg_count, expect_ext_object_type, expect_string,
    BtResult, BtValue, ExtObject, ObjectStore,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    fs,
    path::{Component, Path},
    time::Instant,
};

/// Maximum retained source descriptors in one extension worker.
const MAX_SOURCES: usize = 64;
/// Maximum retained jobs, including completed jobs awaiting close.
const MAX_JOBS: usize = 32;

thread_local! {
    /// Small source descriptors, without decoded media buffers.
    static SOURCES: RefCell<ObjectStore<Source>> = RefCell::new(ObjectStore::new(MAX_SOURCES));
    /// Jobs own process handles and clean them up when removed.
    static JOBS: RefCell<ObjectStore<Job>> = RefCell::new(ObjectStore::new(MAX_JOBS));
}

/// Validated source path and inherited execution deadline.
#[derive(Clone)]
struct Source {
    /// Project-relative filename.
    path: String,
    /// Maximum combined probe and encode wall time.
    timeout_ms: u64,
}

/// Fixed output settings; arbitrary codec or filter arguments are not accepted.
#[derive(Clone, Debug)]
struct Options {
    /// MPEG-4 quality scale (2 is best, 31 is worst).
    quality: u64,
    /// Explicit output frame rate, or source timing if absent.
    fps: Option<f64>,
    /// Whether concatenation retains and normalizes audio.
    audio: bool,
}

/// One supported media operation and its bounded parameters.
#[derive(Clone, Debug)]
enum Operation {
    /// Return validated metadata without encoding.
    Info,
    /// Re-encode video and optional first audio stream.
    Transcode,
    /// Accurate trim after decoding from the requested seek point.
    Trim {
        /// Seek position in seconds from the beginning of the media timeline.
        start: f64,
        /// Length in seconds, bounded by the probed input duration.
        duration: f64,
    },
    /// Normalize timestamps, geometry, frame rate and audio before joining.
    Concat,
    /// Decode exactly one frame at or after a timestamp.
    Frame {
        /// Timestamp in seconds; the first following decoded frame is selected.
        time: f64,
    },
    /// Resize to explicitly even dimensions.
    Resize {
        /// Even output width in pixels.
        width: u64,
        /// Even output height in pixels.
        height: u64,
    },
    /// Encode the first audio stream in an audio-only container.
    ExtractAudio,
    /// Replace audio from time zero, padding short tracks and cutting long ones.
    ReplaceAudio,
}

/// A staged asynchronous operation with explicit ownership of its output.
struct Job {
    /// Current host process; None after completion or between stages.
    process_id: Option<u64>,
    /// Input filenames, at most eight.
    inputs: Vec<String>,
    /// Validated metadata gathered incrementally.
    probes: Vec<Value>,
    /// Operation to launch after preflight.
    operation: Operation,
    /// Bounded output settings.
    options: Options,
    /// Reserved output filename, absent for info.
    output: Option<String>,
    /// True only after a create_new reservation succeeds.
    owns_output: bool,
    /// Monotonic start time for the combined deadline.
    started: Instant,
    /// Combined deadline in milliseconds.
    timeout_ms: u64,
    /// Public state: probing, running, succeeded, failed, cancelled or timed_out.
    state: String,
    /// Latest bounded English failure details.
    error: String,
    /// Last reported output timeline position in seconds.
    processed_seconds: f64,
    /// Successful result, missing while unfinished or unsuccessful.
    result: Option<Value>,
}

impl Drop for Job {
    /// Release the exact host handle and any unsuccessful owned output.
    fn drop(&mut self) {
        let active = self.process_id.take();
        if let Some(id) = active {
            let _ = host(
                json!({"op":"close", "id":id, "discard_output":self.owns_output && self.state != "succeeded"}),
            );
        }
        if active.is_none() && self.owns_output && self.state != "succeeded" {
            if let Some(path) = &self.output {
                let _ = fs::remove_file(path);
            }
        }
    }
}

bt_extension!(1 => open, 2 => info, 3 => transcode, 4 => trim,
    5 => concat, 6 => frame, 7 => resize, 8 => extract_audio,
    9 => replace_audio, 10 => source_close, 11 => status,
    12 => cancel, 13 => job_close, 14 => job_id, 15 => restore_job);
bt_extension_shutdown!(shutdown);

/// Validate the source without decoding it or starting a process.
fn open(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "video")?;
    let path = input_path(&expect_string(&args, 0, "path")?)?;
    let fields = fields(&args[1])?;
    reject_unknown(fields, &["timeout_ms"])?;
    let timeout_ms = integer(fields, "timeout_ms", 60000, 1, 300000)?;
    let id = SOURCES.with(|s| s.borrow_mut().insert(Source { path, timeout_ms }))?;
    Ok(ExtObject::new(1, id, "Video").into())
}

/// Read a live source receiver.
fn source(args: &[BtValue]) -> BtResult<Source> {
    let id = expect_ext_object_type(args, 0, "self", 1, "Video")?.object_id;
    SOURCES.with(|s| s.borrow().get_required(id, "Video").cloned())
}

/// Start metadata inspection as a nonblocking job.
fn info(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "Video.info")?;
    start(
        source(&args)?,
        Operation::Info,
        None,
        vec![],
        default_options(),
    )
}

/// Start transcoding using a fixed container-to-codec mapping.
fn transcode(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 3, "Video.transcode")?;
    start(
        source(&args)?,
        Operation::Transcode,
        Some(expect_string(&args, 1, "output")?),
        vec![],
        options(&args[2])?,
    )
}

/// Start an accurate time trim; times use seconds relative to the first frame.
fn trim(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 5, "Video.trim")?;
    let start_time = number(&args[2], "start", 0.0, 86400.0)?;
    let duration = number(&args[3], "duration", 0.001, 86400.0)?;
    start(
        source(&args)?,
        Operation::Trim {
            start: start_time,
            duration,
        },
        Some(expect_string(&args, 1, "output")?),
        vec![],
        options(&args[4])?,
    )
}

/// Join this source and up to seven additional project-relative files.
fn concat(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 4, "Video.concat")?;
    let BtValue::Array(values) = &args[1] else {
        return Err("paths must be an array".into());
    };
    if values.is_empty() || values.len() > 7 {
        return Err("concat requires 1..=7 additional inputs".into());
    }
    let paths = values
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| "concat paths must be strings".into())
                .and_then(input_path)
        })
        .collect::<BtResult<_>>()?;
    start(
        source(&args)?,
        Operation::Concat,
        Some(expect_string(&args, 2, "output")?),
        paths,
        options(&args[3])?,
    )
}

/// Extract one PNG or JPEG frame for optional image-extension processing.
fn frame(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 3, "Video.frame")?;
    start(
        source(&args)?,
        Operation::Frame {
            time: number(&args[2], "time", 0.0, 86400.0)?,
        },
        Some(expect_string(&args, 1, "output")?),
        vec![],
        default_options(),
    )
}

/// Resize and re-encode video with a bounded even output geometry.
fn resize(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 5, "Video.resize")?;
    let width = dimension(&args[2])?;
    let height = dimension(&args[3])?;
    if width * height > 16777216 {
        return Err("output exceeds 16777216 pixels".into());
    }
    start(
        source(&args)?,
        Operation::Resize { width, height },
        Some(expect_string(&args, 1, "output")?),
        vec![],
        options(&args[4])?,
    )
}

/// Extract and encode the first audio stream without loading it into WASM.
fn extract_audio(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Video.extract_audio")?;
    start(
        source(&args)?,
        Operation::ExtractAudio,
        Some(expect_string(&args, 1, "output")?),
        vec![],
        default_options(),
    )
}

/// Replace the first audio stream, preserving video duration from time zero.
fn replace_audio(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 4, "Video.replace_audio")?;
    let audio = input_path(&expect_string(&args, 1, "audio")?)?;
    start(
        source(&args)?,
        Operation::ReplaceAudio,
        Some(expect_string(&args, 2, "output")?),
        vec![audio],
        options(&args[3])?,
    )
}

/// Release a source descriptor; already created jobs own independent snapshots.
fn source_close(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "Video.close")?;
    let id = expect_ext_object_type(&args, 0, "self", 1, "Video")?.object_id;
    SOURCES.with(|s| s.borrow_mut().remove_required(id, "Video"))?;
    Ok(true.into())
}

/// Create a bounded job and begin the first asynchronous metadata probe.
fn start(
    src: Source,
    operation: Operation,
    output: Option<String>,
    extra: Vec<String>,
    options: Options,
) -> BtResult<BtValue> {
    if let Some(path) = &output {
        output_path(path, &operation)?;
    }
    let mut inputs = vec![src.path];
    inputs.extend(extra);
    let mut job = Job {
        process_id: None,
        inputs,
        probes: vec![],
        operation,
        options,
        output,
        owns_output: false,
        started: Instant::now(),
        timeout_ms: src.timeout_ms,
        state: "probing".into(),
        error: String::new(),
        processed_seconds: 0.0,
        result: None,
    };
    job.launch_probe()?;
    let id = JOBS.with(|j| j.borrow_mut().insert(job))?;
    Ok(ExtObject::new(2, id, "VideoJob").into())
}

impl Job {
    /// Return the remaining overall deadline, including time between status calls.
    fn remaining_ms(&self) -> BtResult<u64> {
        self.timeout_ms
            .checked_sub(self.started.elapsed().as_millis() as u64)
            .filter(|n| *n > 0)
            .ok_or_else(|| "video job deadline exceeded".into())
    }

    /// Start one metadata process; at most one native process belongs to a job.
    fn launch_probe(&mut self) -> BtResult<()> {
        let args = probe_args(&self.inputs[self.probes.len()]);
        self.launch("ffprobe", args)
    }

    /// Submit only argument arrays to the generic bounded process host.
    fn launch(&mut self, program: &str, args: Vec<String>) -> BtResult<()> {
        let cleanup = if self.owns_output {
            self.output.iter().cloned().collect::<Vec<_>>()
        } else {
            vec![]
        };
        let response = host(json!({"op":"spawn", "program":program, "args":args,
            "timeout_ms":self.remaining_ms()?, "cleanup_paths":cleanup,
            "read_paths":self.inputs, "write_paths":cleanup}))?;
        self.process_id = Some(response["id"].as_u64().ok_or("invalid host process id")?);
        Ok(())
    }

    /// Record a terminal failure and reclaim the current process handle.
    fn fail(&mut self, state: &str, message: String) {
        self.state = state.into();
        self.error = message;
        let active = self.process_id.take();
        if let Some(id) = active {
            let _ = host(json!({"op":"close", "id":id, "discard_output":self.owns_output}));
            // The native worker now owns cleanup; do not race it on Windows.
            self.owns_output = false;
        }
        if active.is_none() && self.owns_output {
            if let Some(path) = &self.output {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// Poll once and advance at most one process stage without waiting for media work.
    fn advance(&mut self) -> BtResult<()> {
        if self.state != "probing" && self.state != "running" {
            return Ok(());
        }
        if self.remaining_ms().is_err() {
            self.fail("timed_out", "video job deadline exceeded".into());
            return Ok(());
        }
        let id = self.process_id.ok_or("video job has no process")?;
        let response = host(json!({"op":"poll", "id":id}))?;
        let state = response["state"]
            .as_str()
            .ok_or("invalid host process state")?;
        if self.state == "running" {
            self.processed_seconds = progress(response["stdout"].as_str().unwrap_or(""));
        }
        if state == "running" || state == "queued" {
            return Ok(());
        }
        host(json!({"op":"close", "id":id}))?;
        self.process_id = None;
        if state != "succeeded" {
            self.fail(
                state,
                response["stderr"]
                    .as_str()
                    .unwrap_or("FFmpeg process failed")
                    .to_string(),
            );
            return Ok(());
        }
        if self.state == "probing" {
            if response["stdout_truncated"] == true {
                return Err("ffprobe metadata exceeds host output limit".into());
            }
            let raw: Value = serde_json::from_str(
                response["stdout"]
                    .as_str()
                    .ok_or("missing ffprobe output")?,
            )
            .map_err(|e| format!("invalid ffprobe JSON: {e}"))?;
            self.probes.push(validate_probe(raw)?);
            if self.probes.len() < self.inputs.len() {
                return self.launch_probe();
            }
            validate_operation(self)?;
            if matches!(self.operation, Operation::Info) {
                self.result = Some(self.probes[0].clone());
                self.state = "succeeded".into();
                return Ok(());
            }
            let args = encode_args(self)?;
            let path = self.output.as_ref().ok_or("missing output")?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| format!("cannot reserve output `{path}`: {e}"))?;
            self.owns_output = true;
            self.launch("ffmpeg", args)?;
            self.state = "running".into();
        } else {
            let path = self.output.as_ref().ok_or("missing output")?;
            let size = fs::metadata(path)
                .map_err(|e| format!("cannot inspect output: {e}"))?
                .len();
            if size == 0 {
                return Err("FFmpeg produced an empty output".into());
            }
            if size >= 2147483648 {
                return Err("output reached the 2 GiB limit".into());
            }
            self.result = Some(json!({"path":path, "size_bytes":size}));
            self.state = "succeeded".into();
        }
        Ok(())
    }

    /// Build the public snapshot, using empty for unavailable results.
    fn snapshot(&self) -> BtValue {
        let mut value = to_bt(
            json!({"state":self.state,"elapsed_ms":self.started.elapsed().as_millis() as u64,
            "processed_seconds":self.processed_seconds,"error":self.error}),
        );
        if let BtValue::Object(fields) = &mut value {
            fields.push((
                "result".into(),
                self.result.clone().map(to_bt).unwrap_or(BtValue::Empty),
            ));
        }
        value
    }
}

/// Poll progress and continue metadata/encode stages without blocking.
fn status(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "VideoJob.status")?;
    let id = expect_ext_object_type(&args, 0, "self", 2, "VideoJob")?.object_id;
    JOBS.with(|jobs| {
        let mut jobs = jobs.borrow_mut();
        let job = jobs.get_mut_required(id, "VideoJob")?;
        if let Err(error) = job.advance() {
            job.fail("failed", error);
        }
        Ok(job.snapshot())
    })
}

/// Cancel exactly this job; completed jobs keep their successful output.
fn cancel(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "VideoJob.cancel")?;
    let id = expect_ext_object_type(&args, 0, "self", 2, "VideoJob")?.object_id;
    JOBS.with(|jobs| {
        let mut jobs = jobs.borrow_mut();
        let job = jobs.get_mut_required(id, "VideoJob")?;
        if job.state == "probing" || job.state == "running" {
            job.fail("cancelled", "video job cancelled".into());
        }
        Ok(job.snapshot())
    })
}

/// Dispose a job handle and cancel any unfinished work.
fn job_close(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "VideoJob.close")?;
    let id = expect_ext_object_type(&args, 0, "self", 2, "VideoJob")?.object_id;
    JOBS.with(|jobs| jobs.borrow_mut().remove_required(id, "VideoJob"))?;
    Ok(true.into())
}

/// Export a project-local job identifier for subsequent short Web requests.
fn job_id(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "VideoJob.id")?;
    let id = expect_ext_object_type(&args, 0, "self", 2, "VideoJob")?.object_id;
    JOBS.with(|jobs| {
        jobs.borrow()
            .get_required(id, "VideoJob")
            .map(|_| BtValue::Int(id as i64))
    })
}

/// Restore a live job belonging to this source; IDs are not authorization tokens.
fn restore_job(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Video.job")?;
    let src = source(&args)?;
    let id = args[1]
        .as_int()
        .filter(|v| *v > 0)
        .ok_or("job id must be a positive integer")? as u64;
    JOBS.with(|jobs| {
        let jobs = jobs.borrow();
        let job = jobs.get_required(id, "VideoJob")?;
        // Paths are already project-relative and link-free. Compare components
        // without allocating or reopening media; redundant dots and slashes do
        // not change source identity. This check does not authenticate a caller.
        let source_components = Path::new(&src.path)
            .components()
            .filter(|part| !matches!(part, Component::CurDir));
        let job_components = Path::new(&job.inputs[0])
            .components()
            .filter(|part| !matches!(part, Component::CurDir));
        if !source_components.eq(job_components) {
            return Err("video job belongs to a different source path".into());
        }
        Ok(ExtObject::new(2, id, "VideoJob").into())
    })
}

/// Clear all retained state; dropping jobs cancels their native processes.
fn shutdown() -> BtResult<BtValue> {
    JOBS.with(|j| *j.borrow_mut() = ObjectStore::new(MAX_JOBS));
    SOURCES.with(|s| *s.borrow_mut() = ObjectStore::new(MAX_SOURCES));
    Ok(BtValue::Empty)
}

/// Exchange small JSON control messages with the optional process host.
fn host(value: Value) -> BtResult<Value> {
    let response = bt_extension_sdk::host_process::request(&value.to_string())?;
    serde_json::from_str(&response).map_err(|e| format!("invalid process host response: {e}"))
}

/// Construct fixed, local-file-only metadata arguments with bounded probing.
fn probe_args(path: &str) -> Vec<String> {
    strings(&["-v", "error", "-max_alloc", "67108864", "-protocol_whitelist", "file",
        "-probesize", "5000000", "-analyzeduration", "5000000", "-show_entries",
        "format=duration,format_name,size:stream=index,codec_type,codec_name,width,height,avg_frame_rate,sample_rate,channels,duration,start_time",
        "-of", "json", "-f", demuxer(path), "-i", &format!("./{path}")])
}

/// Validate metadata before any video decoder is launched.
fn validate_probe(raw: Value) -> BtResult<Value> {
    let streams = raw["streams"].as_array().ok_or("media has no streams")?;
    if streams.is_empty() || streams.len() > 16 {
        return Err("media must have 1..=16 streams".into());
    }
    let duration = raw["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or("media requires a finite duration")?;
    if !duration.is_finite() || !(0.001..=86400.0).contains(&duration) {
        return Err("media duration must be 0.001..=86400 seconds".into());
    }
    for stream in streams {
        if stream["codec_type"] == "video" {
            let w = stream["width"].as_u64().unwrap_or(0);
            let h = stream["height"].as_u64().unwrap_or(0);
            if w == 0 || h == 0 || w > 8192 || h > 8192 || w * h > 16777216 {
                return Err("video dimensions exceed 8192 per axis or 16777216 pixels".into());
            }
            let fps = rational(stream["avg_frame_rate"].as_str().unwrap_or("0/0"));
            if !(0.001..=240.0).contains(&fps) {
                return Err("video frame rate must be finite and at most 240".into());
            }
        }
        if stream["codec_type"] == "audio" {
            let channels = stream["channels"].as_u64().unwrap_or(0);
            let rate = stream["sample_rate"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if !(1..=8).contains(&channels) || !(8000..=192000).contains(&rate) {
                return Err("audio requires 1..=8 channels and 8000..=192000 Hz".into());
            }
        }
    }
    Ok(
        json!({"duration":duration,"format":raw["format"]["format_name"],"size_bytes":raw["format"]["size"].as_str().and_then(|s|s.parse::<u64>().ok()).unwrap_or(0),"streams":streams}),
    )
}

/// Reject unsupported stream/timeline combinations before reserving output.
fn validate_operation(job: &Job) -> BtResult<()> {
    if matches!(job.operation, Operation::Info) {
        return Ok(());
    }
    let first = &job.probes[0];
    if matches!(job.operation, Operation::ExtractAudio) {
        if stream(first, "audio").is_none() {
            return Err("input has no audio stream".into());
        }
    } else if stream(first, "video").is_none() {
        return Err("input has no video stream".into());
    }
    let duration = first["duration"].as_f64().ok_or("missing duration")?;
    match job.operation {
        Operation::Trim {
            start,
            duration: length,
        } if start >= duration || start + length > duration + 0.001 => {
            return Err("trim range exceeds input duration".into())
        }
        Operation::Frame { time } if time >= duration => {
            return Err("frame timestamp must be below input duration".into())
        }
        Operation::Concat => {
            if job
                .probes
                .iter()
                .filter_map(|p| p["duration"].as_f64())
                .sum::<f64>()
                > 86400.0
            {
                return Err("combined concat duration exceeds 86400 seconds".into());
            }
            for probe in &job.probes {
                if stream(probe, "video").is_none() {
                    return Err("every concat input requires video".into());
                }
                if job.options.audio && stream(probe, "audio").is_none() {
                    return Err("audio concat requires audio in every input; set audio:false for video-only output".into());
                }
            }
        }
        Operation::ReplaceAudio if stream(&job.probes[1], "audio").is_none() => {
            return Err("replacement input has no audio stream".into())
        }
        _ => {}
    }
    Ok(())
}

/// Generate a fixed, bounded FFmpeg graph without accepting filter text from callers.
fn encode_args(job: &Job) -> BtResult<Vec<String>> {
    let output = job.output.as_ref().ok_or("missing output")?;
    let mut a = strings(&[
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-max_alloc",
        "67108864",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    for (index, path) in job.inputs.iter().enumerate() {
        if index == 0 {
            match job.operation {
                Operation::Trim { start, .. } => a.extend(strings(&["-ss", &start.to_string()])),
                Operation::Frame { time } => a.extend(strings(&["-ss", &time.to_string()])),
                _ => {}
            }
        }
        a.extend(strings(&[
            "-protocol_whitelist",
            "file",
            "-probesize",
            "5000000",
            "-analyzeduration",
            "5000000",
            "-threads",
            "1",
            "-f",
            demuxer(path),
            "-i",
            &format!("./{path}"),
        ]));
    }
    let mut filter = String::new();
    match job.operation {
        Operation::Concat => {
            let v = stream(&job.probes[0], "video").ok_or("missing video")?;
            let w = v["width"].as_u64().unwrap_or(0) / 2 * 2;
            let h = v["height"].as_u64().unwrap_or(0) / 2 * 2;
            let fps = job
                .options
                .fps
                .unwrap_or_else(|| rational(v["avg_frame_rate"].as_str().unwrap_or("30/1")));
            let mut graph = String::new();
            let mut labels = String::new();
            for i in 0..job.inputs.len() {
                graph.push_str(&format!(
                    "[{i}:v:0]setpts=PTS-STARTPTS,scale={w}:{h},setsar=1,fps={fps}[v{i}];"
                ));
                labels.push_str(&format!("[v{i}]"));
                if job.options.audio {
                    graph.push_str(&format!("[{i}:a:0]asetpts=PTS-STARTPTS,aresample=48000:async=1,aformat=channel_layouts=stereo[a{i}];"));
                    labels.push_str(&format!("[a{i}]"));
                }
            }
            graph.push_str(&format!(
                "{labels}concat=n={}:v=1:a={}[v]{}",
                job.inputs.len(),
                u8::from(job.options.audio),
                if job.options.audio { "[a]" } else { "" }
            ));
            a.extend(strings(&["-filter_complex", &graph, "-map", "[v]"]));
            if job.options.audio {
                a.extend(strings(&["-map", "[a]"]));
            }
        }
        Operation::ExtractAudio => {
            a.extend(strings(&["-map", "0:a:0", "-vn"]));
        }
        Operation::ReplaceAudio => {
            a.extend(strings(&[
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-af",
                "apad,aresample=48000:async=1",
                "-t",
                &job.probes[0]["duration"].to_string(),
            ]));
        }
        Operation::Frame { .. } => {
            a.extend(strings(&[
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-an",
                "-update",
                "1",
            ]));
        }
        _ => {
            a.extend(strings(&["-map", "0:v:0", "-map", "0:a:0?"]));
            if let Operation::Trim { duration, .. } = job.operation {
                a.extend(strings(&["-t", &duration.to_string()]));
            }
            if let Operation::Resize { width, height } = job.operation {
                filter = format!("scale={width}:{height},setsar=1");
            }
        }
    }
    if !matches!(
        job.operation,
        Operation::ExtractAudio | Operation::Frame { .. }
    ) {
        if !job.options.audio && !matches!(job.operation, Operation::ReplaceAudio) {
            a.push("-an".into());
        }
        if filter.is_empty() && !matches!(job.operation, Operation::Concat) {
            filter = "scale=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1".into();
        }
        if !filter.is_empty() {
            a.extend(strings(&["-vf", &filter]));
        }
        if let Some(fps) = job.options.fps {
            a.extend(strings(&["-r", &fps.to_string(), "-fps_mode", "cfr"]));
        }
        if extension(output) == "mp4" {
            a.extend(strings(&[
                "-c:v",
                "mpeg4",
                "-q:v",
                &job.options.quality.to_string(),
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
            ]));
        } else {
            a.extend(strings(&[
                "-c:v",
                "libvpx-vp9",
                "-crf",
                &(job.options.quality * 2).to_string(),
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
                "-c:a",
                "libopus",
                "-b:a",
                "128k",
            ]));
        }
        a.extend(strings(&[
            "-pix_fmt", "yuv420p", "-ac", "2", "-ar", "48000",
        ]));
    } else if matches!(job.operation, Operation::ExtractAudio) {
        let codec = match extension(output).as_str() {
            "wav" => "pcm_s16le",
            "flac" => "flac",
            _ => "aac",
        };
        a.extend(strings(&["-c:a", codec, "-ac", "2", "-ar", "48000"]));
    }
    a.extend(strings(&[
        "-threads",
        "2",
        "-max_muxing_queue_size",
        "128",
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-fs",
        "2147483648",
        &format!("./{output}"),
    ]));
    Ok(a)
}

/// Obtain the first stream of a requested media type.
fn stream<'a>(probe: &'a Value, kind: &str) -> Option<&'a Value> {
    probe["streams"]
        .as_array()?
        .iter()
        .find(|s| s["codec_type"] == kind)
}

/// Parse a rational frame rate, returning zero for unknown or invalid values.
fn rational(text: &str) -> f64 {
    let Some((a, b)) = text.split_once('/') else {
        return 0.0;
    };
    a.parse::<f64>().unwrap_or(0.0) / b.parse::<f64>().unwrap_or(0.0)
}

/// Read the latest FFmpeg progress time without retaining its history.
fn progress(text: &str) -> f64 {
    text.lines()
        .rev()
        .find_map(|l| {
            l.strip_prefix("out_time_us=")
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        .max(0.0)
        / 1_000_000.0
}

/// Restrict filenames to local project-relative regular files, never URL syntax.
fn safe_path(text: &str) -> BtResult<String> {
    let value = text.replace('\\', "/");
    if value.is_empty()
        || value.len() > 4096
        || value.contains([':', '%'])
        || value.chars().any(char::is_control)
        || Path::new(&value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("video paths must be project-relative files without parent traversal, URL syntax, percent signs or control characters".into());
    }
    // Reject links in every component: native FFmpeg must not escape the WASI root.
    let mut prefix = std::path::PathBuf::new();
    for component in Path::new(&value).components() {
        prefix.push(component);
        if fs::symlink_metadata(&prefix)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err("video paths must not contain symbolic links".into());
        }
    }
    Ok(value)
}

/// Validate input size and a deliberately finite local-media format set.
fn input_path(text: &str) -> BtResult<String> {
    let path = safe_path(text)?;
    if ![
        "mp4", "mov", "mkv", "webm", "avi", "wav", "mp3", "m4a", "flac", "ogg",
    ]
    .contains(&extension(&path).as_str())
    {
        return Err("unsupported video/audio input extension".into());
    }
    let metadata = fs::metadata(&path).map_err(|e| format!("cannot read input `{path}`: {e}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 34359738368 {
        return Err("input must be a nonempty regular file no larger than 32 GiB".into());
    }
    Ok(path)
}

/// Reject existing output files; jobs never overwrite caller-owned files.
fn output_path(text: &str, operation: &Operation) -> BtResult<()> {
    let path = safe_path(text)?;
    let allowed: &[&str] = match operation {
        Operation::Frame { .. } => &["png", "jpg", "jpeg"],
        Operation::ExtractAudio => &["wav", "flac", "m4a"],
        _ => &["mp4", "webm"],
    };
    if !allowed.contains(&extension(&path).as_str()) {
        return Err(format!(
            "unsupported output extension; expected {}",
            allowed.join(", ")
        ));
    }
    if fs::symlink_metadata(&path).is_ok() {
        return Err("output already exists; choose a new filename".into());
    }
    let parent = Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err("output parent directory must exist".into());
    }
    Ok(())
}

/// Return a lowercase filename suffix.
fn extension(text: &str) -> String {
    Path::new(text)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Force a finite demuxer set so disguised playlists cannot follow local references.
fn demuxer(path: &str) -> &'static str {
    match extension(path).as_str() {
        "mp4" | "mov" | "m4a" => "mov",
        "mkv" | "webm" => "matroska",
        "avi" => "avi",
        "wav" => "wav",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" => "ogg",
        _ => "invalid",
    }
}

/// Borrow a plain option object, accepting empty as default options.
fn fields(value: &BtValue) -> BtResult<&[(String, BtValue)]> {
    match value {
        BtValue::Object(v) => Ok(v),
        BtValue::Empty => Ok(&[]),
        _ => Err("options must be an object".into()),
    }
}

/// Reject misspelled options rather than silently ignoring their effect.
fn reject_unknown(fields: &[(String, BtValue)], names: &[&str]) -> BtResult<()> {
    for (k, _) in fields {
        if !names.contains(&k.as_str()) {
            return Err(format!("unknown video option `{k}`"));
        }
    }
    Ok(())
}

/// Find one option without copying the object.
fn field<'a>(fields: &'a [(String, BtValue)], name: &str) -> Option<&'a BtValue> {
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// Read a bounded integer option.
fn integer(
    fields: &[(String, BtValue)],
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> BtResult<u64> {
    match field(fields, name) {
        None => Ok(default),
        Some(BtValue::Int(v)) if *v >= min as i64 && *v <= max as i64 => Ok(*v as u64),
        _ => Err(format!("{name} must be an integer in {min}..={max}")),
    }
}

/// Read a finite floating-point parameter.
fn number(value: &BtValue, name: &str, min: f64, max: f64) -> BtResult<f64> {
    let n = match value {
        BtValue::Int(n) => *n as f64,
        BtValue::Float(n) => *n,
        _ => return Err(format!("{name} must be a number")),
    };
    if !n.is_finite() || !(min..=max).contains(&n) {
        return Err(format!("{name} must be in {min}..={max}"));
    }
    Ok(n)
}

/// Read an even dimension accepted by the YUV 4:2:0 output encoders.
fn dimension(value: &BtValue) -> BtResult<u64> {
    match value {
        BtValue::Int(n) if *n >= 2 && *n <= 8192 && *n % 2 == 0 => Ok(*n as u64),
        _ => Err("dimensions must be even integers in 2..=8192".into()),
    }
}

/// Return consistent defaults for every video encoder.
fn default_options() -> Options {
    Options {
        quality: 5,
        fps: None,
        audio: true,
    }
}

/// Parse the shared encoding options and reject invalid values.
fn options(value: &BtValue) -> BtResult<Options> {
    let f = fields(value)?;
    reject_unknown(f, &["quality", "fps", "audio"])?;
    let audio = match field(f, "audio") {
        None => true,
        Some(BtValue::Bool(v)) => *v,
        _ => return Err("audio must be a bool".into()),
    };
    Ok(Options {
        quality: integer(f, "quality", 5, 2, 31)?,
        fps: field(f, "fps")
            .map(|v| number(v, "fps", 1.0, 120.0))
            .transpose()?,
        audio,
    })
}

/// Convert owned JSON metadata to BT values without Base64 or media payloads.
fn to_bt(value: Value) -> BtValue {
    match value {
        Value::Null => BtValue::Null,
        Value::Bool(v) => v.into(),
        Value::Number(v) => v
            .as_i64()
            .map(BtValue::Int)
            .unwrap_or_else(|| BtValue::Float(v.as_f64().unwrap_or(0.0))),
        Value::String(v) => v.into(),
        Value::Array(v) => BtValue::Array(v.into_iter().map(to_bt).collect()),
        Value::Object(v) => BtValue::Object(v.into_iter().map(|(k, v)| (k, to_bt(v))).collect()),
    }
}

/// Copy static command tokens into the native process argument array.
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

#[cfg(test)]
mod tests;
