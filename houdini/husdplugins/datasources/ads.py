"""Houdini Asset Gallery datasource for ADS assets."""

from __future__ import annotations

import html
import urllib.error
import urllib.request

import hou
import husd

from ads.client import AdsHttpClient
from ads.houdini_catalog import CatalogConfig, CatalogIndex, asset_uri, parse_datasource_args


class AdsDataSource(husd.datasource.DataSource):
    _SOURCE_NAMES = ("ADS", "ads://catalog")

    def __init__(self, source_identifier, args):
        super(AdsDataSource, self).__init__(source_identifier, args)
        self._config = None
        self._client = None
        self._index = CatalogIndex([])
        self._thumbnail_cache = {}
        self._last_error = ""
        self._load(args)

    @staticmethod
    def canHandle(source_identifier):
        return str(source_identifier).strip() in AdsDataSource._SOURCE_NAMES

    def isValid(self):
        return self._client is not None and not self._last_error

    def isReadOnly(self):
        return True

    def infoHtml(self):
        if self._config is None:
            detail = self._last_error or "ADS catalog is not configured."
        else:
            filters = []
            if self._config.q:
                filters.append("q={}".format(self._config.q))
            if self._config.category:
                filters.append("category={}".format(self._config.category))
            if self._config.department:
                filters.append("department={}".format(self._config.department))
            detail = "Server: {}<br/>Profile: {}<br/>Items: {}".format(
                html.escape(self._config.server),
                html.escape(self._config.profile),
                len(self._index.item_ids()),
            )
            if filters:
                detail += "<br/>Filters: {}".format(html.escape(", ".join(filters)))
            if self._last_error:
                detail += "<br/>Error: {}".format(html.escape(self._last_error))
        return """
            <html>
                <body>
                    <p align="center">ADS Asset Catalog</p>
                    <p>{}</p>
                </body>
            </html>""".format(detail)

    def itemIds(self):
        return self._index.item_ids()

    def updatedItemIds(self):
        return []

    def childItemIds(self, item_id):
        return self._index.child_item_ids(item_id)

    def parentId(self, item_id):
        return self._index.parent_id(item_id)

    def sourceTypeName(self, item_id=None):
        return "sidefx_usd_file_source"

    def typeName(self, item_id):
        return self._index.type_name(item_id) or "asset"

    def label(self, item_id):
        return self._index.label(item_id)

    def thumbnail(self, item_id):
        asset = self._index.asset(item_id)
        if not asset:
            return None
        url = asset.get("thumbnail_url")
        if not url:
            return None
        if url in self._thumbnail_cache:
            return self._thumbnail_cache[url]
        try:
            request = urllib.request.Request(str(url), headers=self._thumbnail_headers(str(url)))
            with urllib.request.urlopen(request, timeout=self._config.timeout) as response:
                data = response.read()
        except (OSError, urllib.error.URLError, urllib.error.HTTPError):
            return None
        self._thumbnail_cache[url] = data
        return data

    def creationDate(self, item_id):
        return 0

    def modificationDate(self, item_id):
        return self._index.modification_time(item_id)

    def isStarred(self, item_id):
        return False

    def colorTag(self, item_id):
        return ""

    def tags(self, item_id):
        return self._index.tags(item_id)

    def metadata(self, item_id):
        return self._index.metadata(item_id)

    def filePath(self, item_id):
        return self._index.file_path(item_id)

    def ownsFile(self, item_id):
        return False

    def blindData(self, item_id):
        return self._index.blind_data(item_id)

    def status(self, item_id):
        return self._index.status(item_id)

    def prepareItemForUse(self, item_id):
        asset = self._index.asset(item_id)
        if not asset:
            return ""
        if self._client is None or self._config is None:
            return self._last_error or "ADS catalog is not connected."
        try:
            self._client.resolve(
                asset_uri(asset),
                profile=self._config.profile,
                mode=self._config.resolve_mode,
            )
        except Exception as error:
            return "ADS resolve failed: {}".format(error)
        return ""

    def _load(self, args):
        try:
            self._config = parse_datasource_args(args)
            self._validate_config(self._config)
            self._client = AdsHttpClient(
                self._config.server,
                token=self._config.token,
                timeout=self._config.timeout,
            )
            response = self._client.assets(
                profile=self._config.profile,
                q=self._config.q,
                category=self._config.category,
                department=self._config.department,
            )
            self._index = CatalogIndex.from_response(response)
        except Exception as error:
            self._client = None
            self._index = CatalogIndex([])
            self._last_error = str(error)

    def _thumbnail_headers(self, url):
        if self._config is None:
            return {}
        server = self._config.server.rstrip("/") + "/"
        if url.startswith(server):
            return {"Authorization": "Bearer {}".format(self._config.token)}
        return {}

    @staticmethod
    def _validate_config(config: CatalogConfig):
        if not config.server:
            raise ValueError("ADS catalog server is not configured.")
        if not config.token:
            raise ValueError("ADS catalog API token is not configured.")


def registerDataSources(manager):
    manager.registerDataSource("ADS", AdsDataSource)
