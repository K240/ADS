import json
import subprocess
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from ads import AdsCli, AdsCommandError, AdsHttpClient


class AdsCliTests(unittest.TestCase):
    def test_pull_builds_expected_command(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="pulled v002 char/hero/model to D:\\workspace\\char\\hero\\model\\v002\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").pull(
                store="D:\\store",
                workspace="D:\\workspace",
                category="char",
                asset_code="hero",
                department="model",
                latest=True,
                force=True,
            )

        self.assertIn("pulled v002", text)
        run.assert_called_once()
        args = run.call_args.args[0]
        self.assertEqual(args[0], "ads.exe")
        self.assertIn("pull", args)
        self.assertIn("--latest", args)
        self.assertIn("--force", args)
        self.assertEqual(args[args.index("--category") + 1], "char")
        self.assertEqual(args[args.index("--asset-code") + 1], "hero")
        self.assertEqual(args[args.index("--department") + 1], "model")

    def test_run_json_parses_cli_json(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout='{"asset":{"asset_key":{"category":"char","asset_code":"hero"}}}\n',
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed):
            data = AdsCli("ads.exe").asset_log(
                store="D:\\store",
                category="char",
                asset_code="hero",
            )

        self.assertEqual(data["asset"]["asset_key"]["asset_code"], "hero")

    def test_nonzero_cli_exit_raises(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=2,
            stdout="",
            stderr="missing required argument",
        )
        with patch("ads.client.subprocess.run", return_value=completed):
            with self.assertRaises(AdsCommandError) as raised:
                AdsCli("ads.exe").verify(store="D:\\missing")

        self.assertEqual(raised.exception.returncode, 2)
        self.assertIn("missing required argument", str(raised.exception))


class _Handler(BaseHTTPRequestHandler):
    requests = []

    def do_GET(self):  # noqa: N802
        self._record(b"")
        if self.path.startswith("/api/assets"):
            self._json({"assets": []})
        elif self.path.startswith("/api/thumbnail-url"):
            self._json("https://assets.example.com/objects/sha256/ab/hash")
        else:
            self.send_error(404)

    def do_POST(self):  # noqa: N802
        body = self._body()
        self._record(body)
        if self.path == "/api/pull":
            payload = json.loads(body.decode("utf-8"))
            self._json({"version": payload.get("version", "v001"), "unchanged": True})
        else:
            self.send_error(404)

    def _record(self, body):
        self.__class__.requests.append(
            {
                "path": self.path,
                "authorization": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type"),
                "body": body,
            }
        )

    def _body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length) if length else b""

    def _json(self, value):
        raw = json.dumps(value).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_args):
        return


class AdsHttpClientTests(unittest.TestCase):
    def setUp(self):
        _Handler.requests = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.client = AdsHttpClient(f"http://{host}:{port}", token="secret")

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def test_assets_sends_bearer_token_and_query(self):
        response = self.client.assets(profile="main", category="char", department="model")

        self.assertEqual(response, {"assets": []})
        request = _Handler.requests[-1]
        self.assertEqual(request["authorization"], "Bearer secret")
        self.assertIn("profile=main", request["path"])
        self.assertIn("category=char", request["path"])
        self.assertIn("department=model", request["path"])

    def test_pull_posts_json(self):
        response = self.client.pull(
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
            version="v002",
            force=True,
        )

        self.assertEqual(response["version"], "v002")
        request = _Handler.requests[-1]
        self.assertEqual(request["authorization"], "Bearer secret")
        self.assertEqual(request["content_type"], "application/json")
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["asset_code"], "hero")
        self.assertEqual(payload["version"], "v002")
        self.assertTrue(payload["force"])

    def test_thumbnail_url_accepts_json_string_response(self):
        url = self.client.thumbnail_url(
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
            version="v001",
        )

        self.assertEqual(url, "https://assets.example.com/objects/sha256/ab/hash")


if __name__ == "__main__":
    unittest.main()
