from __future__ import annotations

import json
import mimetypes
import os
import subprocess
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, MutableMapping, Sequence
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen


JsonObject = dict[str, Any]


class AdsCommandError(RuntimeError):
    """Raised when the local ADS CLI exits with a non-zero status.

    ``returncode`` is ``None`` when the CLI never exited (command timeout).
    """

    def __init__(
        self,
        args: Sequence[str],
        returncode: int | None,
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

    ``timeout`` bounds every subprocess call in seconds; it defaults to
    ``ADS_CLI_TIMEOUT_SECONDS`` when set and is otherwise unlimited (heavy
    `wip add` runs on GB-scale sources legitimately take a long time).
    """

    executable: str | os.PathLike[str] = "ads"
    timeout: float | None = field(default_factory=lambda: _default_cli_timeout())

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

    def add(
        self,
        *,
        store: str | os.PathLike[str],
        workspace: str | os.PathLike[str] | None = None,
        category: str,
        asset_code: str,
        department: str,
        version: int | str | None = None,
        source: str | os.PathLike[str] | None = None,
    ) -> str:
        # Mirrors the Rust CLI: --source registers an arbitrary folder and
        # auto-assigns the next version when --version is omitted; the legacy
        # workspace form locates the v### folder and so requires a version.
        if source is None and version is None:
            raise ValueError("add requires version unless source is provided")
        args: list[str | int | os.PathLike[str]] = [
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
        ]
        if version is not None:
            args += ["--version", version]
        if source is not None:
            args += ["--source", _path(source)]
        return self.run_text(args)

    def info(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str | None = None,
        version: int | str | None = None,
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

    def wip_add(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        source: str | os.PathLike[str],
    ) -> str:
        return self.run_text(
            [
                "wip",
                "add",
                "--store",
                _path(store),
                "--category",
                category,
                "--asset-code",
                asset_code,
                "--department",
                department,
                "--source",
                _path(source),
            ]
        )

    def wip_list(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
    ) -> list[JsonObject]:
        text = self.run_text(
            [
                "wip",
                "list",
                "--store",
                _path(store),
                "--category",
                category,
                "--asset-code",
                asset_code,
                "--department",
                department,
            ]
        )
        data = json.loads(text)
        if not isinstance(data, list):
            raise ValueError("ADS wip list did not return a JSON array")
        return data

    def publish_promote(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        wip_seq: int | None = None,
        no_validate: bool = False,
    ) -> str:
        args = [
            "publish",
            "promote",
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if wip_seq is not None:
            args += ["--wip-seq", wip_seq]
        if no_validate:
            args.append("--no-validate")
        return self.run_text(args)

    def gc(
        self,
        *,
        store: str | os.PathLike[str],
        retention: int | None = None,
        grace_hours: int | None = None,
        dry_run: bool = False,
    ) -> JsonObject:
        args = ["gc", "--store", _path(store)]
        if retention is not None:
            args += ["--retention", retention]
        if grace_hours is not None:
            args += ["--grace-hours", grace_hours]
        if dry_run:
            args.append("--dry-run")
        return self.run_json(args)

    def publish_validate(
        self,
        *,
        store: str | os.PathLike[str] | None = None,
        category: str | None = None,
        asset_code: str | None = None,
        department: str | None = None,
        version: int | str | None = None,
        latest: bool = False,
        wip: bool = False,
        wip_seq: int | None = None,
        source: str | os.PathLike[str] | None = None,
    ) -> str:
        args: list[str | int | os.PathLike[str]] = ["publish", "validate"]
        if source is not None:
            args += ["--source", _path(source)]
            return self.run_text(args)
        if store is None or not category or not asset_code or not department:
            raise ValueError(
                "publish_validate requires store/category/asset_code/department, or source"
            )
        args += [
            "--store",
            _path(store),
            "--category",
            category,
            "--asset-code",
            asset_code,
            "--department",
            department,
        ]
        if version is not None:
            args += ["--version", version]
        if latest:
            args.append("--latest")
        if wip:
            args.append("--wip")
        if wip_seq is not None:
            args += ["--wip-seq", wip_seq]
        return self.run_text(args)

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
        version: int | str | None = None,
        latest: bool = False,
        workspace: str | os.PathLike[str] | None = None,
        materialize: bool = False,
        force: bool = False,
    ) -> str:
        args = [
            "fetch",
            "--server",
            server,
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
        return self.run_text(args, env=_token_env(auth_token))

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
        return self.run_text(args, env=_token_env(auth_token))

    def push(
        self,
        *,
        server: str,
        auth_token: str,
        store: str | os.PathLike[str],
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        version: int | str | None = None,
        latest: bool = False,
        set_current: bool = False,
    ) -> str:
        args = [
            "push",
            "--server",
            server,
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
        if set_current:
            args.append("--set-current")
        return self.run_text(args, env=_token_env(auth_token))

    def checkout(
        self,
        *,
        store: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        dest: str | os.PathLike[str],
        version: int | str | None = None,
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
        version: int | str,
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
        version: int | str,
    ) -> str:
        return self.run_text(
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
        version: int | str | None = None,
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

    def run_json(
        self,
        args: Sequence[str | int | os.PathLike[str]],
        *,
        env: Mapping[str, str] | None = None,
    ) -> JsonObject:
        text = self.run_text(args, env=env)
        data = json.loads(text)
        if not isinstance(data, dict):
            raise ValueError("ADS command did not return a JSON object")
        return data

    def run_text(
        self,
        args: Sequence[str | int | os.PathLike[str]],
        *,
        env: Mapping[str, str] | None = None,
    ) -> str:
        result = self.run(args, env=env)
        return result.stdout.strip()

    def run(
        self,
        args: Sequence[str | int | os.PathLike[str]],
        *,
        env: Mapping[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        cmd = [_path(self.executable), *[_path(arg) for arg in args]]
        try:
            result = subprocess.run(
                cmd,
                text=True,
                capture_output=True,
                check=False,
                timeout=self.timeout,
                env=dict(env) if env is not None else None,
            )
        except subprocess.TimeoutExpired:
            # A wedged ads.exe must not hang the host (Houdini) forever.
            # `from None` severs both cause and context: TimeoutExpired.cmd
            # (and its str()) carries the unredacted argv, so chaining it
            # would print secrets in tracebacks despite the redaction below.
            raise AdsCommandError(
                _redact_secrets(cmd),
                None,
                "",
                f"ads command timed out after {self.timeout} seconds",
            ) from None
        if result.returncode != 0:
            # Errors travel into logs and tracebacks; never carry credentials
            # along (the subprocess itself received the real values).
            raise AdsCommandError(
                _redact_secrets(cmd), result.returncode, result.stdout, result.stderr
            )
        return result


@dataclass(frozen=True)
class AdsHttpClient:
    """Client for the ADS Web API served by `ads serve`."""

    base_url: str
    # repr=False keeps the bearer token out of logs and tracebacks that
    # stringify the client.
    token: str = field(repr=False)
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
        version: int | str | None = None,
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

    def wips(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
    ) -> JsonObject:
        return self.get(
            "/api/wips",
            {
                "profile": profile,
                "category": category,
                "asset_code": asset_code,
                "department": department,
            },
        )

    def promote(
        self,
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        wip_seq: int | None = None,
        no_validate: bool = False,
    ) -> JsonObject:
        payload: JsonObject = {
            "profile": profile,
            "category": category,
            "asset_code": asset_code,
            "department": department,
        }
        if wip_seq is not None:
            payload["wip_seq"] = wip_seq
        if no_validate:
            payload["no_validate"] = True
        return self.post_json("/api/promote", payload)

    def gc(
        self,
        *,
        profile: str = "main",
        retention: int | None = None,
        grace_hours: int | None = None,
        dry_run: bool = False,
    ) -> JsonObject:
        payload: JsonObject = {"profile": profile, "dry_run": dry_run}
        if retention is not None:
            payload["retention"] = retention
        if grace_hours is not None:
            payload["grace_hours"] = grace_hours
        return self.post_json("/api/gc", payload)

    def wip_import(
        self,
        manifest: Mapping[str, Any],
        *,
        profile: str = "main",
        category: str,
        asset_code: str,
        department: str,
        source_path: str | None = None,
    ) -> JsonObject:
        # Remote counterpart of `ads wip add` for stores held open by
        # `ads serve`: every object in the manifest must already exist on the
        # server (upload via upload_object first). source_path defaults
        # server-side to category/asset_code/department.
        #
        # The server rejects manifests whose entries are not strictly
        # ascending by relative_path (canonical form). Sorting here is safe
        # because the server computes the manifest hash from what it
        # receives; callers building entries in filesystem-listing order
        # (case-insensitive on NTFS) would otherwise get a surprise 400.
        # Duplicate paths are left for the server to reject with its message.
        body = dict(manifest)
        entries = body.get("entries")
        if isinstance(entries, list):
            body["entries"] = sorted(
                entries, key=lambda entry: entry.get("relative_path", "")
            )
        payload: JsonObject = {
            "profile": profile,
            "category": category,
            "asset_code": asset_code,
            "department": department,
            "manifest": body,
        }
        if source_path is not None:
            payload["source_path"] = source_path
        return self.post_json("/api/wip", payload)

    def object_bytes(self, sha256: str, *, profile: str = "main") -> bytes:
        return self.request_bytes(
            "GET",
            _with_query("/api/object", {"profile": profile, "sha256": sha256}),
        )

    def object_status(
        self,
        sha256: str,
        *,
        profile: str = "main",
        size: int | None = None,
    ) -> JsonObject:
        return self.get(
            "/api/object/status",
            {"profile": profile, "sha256": sha256, "size": size},
        )

    def upload_object(
        self,
        sha256: str,
        data: bytes,
        *,
        profile: str = "main",
    ) -> JsonObject:
        return self.request_json(
            "PUT",
            _with_query("/api/object", {"profile": profile, "sha256": sha256}),
            body=data,
            headers={"Content-Type": "application/octet-stream"},
        )

    def import_version_info(
        self,
        version_info: Mapping[str, Any],
        *,
        profile: str = "main",
    ) -> JsonObject:
        # The manifest's entries must already be strictly ascending by
        # relative_path (canonical form) — the server rejects anything else.
        # Unlike wip_import, no client-side sort is possible here: the
        # record's manifest_hash was computed over the entry order, so
        # reordering would break the hash check instead of fixing the sort.
        return self.put_json(
            "/api/version",
            {"profile": profile, "version_info": dict(version_info)},
        )

    def import_thumbnail_info(
        self,
        thumbnail: Mapping[str, Any],
        *,
        profile: str = "main",
    ) -> JsonObject:
        return self.put_json(
            "/api/thumbnail",
            {"profile": profile, "thumbnail": dict(thumbnail)},
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
        version: int | str,
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
        version: int | str | None = None,
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
        version: int | str,
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
        version: int | str | None = None,
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
        version: int | str,
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


_SECRET_FLAGS = frozenset({"--auth-token"})


def _redact_secrets(args: Sequence[str]) -> list[str]:
    redacted = list(args)
    for index, value in enumerate(redacted[:-1]):
        if value in _SECRET_FLAGS:
            redacted[index + 1] = "***"
    return redacted


def _token_env(auth_token: str) -> dict[str, str]:
    # The token travels via the environment instead of argv so it never shows
    # up in process listings (Task Manager, ps). The Rust CLI reads
    # ADS_WEB_TOKEN for every --server command.
    return {**os.environ, "ADS_WEB_TOKEN": auth_token}


def _default_cli_timeout() -> float | None:
    value = os.environ.get("ADS_CLI_TIMEOUT_SECONDS", "").strip()
    if not value:
        return None
    timeout = float(value)
    if timeout <= 0:
        raise ValueError("ADS_CLI_TIMEOUT_SECONDS must be greater than zero")
    return timeout


def _path(value: str | int | os.PathLike[str]) -> str:
    # Versions are plain integers in schema v8; let callers pass them as-is.
    if isinstance(value, int):
        return str(value)
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


def _workspace_request(
    profile: str,
    category: str,
    asset_code: str,
    department: str,
    version: int | str | None,
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
