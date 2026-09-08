"""Verify installed media packages, file interoperability, and short Web requests.

Only standard-library modules are used. Every process and output belongs to the
fresh project created under the repository's ignored target directory.
"""

import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import socket
import subprocess
import time
import urllib.parse
import urllib.request
import uuid
import zipfile


def run(command, cwd, marker=None):
    """Run a bounded validation command and require its explicit success marker."""
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True,
                            encoding="utf-8", errors="replace", timeout=120)
    output = result.stdout + result.stderr
    if result.returncode or (marker and marker not in output):
        raise RuntimeError(f"Command failed: {command}\n{output}")
    return output


def main():
    """Build an isolated acceptance project and emit a machine-readable report."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bt", default="target/debug/bt.exe")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    bt = (repo / args.bt).resolve()
    project = repo / "target/quality/media-extensions" / ("integration-" + uuid.uuid4().hex)
    project.mkdir(parents=True)
    packages = {}
    for name in ("image", "video", "sqlite"):
        package = repo / f"extension/{name}/target/{name}-1.0.0.bts"
        with zipfile.ZipFile(package) as archive:
            assert archive.testzip() is None
            entries = set(archive.namelist())
            expected = {"manifest.json", "bindings.json", "module.wasm", "README.md",
                        "LICENSE-MIT", "LICENSE-APACHE", "COPYRIGHT", "THIRD_PARTY_LICENSES.txt"}
            assert entries == expected, entries
            manifest = json.loads(archive.read("manifest.json"))
            assert manifest["name"] == name and manifest["version"] == "1.0.0"
            bindings = json.loads(archive.read("bindings.json"))
            entry_names = {entry["name"] for entry in bindings["functions"]}
            assert entry_names == ({"sqlite", "sqlite_open"} if name == "sqlite" else {name})
            assert len(archive.read("THIRD_PARTY_LICENSES.txt")) > 1000
        packages[name] = {"path": str(package), "bytes": package.stat().st_size,
                          "sha256": hashlib.sha256(package.read_bytes()).hexdigest(),
                          "entries": sorted(entries)}
        run([str(bt), "ext", "check", str(package)], project, "Extension package check passed:")
        run([str(bt), "ext", "install", str(package), str(project)], project, "Installed extension package:")
    backend = run(["ffmpeg", "-version"], project).splitlines()[0]
    run(["ffmpeg", "-v", "error", "-nostdin", "-f", "lavfi", "-i",
         "testsrc2=size=640x360:rate=25:duration=6", "-f", "lavfi", "-i",
         "sine=frequency=440:duration=6", "-c:v", "mpeg4", "-q:v", "4",
         "-c:a", "aac", "-shortest", "source.mp4"], project)
    (project / "cross.bt").write_text("""source = video('@/source.mp4', {})
job = source.frame('@/frame.png', 0.5)
state = job.status()
while state.state == 'probing' || state.state == 'running' {
    sleep(25)
    state = job.status()
}
assert(state.state == 'succeeded', state.error)
job.close()
source.close()
frame = image('@/frame.png')
assert(frame.info().width == 640, 'video frame width')
frame.crop(0, 0, 320, 180).resize(160, 90, 'triangle')
     .text('BT media', 4, 4, 1, [255, 255, 255, 255])
     .adjust({brightness: 10, saturation: 0.8})
     .save('@/poster.webp', 'webp', {})
encoded = frame.encode('png', {})
copy = image('@/copy.png').decode(encoded)
assert(copy.info().width == 160, 'Bytes width')
assert(copy.info().height == 90, 'Bytes height')
assert(is_empty(copy.pixel(-1, 0)), 'missing pixel must be empty')
copy.close()
frame.close()
db = sqlite('@/media.db', {})
query = db.query('CREATE TABLE outputs (path TEXT)')
query.exec()
query.close()
query = db.query('INSERT INTO outputs VALUES (?)').bind('poster.webp')
assert(query.exec().rows_affected == 1, 'save output metadata')
query.close()
db.close()
db = sqlite_open('@/media.db', {})
query = db.query('SELECT path FROM outputs')
assert(query.one().path == 'poster.webp', 'compatible SQLite entry')
query.close()
db.close()
print 'cross-media:ok'
""", encoding="utf-8")
    started = time.perf_counter()
    cross_output = run([str(bt), "cross.bt"], project, "cross-media:ok")
    cross_ms = (time.perf_counter() - started) * 1000
    run(["ffmpeg", "-v", "error", "-nostdin", "-i", "poster.webp", "-f", "null", "-"], project)
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    (project / "server.bt").write_text(f"""net.listen({{type:'web', bind:'127.0.0.1:{port}', sites:[{{domains:['127.0.0.1'], root:'.', entry:'http.bt'}}]}})
println('media-web-ready')
""", encoding="utf-8")
    (project / "http.bt").write_text("""web.header('Content-Type', 'application/json')
