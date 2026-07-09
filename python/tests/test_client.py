import json
import os
import subprocess
import threading
import traceback
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from ads import AdsCli, AdsCommandError, AdsHttpClient


class AdsCliTests(unittest.TestCase):
    def test_wip_add_builds_expected_command(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="registered wip seq=3 char/hero/model files=2 bytes=10 manifest=abc\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").wip_add(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                source="D:\\workspace\\.ads-staging\\run1\\char\\hero\\model",
            )

        self.assertIn("registered wip seq=3", text)
        run.assert_called_once()
        args = run.call_args.args[0]
        self.assertEqual(args[0], "ads.exe")
        self.assertEqual(args[1:3], ["wip", "add"])
        self.assertEqual(args[args.index("--category") + 1], "char")
        self.assertEqual(args[args.index("--asset-code") + 1], "hero")
        self.assertEqual(args[args.index("--department") + 1], "model")
        self.assertIn("--source", args)

    def test_publish_promote_accepts_integer_wip_seq(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="promoted v003 char/hero/model files=2 bytes=10 manifest=abc\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").publish_promote(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                wip_seq=7,
            )

        self.assertIn("promoted v003", text)
        args = run.call_args.args[0]
        self.assertEqual(args[1:3], ["publish", "promote"])
        self.assertEqual(args[args.index("--wip-seq") + 1], "7")

    def test_command_errors_never_contain_auth_tokens(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="",
            stderr="remote request failed",
        )
        with patch("ads.client.subprocess.run", return_value=completed):
            with self.assertRaises(AdsCommandError) as raised:
                AdsCli("ads.exe").fetch(
                    server="http://ads-server:8787",
                    auth_token="super-secret",
                    store="D:\\cache",
                    category="char",
                    asset_code="hero",
                    department="model",
                    latest=True,
                )

        error = raised.exception
        self.assertNotIn("super-secret", " ".join(error.args_list))
        self.assertNotIn("super-secret", str(error))

    def test_run_text_redacts_explicit_auth_token_args(self):
        # Defense in depth for direct run_text callers that still pass
        # --auth-token on the command line.
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=1,
            stdout="",
            stderr="remote request failed",
        )
        with patch("ads.client.subprocess.run", return_value=completed):
            with self.assertRaises(AdsCommandError) as raised:
                AdsCli("ads.exe").run_text(["push", "--auth-token", "super-secret"])

        error = raised.exception
        self.assertIn("***", error.args_list)
        self.assertNotIn("super-secret", " ".join(error.args_list))

    def test_add_with_source_auto_assigns_version(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="registered v004 char/hero/model files=1 bytes=10 manifest=abc\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe").add(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                source="D:\\delivery\\hero_model_fix",
            )

        args = run.call_args.args[0]
        self.assertEqual(args[:2], ["ads.exe", "add"])
        self.assertEqual(args[args.index("--source") + 1], "D:\\delivery\\hero_model_fix")
        self.assertNotIn("--version", args)
        self.assertNotIn("--workspace", args)

    def test_add_with_source_and_explicit_version(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="registered v002 char/hero/model files=1 bytes=10 manifest=abc\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe").add(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                version=2,
                source="D:\\delivery\\hero_model_fix",
            )

        args = run.call_args.args[0]
        self.assertEqual(args[args.index("--version") + 1], "2")
        self.assertEqual(args[args.index("--source") + 1], "D:\\delivery\\hero_model_fix")

    def test_add_legacy_workspace_form_requires_version(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="registered v003 char/hero/model files=1 bytes=10 manifest=abc\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe").add(
                store="D:\\store",
                workspace="D:\\workspace",
                category="char",
                asset_code="hero",
                department="model",
                version="v003",
            )

        args = run.call_args.args[0]
        self.assertEqual(args[args.index("--workspace") + 1], "D:\\workspace")
        self.assertEqual(args[args.index("--version") + 1], "v003")
        self.assertNotIn("--source", args)

        with self.assertRaises(ValueError):
            AdsCli("ads.exe").add(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
            )

    def test_timeout_defaults_from_environment(self):
        with patch.dict(os.environ, {"ADS_CLI_TIMEOUT_SECONDS": "12.5"}):
            self.assertEqual(AdsCli("ads.exe").timeout, 12.5)

        with patch.dict(os.environ):
            os.environ.pop("ADS_CLI_TIMEOUT_SECONDS", None)
            self.assertIsNone(AdsCli("ads.exe").timeout)

        self.assertEqual(AdsCli("ads.exe", timeout=3.0).timeout, 3.0)

    def test_timeout_is_passed_to_subprocess(self):
        completed = subprocess.CompletedProcess(args=[], returncode=0, stdout="ok\n", stderr="")
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe", timeout=7.5).verify(store="D:\\store")

        self.assertEqual(run.call_args.kwargs["timeout"], 7.5)

    def test_timeout_expiry_raises_ads_command_error_with_redacted_argv(self):
        # TimeoutExpired.cmd carries the unredacted argv, like the real one
        # subprocess.run raises with the full command line.
        expired = subprocess.TimeoutExpired(
            cmd=["ads.exe", "push", "--auth-token", "super-secret"], timeout=1.5
        )
        with patch("ads.client.subprocess.run", side_effect=expired):
            with self.assertRaises(AdsCommandError) as raised:
                AdsCli("ads.exe", timeout=1.5).run_text(
                    ["push", "--auth-token", "super-secret"]
                )

        error = raised.exception
        self.assertIsNone(error.returncode)
        self.assertIn("timed out after 1.5 seconds", str(error))
        self.assertIn("***", error.args_list)
        self.assertNotIn("super-secret", " ".join(error.args_list))
        # The chained TimeoutExpired would leak the token through its .cmd in
        # rendered tracebacks; the raise must sever both cause and context.
        rendered = "".join(
            traceback.format_exception(type(error), error, error.__traceback__)
        )
        self.assertNotIn("super-secret", rendered)

    def test_checkout_accepts_integer_version(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="checked out\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe").checkout(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                dest="D:\\temp\\out",
                version=2,
            )

        args = run.call_args.args[0]
        self.assertEqual(args[args.index("--version") + 1], "2")

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

    def test_publish_validate_builds_store_and_source_commands(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="ok files_scanned=1 references_checked=2 warnings=0\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").publish_validate(
                store="D:\\store",
                category="char",
                asset_code="hero",
                department="model",
                wip=True,
            )
        self.assertIn("references_checked=2", text)
        args = run.call_args.args[0]
        self.assertEqual(args[1:3], ["publish", "validate"])
        self.assertIn("--wip", args)

        with patch("ads.client.subprocess.run", return_value=completed) as run:
            AdsCli("ads.exe").publish_validate(source="D:\\staging\\run1")
        args = run.call_args.args[0]
        self.assertEqual(args[args.index("--source") + 1], "D:\\staging\\run1")
        self.assertNotIn("--store", args)

        with self.assertRaises(ValueError):
            AdsCli("ads.exe").publish_validate(category="char")

    def test_fetch_builds_expected_command(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="fetched v003 char/hero/model objects_downloaded=1 objects_reused=0 bytes_downloaded=10\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").fetch(
                server="http://ads-server:8787",
                auth_token="secret",
                profile="main",
                store="D:\\cache",
                workspace="D:\\workspace",
                category="char",
                asset_code="hero",
                department="model",
                version="v003",
                materialize=True,
                force=True,
            )

        self.assertIn("fetched v003", text)
        args = run.call_args.args[0]
        self.assertEqual(args[:2], ["ads.exe", "fetch"])
        self.assertEqual(args[args.index("--server") + 1], "http://ads-server:8787")
        self.assertIn("--materialize", args)
        self.assertIn("--force", args)
        # The token travels via the environment, never argv (process listings).
        self.assertNotIn("--auth-token", args)
        self.assertNotIn("secret", args)
        self.assertEqual(run.call_args.kwargs["env"]["ADS_WEB_TOKEN"], "secret")

    def test_sync_builds_expected_command(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="synced assets=1 versions=1 objects_downloaded=1 objects_reused=0 bytes_downloaded=10 materialized=0\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").sync(
                server="http://ads-server:8787",
                auth_token="secret",
                profile="main",
                store="D:\\cache",
                category="char",
                asset_code="hero",
                department="model",
                all_versions=True,
            )

        self.assertIn("synced assets=1", text)
        args = run.call_args.args[0]
        self.assertEqual(args[:2], ["ads.exe", "sync"])
        self.assertIn("--all-versions", args)
        self.assertEqual(args[args.index("--category") + 1], "char")
        self.assertNotIn("--auth-token", args)
        self.assertNotIn("secret", args)
        self.assertEqual(run.call_args.kwargs["env"]["ADS_WEB_TOKEN"], "secret")

    def test_push_builds_expected_command(self):
        completed = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout="pushed v003 char/hero/model objects_uploaded=1 objects_reused=0 bytes_uploaded=10 thumbnails_pushed=1\n",
            stderr="",
        )
        with patch("ads.client.subprocess.run", return_value=completed) as run:
            text = AdsCli("ads.exe").push(
                server="http://ads-server:8787",
                auth_token="secret",
                profile="main",
                store="D:\\cache",
                category="char",
                asset_code="hero",
                department="model",
                version="v003",
                set_current=True,
            )

        self.assertIn("pushed v003", text)
        args = run.call_args.args[0]
        self.assertEqual(args[:2], ["ads.exe", "push"])
        self.assertIn("--set-current", args)
        self.assertEqual(args[args.index("--version") + 1], "v003")
        self.assertNotIn("--auth-token", args)
        self.assertNotIn("secret", args)
        self.assertEqual(run.call_args.kwargs["env"]["ADS_WEB_TOKEN"], "secret")

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
        elif self.path.startswith("/api/wips"):
            self._json(
                {
                    "wips": [
                        {
                            "seq": 3,
                            "manifest_hash": "b" * 64,
                            "created_at": "2026-07-03T00:00:00Z",
                            "source_path": "char/hero/model",
                            "file_count": 2,
                            "total_bytes": 10,
                        }
                    ]
                }
            )
        elif self.path.startswith("/api/version"):
            self._json(
                {
                    "version": {"version": "v002"},
                    "manifest": {"entries": [{"relative_path": "crate.usd", "sha256": "a" * 64}]},
                }
            )
        elif self.path.startswith("/api/object/status"):
            self._json({"sha256": "a" * 64, "exists": True})
        elif self.path.startswith("/api/object"):
            self._bytes(b"object-bytes")
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
        elif self.path == "/api/wip":
            payload = json.loads(body.decode("utf-8"))
            self._json(
                {
                    "created": True,
                    "seq": 4,
                    "manifest_hash": "b" * 64,
                    "file_count": len(payload["manifest"]["entries"]),
                    "total_bytes": sum(
                        entry["size"] for entry in payload["manifest"]["entries"]
                    ),
                }
            )
        elif self.path == "/api/promote":
            payload = json.loads(body.decode("utf-8"))
            self._json(
                {
                    "outcome": {
                        "version": 3,
                        "created": True,
                        "manifest_hash": "c" * 64,
                        "file_count": 2,
                        "total_bytes": 10,
                    },
                    "validation": {"ok": True, "errors": [], "warnings": []},
                    "wip_seq": payload.get("wip_seq"),
                }
            )
        elif self.path == "/api/gc":
            payload = json.loads(body.decode("utf-8"))
            self._json(
                {
                    "dry_run": bool(payload.get("dry_run")),
                    "wip_removed": 0,
                    "objects_removed": 0,
                }
            )
        else:
            self.send_error(404)

    def do_PUT(self):  # noqa: N802
        body = self._body()
        self._record(body)
        if self.path.startswith("/api/object"):
            self._json({"sha256": "a" * 64, "size": len(body), "reused": False})
        elif self.path == "/api/version":
            payload = json.loads(body.decode("utf-8"))
            self._json(payload["version_info"]["version"])
        elif self.path == "/api/thumbnail":
            payload = json.loads(body.decode("utf-8"))
            self._json(payload["thumbnail"])
        elif self.path == "/api/current":
            payload = json.loads(body.decode("utf-8"))
            self._json({"current": payload.get("version"), "explicit": not payload.get("reset", False)})
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

    def _bytes(self, value):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(value)))
        self.end_headers()
        self.wfile.write(value)

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

    def test_repr_hides_bearer_token(self):
        self.assertNotIn("secret", repr(self.client))

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

    def test_version_info_gets_manifest(self):
        response = self.client.version_info(
            profile="main",
            category="prop",
            asset_code="crate",
            department="model",
            version="v002",
        )

        self.assertEqual(response["version"]["version"], "v002")
        self.assertEqual(response["manifest"]["entries"][0]["relative_path"], "crate.usd")
        request = _Handler.requests[-1]
        self.assertIn("/api/version", request["path"])
        self.assertIn("version=v002", request["path"])

    def test_object_bytes_downloads_raw_bytes(self):
        data = self.client.object_bytes("a" * 64, profile="main")

        self.assertEqual(data, b"object-bytes")
        request = _Handler.requests[-1]
        self.assertIn("/api/object", request["path"])
        self.assertIn("sha256=", request["path"])

    def test_object_status_upload_and_import_version(self):
        status = self.client.object_status("a" * 64, profile="main", size=12)
        self.assertTrue(status["exists"])
        request = _Handler.requests[-1]
        self.assertIn("/api/object/status", request["path"])
        self.assertIn("size=12", request["path"])

        upload = self.client.upload_object("a" * 64, b"new-object", profile="main")
        self.assertFalse(upload["reused"])
        request = _Handler.requests[-1]
        self.assertEqual(request["content_type"], "application/octet-stream")
        self.assertEqual(request["body"], b"new-object")

        version = {
            "version": {"version": "v001", "department_key": {"department": "model"}},
            "manifest": {"entries": []},
        }
        imported = self.client.import_version_info(version, profile="main")
        self.assertEqual(imported["version"], "v001")
        request = _Handler.requests[-1]
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["profile"], "main")

        thumbnail = {"version": "v001", "sha256": "a" * 64, "mime_type": "image/png"}
        imported_thumbnail = self.client.import_thumbnail_info(thumbnail, profile="main")
        self.assertEqual(imported_thumbnail["mime_type"], "image/png")
        request = _Handler.requests[-1]
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["thumbnail"]["version"], "v001")

    def test_wips_sends_department_query(self):
        response = self.client.wips(
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
        )

        self.assertEqual(response["wips"][0]["seq"], 3)
        self.assertEqual(response["wips"][0]["source_path"], "char/hero/model")
        request = _Handler.requests[-1]
        self.assertEqual(request["authorization"], "Bearer secret")
        self.assertIn("/api/wips", request["path"])
        self.assertIn("asset_code=hero", request["path"])
        self.assertIn("department=model", request["path"])

    def test_wip_import_posts_manifest(self):
        manifest = {
            "entries": [
                {
                    "relative_path": "hero.usd",
                    "sha256": "a" * 64,
                    "size": 10,
                    "mode": 33188,
                }
            ]
        }
        response = self.client.wip_import(
            manifest,
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
            source_path="D:/workspace/char/hero/model",
        )

        self.assertTrue(response["created"])
        self.assertEqual(response["seq"], 4)
        self.assertEqual(response["total_bytes"], 10)
        request = _Handler.requests[-1]
        self.assertEqual(request["authorization"], "Bearer secret")
        self.assertEqual(request["content_type"], "application/json")
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["category"], "char")
        self.assertEqual(payload["manifest"]["entries"][0]["sha256"], "a" * 64)
        self.assertEqual(payload["source_path"], "D:/workspace/char/hero/model")

    def test_wip_import_sorts_entries_into_canonical_order(self):
        # The server rejects manifests not strictly ascending by
        # relative_path; callers building entries in NTFS listing order
        # (case-insensitive, so "a.usd" before "B.usd") must not get a 400.
        manifest = {
            "entries": [
                {"relative_path": "a.usd", "sha256": "a" * 64, "size": 1, "mode": 33188},
                {"relative_path": "B.usd", "sha256": "b" * 64, "size": 2, "mode": 33188},
            ]
        }
        self.client.wip_import(
            manifest,
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
        )

        payload = json.loads(_Handler.requests[-1]["body"].decode("utf-8"))
        self.assertEqual(
            [entry["relative_path"] for entry in payload["manifest"]["entries"]],
            ["B.usd", "a.usd"],
        )
        # The caller's manifest is not mutated in place.
        self.assertEqual(manifest["entries"][0]["relative_path"], "a.usd")

    def test_promote_posts_wip_seq(self):
        response = self.client.promote(
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
            wip_seq=7,
        )

        self.assertEqual(response["outcome"]["version"], 3)
        self.assertEqual(response["wip_seq"], 7)
        request = _Handler.requests[-1]
        self.assertEqual(request["authorization"], "Bearer secret")
        self.assertEqual(request["content_type"], "application/json")
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["wip_seq"], 7)
        self.assertNotIn("no_validate", payload)

    def test_gc_posts_dry_run(self):
        response = self.client.gc(profile="main", retention=10, dry_run=True)

        self.assertTrue(response["dry_run"])
        request = _Handler.requests[-1]
        payload = json.loads(request["body"].decode("utf-8"))
        self.assertEqual(payload["profile"], "main")
        self.assertEqual(payload["retention"], 10)
        self.assertTrue(payload["dry_run"])

    def test_wip_import_omits_default_source_path(self):
        response = self.client.wip_import(
            {"entries": []},
            profile="main",
            category="char",
            asset_code="hero",
            department="model",
        )

        self.assertTrue(response["created"])
        payload = json.loads(_Handler.requests[-1]["body"].decode("utf-8"))
        # Absent (not null) so the server defaults it to the logical work
        # line, matching local `wip add`.
        self.assertNotIn("source_path", payload)

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
