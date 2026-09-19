"""Persistent ReNikudPlus HTTP sidecar. The ONNX model is loaded exactly once."""

import importlib.util
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MODEL_PATH = Path(os.environ.get("RENIKUD_MODEL_PATH", "/models/model_int8.onnx"))
WRAPPER_PATH = Path(os.environ.get("RENIKUD_WRAPPER_PATH", "/models/renikud_onnx.py"))
PORT = int(os.environ.get("RENIKUD_PORT", "8787"))


def load_model():
    spec = importlib.util.spec_from_file_location("renikud_plus", WRAPPER_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Cannot import ReNikudPlus wrapper from {WRAPPER_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.G2P(str(MODEL_PATH))


G2P_MODEL = load_model()
GENDER = {"male": 1, "female": 2}


class Handler(BaseHTTPRequestHandler):
    server_version = "callora-renikud/1"

    def log_message(self, _format, *_args):
        # Never put customer text into access logs.
        return

    def send_json(self, status, payload):
        encoded = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json; charset=utf-8")
        self.send_header("content-length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self):
        if self.path == "/health":
            self.send_json(200, {"ok": True})
        else:
            self.send_json(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/pronounce":
            self.send_json(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("content-length", "0"))
            if length <= 0 or length > 65536:
                raise ValueError("invalid body size")
            body = json.loads(self.rfile.read(length))
            text = body.get("text")
            if not isinstance(text, str) or not text.strip() or len(text) > 2000:
                raise ValueError("text must be a non-empty string of at most 2000 characters")
            started = time.perf_counter()
            pronunciation = G2P_MODEL.phonemize(
                text,
                speaker=GENDER.get(body.get("speakerGender"), 0),
                target_speaker=GENDER.get(body.get("targetGender"), 0),
            )
            elapsed = round((time.perf_counter() - started) * 1000, 3)
            self.send_json(200, {"pronunciation": pronunciation, "processingMs": elapsed})
        except (ValueError, json.JSONDecodeError) as error:
            self.send_json(400, {"error": str(error)})
        except Exception:
            self.send_json(500, {"error": "pronunciation failed"})


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()