mode = web.get.mode
if mode == 'start' {
    source = video('@/source.mp4', {timeout_ms: 60000})
    job = source.resize('@/web-output.webm', 1920, 1080, {quality: 12})
    source.close()
    print json({id: job.id()})
} else if mode == 'poll' {
    source = video('@/source.mp4', {})
    job = source.job(int(web.get.id))
    source.close()
    print json(job.status())
} else if mode == 'close' {
    source = video('@/source.mp4', {})
    job = source.job(int(web.get.id))
    source.close()
    print json({closed: job.close()})
} else {
    print json({ok: true})
}
""", encoding="utf-8")
    log = (project / "server.log").open("w", encoding="utf-8")
    server = subprocess.Popen([str(bt), "server.bt"], cwd=project, stdout=log,
                              stderr=subprocess.STDOUT,
                              creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    url = f"http://127.0.0.1:{port}/"

    def request(**query):
        """Perform one real HTTP request with a bounded network timeout."""
        begin = time.perf_counter()
        with urllib.request.urlopen(url + "?" + urllib.parse.urlencode(query), timeout=5) as response:
            value = json.loads(response.read())
        return value, (time.perf_counter() - begin) * 1000

    job_id = None
    try:
        for _ in range(100):
            try:
                request()
                break
            except OSError:
                if server.poll() is not None:
                    raise RuntimeError((project / "server.log").read_text(encoding="utf-8"))
                time.sleep(0.1)
        else:
            raise RuntimeError("Web server did not become ready")
        created, submit_ms = request(mode="start")
        job_id = created["id"]
        polls = []
        for _ in range(100):
            state, elapsed = request(mode="poll", id=job_id)
            polls.append(elapsed)
            if state["state"] != "probing":
                break
            time.sleep(0.025)
        assert state["state"] == "running", state
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as clients:
            health = list(clients.map(lambda _: request(), range(40)))
        assert all(value["ok"] for value, _ in health)
        for _ in range(2400):
            state, elapsed = request(mode="poll", id=job_id)
            polls.append(elapsed)
            if state["state"] not in ("probing", "running"):
                break
            time.sleep(0.025)
        assert state["state"] == "succeeded", state
        request(mode="close", id=job_id)
        job_id = None
        latency = sorted(elapsed for _, elapsed in health)
        report = {"ok": True, "project": str(project), "backend": backend,
                  "packages": packages, "cross_media_ms": round(cross_ms, 3),
                  "cross_output": cross_output.strip(), "web_submit_ms": round(submit_ms, 3),
                  "web_health_requests": len(health),
                  "web_health_p50_ms": round(latency[len(latency)//2], 3),
                  "web_health_p95_ms": round(latency[int(len(latency)*0.95)-1], 3),
                  "web_health_max_ms": round(max(latency), 3),
                  "web_poll_max_ms": round(max(polls), 3), "web_result": state}
        run(["ffmpeg", "-v", "error", "-nostdin", "-i", "web-output.webm", "-f", "null", "-"], project)
        (project / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps(report, indent=2))
    finally:
        if job_id is not None and server.poll() is None:
            try:
                request(mode="close", id=job_id)
            except Exception:
                pass
        # Popen retains ownership of this exact process; never terminate by image name.
        if server.poll() is None:
            server.terminate()
        server.wait(timeout=10)
        log.close()


if __name__ == "__main__":
    main()
