import importlib.util
import json
import sys
import threading
import types
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from ads.houdini_catalog import CatalogIndex, asset_uri, parse_datasource_args


ASSET = {
    "category": "show/char",
    "asset_code": "hero",
    "department": "model",
    "current": "v001",
    "latest": "v002",
    "explicit_current": True,
    "version_count": 2,
    "latest_created_at": "2026-05-28T00:00:00+00:00",
    "latest_file_count": 3,
    "latest_total_bytes": 123,
}


class CatalogConfigTests(unittest.TestCase):
    def test_parse_datasource_args_prefers_args_and_token_env(self):
        env = {
            "ADS_CATALOG_SERVER": "http://catalog",
            "ADS_CATALOG_PROFILE": "env-profile",
            "ADS_CATALOG_API_TOKEN": "env-token",
            "ADS_CATALOG_RESOLVE_MODE": "remote",
            "ADS_TEST_TOKEN": "secret",
        }

        config = parse_datasource_args(
            "server=http://arg;profile=main&category=show/char;"
            "department=model;token_env=ADS_TEST_TOKEN;resolve_mode=local;timeout=12.5",
            env,
        )

        self.assertEqual(config.server, "http://arg")
        self.assertEqual(config.profile, "main")
        self.assertEqual(config.token, "secret")
        self.assertEqual(config.category, "show/char")
        self.assertEqual(config.department, "model")
        self.assertEqual(config.resolve_mode, "local")
        self.assertEqual(config.timeout, 12.5)

    def test_parse_datasource_args_uses_env_fallbacks(self):
        config = parse_datasource_args(
            None,
            {
                "ADS_RESOLVER_SERVER": "http://resolver",
                "ADS_RESOLVER_PROFILE": "phase3",
                "ADS_WEB_TOKEN": "web-token",
                "ADS_RESOLVER_MODE": "remote",
            },
        )

        self.assertEqual(config.server, "http://resolver")
        self.assertEqual(config.profile, "phase3")
        self.assertEqual(config.token, "web-token")
        self.assertEqual(config.resolve_mode, "remote")


class CatalogIndexTests(unittest.TestCase):
    def test_builds_nested_category_tree_and_leaf_ids(self):
        index = CatalogIndex(
            [
                ASSET,
                {
                    **ASSET,
                    "department": "anim",
                    "current": "v003",
                    "latest": "v003",
                },
            ]
        )

        self.assertEqual(index.item_ids(), ["ads:asset:show/char/hero/anim", "ads:asset:show/char/hero/model"])
        self.assertEqual(index.child_item_ids(""), ["ads:folder:category:show"])
        self.assertEqual(index.child_item_ids("ads:folder:category:show"), ["ads:folder:category:show/char"])
        self.assertEqual(
            index.child_item_ids("ads:folder:category:show/char"),
            ["ads:asset:show/char/hero/anim", "ads:asset:show/char/hero/model"],
        )
        self.assertEqual(index.parent_id("ads:asset:show/char/hero/model"), "ads:folder:category:show/char")
        self.assertEqual(index.type_name("ads:folder:category:show"), "folder")
        self.assertEqual(index.type_name("ads:asset:show/char/hero/model"), "asset")

    def test_leaf_metadata_file_path_tags_status_and_blind_data(self):
        index = CatalogIndex([ASSET])
        item_id = "ads:asset:show/char/hero/model"

        self.assertEqual(asset_uri(ASSET), "ads://show/char/hero/model/hero.usd")
        self.assertEqual(index.file_path(item_id), "ads://show/char/hero/model/hero.usd")
        self.assertEqual(index.tags(item_id), ["show", "char", "hero", "model"])
        self.assertEqual(index.status(item_id), "current v001 / latest v002")
        self.assertEqual(index.modification_time(item_id), 1779926400)

        metadata = index.metadata(item_id)
        self.assertEqual(metadata["category"], "show/char")
        self.assertEqual(metadata["asset_code"], "hero")
        self.assertEqual(metadata["department"], "model")
        self.assertEqual(metadata["ads_uri"], "ads://show/char/hero/model/hero.usd")

        blind_data = json.loads(index.blind_data(item_id).decode("ascii"))
        self.assertEqual(blind_data["name"], "hero")
        self.assertEqual(blind_data["refprimpath"], "")
        self.assertEqual(blind_data["ads_uri"], "ads://show/char/hero/model/hero.usd")


