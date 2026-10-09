"""Verify request cancellation, worker recovery and output limits on an isolated BT server."""

import argparse
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


class SlowHandler(http.server.BaseHTTPRequestHandler):
    """Provide a local slow upstream without external network dependencies."""

    def do_GET(self):
        """Delay the upstream response beyond BT's request deadline."""
        time.sleep(1)
        try:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"late upstream response")
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    def log_message(self, *_args):
        """Keep expected upstream cancellation out of the acceptance output."""


def request(url):
    """Capture both successful and failing HTTP responses with a bounded client wait."""
    start = time.perf_counter()
    try:
        response = urllib.request.urlopen(url, timeout=4)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return {
            "status": response.status,
            "body": response.read().decode("utf-8"),
            "elapsed_ms": round((time.perf_counter() - start) * 1000, 3),
        }


def main():
    """Run bounded real-HTTP regressions and clean up only the processes we created."""
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bt", type=Path, default=repo / "target/debug" / ("bt.exe" if os.name == "nt" else "bt"))
    parser.add_argument("--output", type=Path, default=repo / "target/quality/web-deadlines.json")
    args = parser.parse_args()
    exe = args.bt.resolve(strict=True)
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SlowHandler)
    upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    upstream_thread.start()
    results = {}
    try:
        with tempfile.TemporaryDirectory(prefix="bt-web-deadlines-") as temporary:
            root = Path(temporary)
            www = root / "www"
            www.mkdir()
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            (root / "main.bt").write_text(
                "net.listen({type:'web',bind:'127.0.0.1:" + str(port)
                + "',sites:[{domains:['127.0.0.1'],root:'www/',entry:'main.bt',upload:{temp:'temp/'}}]})",
                encoding="utf-8",
            )
            (www / "main.bt").write_text(
                "mode = web.get.mode\n"
                "if mode == 'loop' { while true {} }\n"
                "if mode == 'sleep' { for i in 100 { sleep(50) } }\n"
                "if mode == 'io' { reqwest('http://127.0.0.1:"
                + str(upstream.server_port) + "/').timeout(5000).send() }\n"
                "if mode == 'output' { for i in 200 { print '0123456789' } }\n"
                "if mode != empty { fs(mode + '.completed').write('unexpected continuation') }\n"
                "print 'ok'\n",
                encoding="utf-8",
            )
            env = os.environ.copy()
            env.update(BT_IO_TIMEOUT_MS="200", BT_IO_BLOCKING_WORKERS="1", BT_WEB_RESPONSE_BODY_LIMIT="1024")
            with (root / "server.log").open("w+", encoding="utf-8") as log:
                process = subprocess.Popen(
                    [str(exe), str(root / "main.bt")], cwd=root, env=env,
                    stdout=log, stderr=log,
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                )
                try:
                    url = f"http://127.0.0.1:{port}/"
                    ready = False
                    for _ in range(100):
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError("BT exited before readiness: " + log.read())
                        try:
                            if request(url)["body"] == "ok":
                                ready = True
                                break
                        except OSError:
                            time.sleep(0.05)
                    if not ready:
                        raise RuntimeError("BT server readiness timed out")
                    for mode in ("loop", "sleep", "io", "output"):
                        failed = request(url + "?mode=" + mode)
                        assert failed["status"] == 500, (mode, failed)
                        expected = "1024-byte limit" if mode == "output" else "exceed"
                        assert expected in failed["body"], (mode, failed)
                        assert failed["elapsed_ms"] < 2000, (mode, failed)
                        recovered = request(url)
                        assert recovered["status"] == 200 and recovered["body"] == "ok", (mode, recovered)
                        assert not (www / (mode + ".completed")).exists(), mode
                        results[mode] = {"rejected": failed, "next_request": recovered}
                finally:
                    if process.poll() is None:
                        process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
    finally:
        upstream.shutdown()
        upstream.server_close()
        upstream_thread.join(timeout=2)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(results, indent=2), encoding="utf-8")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
