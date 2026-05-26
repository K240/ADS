from __future__ import annotations

import json
import mimetypes
import os
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, MutableMapping, Sequence
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen


JsonObject = dict[str, Any]


class AdsCommandError(RuntimeError):
    """Raised when the local ADS CLI exits with a non-zero status."""

    def __init__(
        self,
        args: Sequence[str],
        returncode: int,
        stdout: str,
        stderr: str,
    ) -> None:
        self.args_list = list(args)
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr
        message = stderr.strip() or stdout.strip() or f"ads exited with {returncode}"
        super().__init__(message)


class AdsHttpError(RuntimeError):
    """Raised when an ADS HTTP API request fails."""

    def __init__(self, status: int, message: str, body: bytes = b"") -> None:
        self.status = status
        self.body = body
        super().__init__(message)


@dataclass(frozen=True)
class AdsCli:
    """Thin local API around the `ads` command.

    This class is intentionally subprocess based so it works inside Houdini
    Python without compiling a native extension module.
    """

    executable: str | os.PathLike[str] = "ads"

    def init(
        self,
        store: str | os.PathLike[str],
        *,
        remote_base_url: str | None = None,
    ) -> str:
        args = ["init", _path(store)]
        if remote_base_url:
            args += ["--remote-base-url", remote_base_url]
        return self.run_text(args)

    def asset_create(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
    ) -> str:
        return self.run_text(
            [
                "asset",
                "create",
                "--store",
                _path(store),
                *_workspace_args(workspace),
                "--category",
                category,
                "--asset-code",
                asset_code,
            ]
        )

    def asset_log(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
    ) -> JsonObject:
        return self.run_json(
            [
                "asset",
                "log",
                "--store",
                _path(store),
                "--category",
                category,
                "--asset-code",
                asset_code,
            ]
        )

    def new_version(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
        department: str,
    ) -> str:
        return self.run_text(
            [
                "new-version",
                "--store",
                _path(store),
                *_workspace_args(workspace),
                "--category",
                category,
                "--asset-code",
                asset_code,
                "--department",
                department,
            ]
        )

    def add(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> str:
        return self.run_text(
            [
                "add",
                "--store",
                _path(store),
                *_workspace_args(workspace),
                "--category",
                category,
                "--asset-code",
                asset_code,
                "--department",
                department,
                "--version",
                version,
            ]
        )

    def info(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str | None = None,
        version: str | None = None,
    ) -> JsonObject:
        args = [
            "info",
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
        ]
        if department:
            args += ["--department", department]
        if version:
            args += ["--version", version]
        return self.run_json(args)

    def pull(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
        department: str,
        latest: bool = False,
        force: bool = False,
    ) -> str:
        args = [
            "pull",
            "--store",
            _path(store),
            *_workspace_args(workspace),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if latest:
            args.append("--latest")
        if force:
            args.append("--force")
        return self.run_text(args)

    def restore(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
        department: str,
        version: str,
        force: bool = False,
    ) -> str:
        args = [
            "restore",
            "--store",
            _path(store),
            *_workspace_args(workspace),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
            "--version",
            version,
        ]
        if force:
            args.append("--force")
        return self.run_text(args)

    def publish_register(
        self,
        *,
        store: str | os.PathLike[str],
        public_root: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> str:
        return self.run_text(
            _publish_args(
                "register",
                store,
                public_root,
                category,
                asset_code,
                department,
                version,
            )
        )

    def publish_validate(
        self,
        *,
        store: str | os.PathLike[str],
        public_root: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> str:
        return self.run_text(
            _publish_args(
                "validate",
                store,
                public_root,
                category,
                asset_code,
                department,
                version,
            )
        )

    def fetch(
        self,
        *,
        server: str,
        auth_token: str,
        store: str | os.PathLike[str],
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str | None = None,
        latest: bool = False,
        workspace: str | os.PathLike[str] | None = None,
        materialize: bool = False,
        force: bool = False,
    ) -> str:
        args = [
            "fetch",
            "--server",
            server,
            "--auth-token",
            auth_token,
            "--profile",
            profile,
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if version:
            args += ["--version", version]
        if latest:
            args.append("--latest")
        if workspace is not None:
            args += ["--workspace", _path(workspace)]
        if materialize:
            args.append("--materialize")
        if force:
            args.append("--force")
        return self.run_text(args)

    def sync(
        self,
        *,
        server: str,
        auth_token: str,
        store: str | os.PathLike[str],
        profile: str = "main",
        category: str | None = None,
        asset_code: str | None = None,
        department: str | None = None,
        latest: bool = False,
        all_versions: bool = False,
        workspace: str | os.PathLike[str] | None = None,
        materialize: bool = False,
        force: bool = False,
    ) -> str:
        args = [
            "sync",
            "--server",
            server,
            "--auth-token",
            auth_token,
            "--profile",
            profile,
            "--store",
            _path(store),
        ]
        if category:
            args += ["--category", category]
        if asset_code:
            args += ["--asset-code", asset_code]
        if department:
            args += ["--department", department]
        if latest:
            args.append("--latest")
        if all_versions:
            args.append("--all-versions")
        if workspace is not None:
            args += ["--workspace", _path(workspace)]
        if materialize:
            args.append("--materialize")
        if force:
            args.append("--force")
        return self.run_text(args)

    def checkout(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        dest: str | os.PathLike[str],
        version: str | None = None,
        latest: bool = False,
        force: bool = False,
    ) -> str:
        args = [
            "checkout",
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if version:
            args += ["--version", version]
        if latest:
            args.append("--latest")
        if force:
            args.append("--force")
        args.append(_path(dest))
        return self.run_text(args)

    def resolve(
        self,
        asset_path: str,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        mode: str = "auto",
        remote_base_url: str | None = None,
    ) -> str:
        args = [
            "resolve",
            "--store",
            _path(store),
            *_workspace_args(workspace),
            "--mode",
            mode,
        ]
        if remote_base_url:
            args += ["--remote-base-url", remote_base_url]
        args.append(asset_path)
        return self.run_text(args)

    def set_remote(
        self,
        *,
        store: str | os.PathLike[str],
        remote_base_url: str,
    ) -> str:
        return self.run_text(
            [
                "set-remote",
                "--store",
                _path(store),
                "--remote-base-url",
                remote_base_url,
            ]
        )

    def current_get(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
    ) -> str:
        return self.run_text(_current_args("get", store, category, asset_code, department))

    def current_status(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
    ) -> JsonObject:
        return self.run_json(_current_args("status", store, category, asset_code, department))

    def current_set(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> str:
        return self.run_text(
            [
                *_current_args("set", store, category, asset_code, department),
                "--version",
                version,
            ]
        )

    def current_reset(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
    ) -> str:
        return self.run_text(_current_args("reset", store, category, asset_code, department))

    def thumbnail_set(
        self,
        thumbnail: str | os.PathLike[str],
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> JsonObject:
        return self.run_json(
            [
                "thumbnail",
                "set",
                "--store",
                _path(store),
                "--category",
                category,
                "--asset-code",
                asset_code,
                "--department",
                department,
                "--version",
                version,
                _path(thumbnail),
            ]
        )

    def thumbnail_url(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        version: str | None = None,
        latest: bool = False,
        remote_base_url: str | None = None,
    ) -> str:
        args = [
            "thumbnail",
            "url",
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if version:
            args += ["--version", version]
        if latest:
            args.append("--latest")
        if remote_base_url:
            args += ["--remote-base-url", remote_base_url]
        return self.run_text(args)

    def verify(self, *, store: str | os.PathLike[str]) -> str:
        return self.run_text(["verify", "--store", _path(store)])

    def run_json(self, args: Sequence[str | os.PathLike[str]]) -> JsonObject:
        text = self.run_text(args)
        data = json.loads(text)
        if not isinstance(data, dict):
            raise ValueError("ADS command did not return a JSON object")
        return data

    def run_text(self, args: Sequence[str | os.PathLike[str]]) -> str:
        result = self.run(args)
        return result.stdout.strip()

    def run(self, args: Sequence[str | os.PathLike[str]]) -> subprocess.CompletedProcess[str]:
        cmd = [_path(self.executable), *[_path(arg) for arg in args]]
        result = subprocess.run(cmd, text=True, capture_output=True, check=False)
        if result.returncode != 0:
            raise AdsCommandError(cmd, result.returncode, result.stdout, result.stderr)
        return result


@dataclass(frozen=True)
class AdsHttpClient:
    """Client for the ADS Web API served by `ads serve`."""

    base_url: str
    token: str
    timeout: float = 30.0

    def profiles(self) -> JsonObject:
        return self.get("/api/profiles")

    def assets(
        self,
        *,
        profile: str = "main",
        q: str | None = None,
        category: str | None = None,
        department: str | None = None,
    ) -> JsonObject:
        return self.get(
            "/api/assets",
            {
                "profile": profile,
                "q": q,
                "category": category,
                "department": department,
            },
        )

    def asset(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
    ) -> JsonObject:
        return self.get(
            "/api/asset",
            {"profile": profile, "category": category, "asset_code": asset_code},
        )

    def versions(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
    ) -> JsonObject:
        return self.get(
            "/api/versions",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
            },
        )

    def version_info(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str | None = None,
        latest: bool = False,
    ) -> JsonObject:
        return self.get(
            "/api/version",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "version": version,
                "latest": latest if latest else None,
            },
        )

    def object_bytes(self, sha256: str, *, profile: str = "main") -> bytes:
        return self.request_bytes(
            "GET",
            _with_query("/api/object", {"profile": profile, "sha256": sha256}),
        )

    def download_object(
        self,
        sha256: str,
        dest: str | os.PathLike[str],
        *,
        profile: str = "main",
        force: bool = False,
    ) -> Path:
        path = Path(dest)
        if path.exists() and not force:
            raise FileExistsError(f"destination exists: {path}")
        if path.parent:
            path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(self.object_bytes(sha256, profile=profile))
        return path

    def current_status(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
    ) -> JsonObject:
        return self.get(
            "/api/current/status",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
            },
        )

    def current_set(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> JsonObject:
        return self.put_json(
            "/api/current",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "version": version,
            },
        )

    def current_reset(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
    ) -> JsonObject:
        return self.put_json(
            "/api/current",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "reset": True,
            },
        )

    def pull(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str | None = None,
        latest: bool = False,
        force: bool = False,
    ) -> JsonObject:
        return self.post_json(
            "/api/pull",
            _workspace_request(profile, category, asset_code, department, version, latest, force),
        )

    def restore(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str,
        force: bool = False,
    ) -> JsonObject:
        return self.post_json(
            "/api/restore",
            _workspace_request(profile, category, asset_code, department, version, False, force),
        )

    def resolve(
        self,
        asset_path: str,
        *,
        profile: str = "main",
        mode: str = "auto",
    ) -> JsonObject:
        return self.get(
            "/api/resolve",
            {"profile": profile, "asset_path": asset_path, "mode": mode},
        )

    def thumbnail_url(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str | None = None,
        latest: bool = False,
        remote_base_url: str | None = None,
    ) -> str:
        response = self.get(
            "/api/thumbnail-url",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "version": version,
                "latest": latest if latest else None,
                "remote_base_url": remote_base_url,
            },
        )
        if isinstance(response, str):
            return response
        if "url" in response:
            return str(response["url"])
        raise ValueError("thumbnail-url response did not contain a URL")

    def upload_thumbnail(
        self,
        thumbnail: str | os.PathLike[str],
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> JsonObject:
        path = Path(thumbnail)
        content_type = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        fields = {
            "profile": profile,
            "category": category,
            "asset_code": asset_code,
            "department": department,
            "version": version,
        }
        body, boundary = _multipart_body(fields, "file", path, content_type)
        return self.request_json(
            "POST",
            "/api/thumbnails",
            body=body,
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )

    def get(self, path: str, query: Mapping[str, Any] | None = None) -> Any:
        return self.request_json("GET", _with_query(path, query))

    def post_json(self, path: str, payload: Mapping[str, Any]) -> JsonObject:
        return self.request_json("POST", path, json_body=payload)

    def put_json(self, path: str, payload: Mapping[str, Any]) -> JsonObject:
        return self.request_json("PUT", path, json_body=payload)

    def request_json(
        self,
        method: str,
        path: str,
        *,
        json_body: Mapping[str, Any] | None = None,
        body: bytes | None = None,
        headers: Mapping[str, str] | None = None,
    ) -> Any:
        request_headers: MutableMapping[str, str] = {
            "Authorization": f"Bearer {self.token}",
        }
        if headers:
            request_headers.update(headers)
        data = body
        if json_body is not None:
            data = json.dumps(json_body).encode("utf-8")
            request_headers["Content-Type"] = "application/json"
        request = Request(
            _join_url(self.base_url, path),
            data=data,
            headers=dict(request_headers),
            method=method,
        )
        try:
            with urlopen(request, timeout=self.timeout) as response:
                raw = response.read()
                content_type = response.headers.get("Content-Type", "")
        except HTTPError as error:
            raw = error.read()
            raise AdsHttpError(error.code, raw.decode("utf-8", "replace"), raw) from error
        except URLError as error:
            raise AdsHttpError(0, str(error)) from error

        if not raw:
            return {}
        if "application/json" not in content_type:
            return raw.decode("utf-8")
        return json.loads(raw.decode("utf-8"))

    def request_bytes(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        headers: Mapping[str, str] | None = None,
    ) -> bytes:
        request_headers: MutableMapping[str, str] = {
            "Authorization": f"Bearer {self.token}",
        }
        if headers:
            request_headers.update(headers)
        request = Request(
            _join_url(self.base_url, path),
            data=body,
            headers=dict(request_headers),
            method=method,
        )
        try:
            with urlopen(request, timeout=self.timeout) as response:
                return response.read()
        except HTTPError as error:
            raw = error.read()
            raise AdsHttpError(error.code, raw.decode("utf-8", "replace"), raw) from error
        except URLError as error:
            raise AdsHttpError(0, str(error)) from error


def _path(value: str | os.PathLike[str]) -> str:
    return os.fspath(value)


def _workspace_args(workspace: str | os.PathLike[str] | None) -> list[str]:
    if workspace is None:
        return []
    return ["--workspace", _path(workspace)]


def _current_args(
    action: str,
    store: str | os.PathLike[str],
    category: str,
    asset_code: str,
    department: str,
) -> list[str]:
    return [
        "current",
        action,
        "--store",
        _path(store),
        "--category",
        category,
        "--asset-code",
        asset_code,
        "--department",
        department,
    ]


def _publish_args(
    action: str,
    store: str | os.PathLike[str],
    public_root: str | os.PathLike[str],
    category: str,
    asset_code: str,
    department: str,
    version: str,
) -> list[str]:
    return [
        "publish",
        action,
        "--store",
        _path(store),
        "--public-root",
        _path(public_root),
        "--category",
        category,
        "--asset-code",
        asset_code,
        "--department",
        department,
        "--version",
        version,
    ]


def _workspace_request(
    profile: str,
    category: str,
    asset_code: str,
    department: str,
    version: str | None,
    latest: bool,
    force: bool,
) -> JsonObject:
    payload: JsonObject = {
        "profile": profile,
        "category": category,
        "asset_code": asset_code,
        "department": department,
        "force": force,
    }
    if version is not None:
        payload["version"] = version
    if latest:
        payload["latest"] = True
    return payload


def _join_url(base_url: str, path: str) -> str:
    return base_url.rstrip("/") + "/" + path.lstrip("/")


def _with_query(path: str, query: Mapping[str, Any] | None) -> str:
    if not query:
        return path
    filtered = {key: value for key, value in query.items() if value is not None}
    if not filtered:
        return path
    return f"{path}?{urlencode(filtered)}"


def _multipart_body(
    fields: Mapping[str, str],
    file_field: str,
    file_path: Path,
    content_type: str,
) -> tuple[bytes, str]:
    boundary = f"ads-{uuid.uuid4().hex}"
    chunks: list[bytes] = []
    for name, value in fields.items():
        chunks.extend(
            [
                f"--{boundary}\r\n".encode("utf-8"),
                f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode("utf-8"),
                str(value).encode("utf-8"),
                b"\r\n",
            ]
        )
    chunks.extend(
        [
            f"--{boundary}\r\n".encode("utf-8"),
            (
                f'Content-Disposition: form-data; name="{file_field}"; '
                f'filename="{file_path.name}"\r\n'
            ).encode("utf-8"),
            f"Content-Type: {content_type}\r\n\r\n".encode("utf-8"),
            file_path.read_bytes(),
            b"\r\n",
            f"--{boundary}--\r\n".encode("utf-8"),
        ]
    )
    return b"".join(chunks), boundary