class _CatalogHandler(BaseHTTPRequestHandler):
    thumb_requests = 0
    resolve_requests = 0

    def do_GET(self):  # noqa: N802
        if self.path.startswith("/api/assets"):
            base = "http://{}:{}".format(*self.server.server_address)
            self._json({"assets": [{**ASSET, "thumbnail_url": base + "/thumb.png"}]})
        elif self.path.startswith("/api/resolve"):
            self.__class__.resolve_requests += 1
            self._json({"location": "ads://show/char/hero/model/hero.usd", "source": "remote"})
        elif self.path == "/thumb.png":
            self.__class__.thumb_requests += 1
            self._bytes(b"thumbnail-bytes")
        else:
            self.send_error(404)

    def _json(self, value):
        raw = json.dumps(value).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _bytes(self, value):
        self.send_response(200)
        self.send_header("Content-Type", "image/png")
        self.send_header("Content-Length", str(len(value)))
        self.end_headers()
        self.wfile.write(value)

    def log_message(self, *_args):
        return


class HoudiniDataSourcePluginTests(unittest.TestCase):
    def setUp(self):
        _CatalogHandler.thumb_requests = 0
        _CatalogHandler.resolve_requests = 0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _CatalogHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.base_url = f"http://{host}:{port}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def test_datasource_loads_assets_resolves_and_caches_thumbnail(self):
        module = _load_ads_datasource_module()
        with patch.dict("os.environ", {"ADS_TEST_TOKEN": "secret"}):
            datasource = module.AdsDataSource(
                "ADS",
                f"server={self.base_url};profile=main;token_env=ADS_TEST_TOKEN",
            )

        item_id = "ads:asset:show/char/hero/model"
        self.assertTrue(datasource.isValid())
        self.assertEqual(datasource.itemIds(), [item_id])
        self.assertEqual(datasource.label(item_id), "hero / model")
        self.assertEqual(datasource.sourceTypeName(item_id), "sidefx_usd_file_source")
        self.assertEqual(datasource.sourceTypeName(), "sidefx_usd_file_source")
        self.assertEqual(datasource.filePath(item_id), "ads://show/char/hero/model/hero.usd")
        self.assertEqual(datasource.prepareItemForUse(item_id), "")
        self.assertEqual(_CatalogHandler.resolve_requests, 1)

        self.assertEqual(datasource.thumbnail(item_id), b"thumbnail-bytes")
        self.assertEqual(datasource.thumbnail(item_id), b"thumbnail-bytes")
        self.assertEqual(_CatalogHandler.thumb_requests, 1)

        blind_data = json.loads(datasource.blindData(item_id).decode("ascii"))
        self.assertEqual(blind_data["ads_uri"], "ads://show/char/hero/model/hero.usd")


def _load_ads_datasource_module():
    class _BaseDataSource:
        def __init__(self, source_identifier, args):
            self._sourceidentifier = source_identifier
            self._args = args

    hou = types.ModuleType("hou")
    husd = types.ModuleType("husd")
    husd.datasource = types.SimpleNamespace(DataSource=_BaseDataSource)
    plugin_path = Path(__file__).resolve().parents[2] / "houdini" / "husdplugins" / "datasources" / "ads.py"
    spec = importlib.util.spec_from_file_location("ads_houdini_datasource_test", plugin_path)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {"hou": hou, "husd": husd}):
        spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()
