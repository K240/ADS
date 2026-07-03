//! Client for a remote ADS server: fetch, sync, and push plumbing.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::*;

#[derive(Clone, Debug)]
pub(crate) struct RemoteClient {
    server: String,
    auth_token: String,
}

/// Sends exactly the declared Content-Length: ureq copies a reader to EOF
/// regardless of the header, so a source file that grows mid-transfer (a DCC
/// still appending — the exact scenario ADS targets) would write surplus
/// bytes past the framing boundary and poison the keep-alive connection,
/// while one that shrinks would leave the server waiting forever for bytes
/// that never come. Growth is truncated at the declared size (the server's
/// hash verification rejects the upload if the prefix changed too); shrink
/// fails fast with UnexpectedEof.
pub(crate) struct ExactLenReader<R> {
    pub(crate) inner: R,
    pub(crate) remaining: u64,
}

impl<R: Read> Read for ExactLenReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..want])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "source ended {} bytes short of its declared length (file shrank mid-upload?)",
                    self.remaining
                ),
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

impl RemoteClient {
    pub(crate) fn new(server: &str, auth_token: &str) -> Result<Self> {
        let server = server.trim().trim_end_matches('/').to_string();
        if server.is_empty() {
            bail!("--server must not be empty");
        }
        if !server.starts_with("http://") && !server.starts_with("https://") {
            bail!("--server must start with http:// or https://");
        }
        let auth_token = auth_token.trim().to_string();
        if auth_token.is_empty() {
            bail!("--auth-token or ADS_WEB_TOKEN is required");
        }
        Ok(Self { server, auth_token })
    }

    pub(crate) fn fetch_version_info(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
        selector: VersionSelector,
    ) -> Result<VersionInfo> {
        let mut query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        match selector {
            VersionSelector::Current => {}
            VersionSelector::Latest => query.push(("latest", "true".to_string())),
            VersionSelector::Wip => bail!("wip versions are local-only and cannot be fetched"),
            VersionSelector::Version(version) => query.push(("version", version.0.to_string())),
        }
        self.get_json("/api/version", &query)
    }

    pub(crate) fn fetch_assets(
        &self,
        profile: &str,
        category: Option<&str>,
        asset_code: Option<&str>,
        department: Option<&str>,
    ) -> Result<AssetsResponse> {
        let mut query = vec![("profile", profile.to_string())];
        if let Some(category) = category {
            query.push(("category", category.to_string()));
        }
        if let Some(asset_code) = asset_code {
            query.push(("asset_code", asset_code.to_string()));
        }
        if let Some(department) = department {
            query.push(("department", department.to_string()));
        }
        self.get_json("/api/assets", &query)
    }

    pub(crate) fn fetch_versions(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
    ) -> Result<VersionsResponse> {
        let query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        self.get_json("/api/versions", &query)
    }

    pub(crate) fn fetch_current_status(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
    ) -> Result<CurrentStatus> {
        let query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        let statuses: Vec<CurrentStatus> = self.get_json("/api/current/status", &query)?;
        statuses
            .into_iter()
            .find(|status| {
                status.department_key.asset_key.category == category
                    && status.department_key.asset_key.asset_code == asset_code
                    && status.department_key.department == department
            })
            .ok_or_else(|| {
                anyhow!(
                    "remote current status not found for {}/{}/{}",
                    category,
                    asset_code,
                    department
                )
            })
    }

    /// Streams the response body straight into the local store's temp +
    /// verify + rename path, so a multi-GB fetch never lives in memory.
    /// Returns the number of bytes downloaded.
    pub(crate) fn fetch_object(&self, profile: &str, sha256: &str, store: &Store) -> Result<u64> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
        ];
        let url = self.url("/api/object", &query);
        let response = self.request(&url)?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let text = read_response_text(response, &url).unwrap_or_default();
            bail!("remote request failed {status} {url}: {text}");
        }
        let mut reader = response.into_reader();
        store
            .write_object_from_reader(sha256, &mut reader)
            .with_context(|| format!("failed to store object from {url}"))
    }

    pub(crate) fn object_status(
        &self,
        profile: &str,
        sha256: &str,
        expected_size: u64,
    ) -> Result<ObjectStatusResponse> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
            ("size", expected_size.to_string()),
        ];
        self.get_json("/api/object/status", &query)
    }

    /// Uploads by handing ureq the file reader itself. The explicit
    /// Content-Length keeps ureq from falling back to chunked framing, so
    /// the server sees the true object size up front.
    pub(crate) fn upload_object(
        &self,
        profile: &str,
        sha256: &str,
        file: &Path,
    ) -> Result<ObjectUploadResponse> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
        ];
        let url = self.url("/api/object", &query);
        let reader =
            File::open(file).with_context(|| format!("failed to open {}", file.display()))?;
        let size = reader
            .metadata()
            .with_context(|| format!("failed to stat {}", file.display()))?
            .len();
        let response = match ureq::put(&url)
            .set("Authorization", &format!("Bearer {}", self.auth_token))
            .set("Content-Type", "application/octet-stream")
            .set("Content-Length", &size.to_string())
            .send(ExactLenReader {
                inner: reader,
                remaining: size,
            }) {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => {
                return Err(error).with_context(|| format!("remote request failed: {url}"));
            }
        };
        decode_json_response(response, &url)
    }

    pub(crate) fn import_version_info(
        &self,
        profile: &str,
        version_info: &VersionInfo,
    ) -> Result<VersionRecord> {
        self.put_json(
            "/api/version",
            &[],
            &VersionImportRequest {
                profile: profile.to_string(),
                version_info: version_info.clone(),
            },
        )
    }

    pub(crate) fn import_thumbnail_info(
        &self,
        profile: &str,
        thumbnail: &ThumbnailRecord,
    ) -> Result<ThumbnailRecord> {
        self.put_json(
            "/api/thumbnail",
            &[],
            &ThumbnailImportRequest {
                profile: profile.to_string(),
                thumbnail: thumbnail.clone(),
            },
        )
    }

    pub(crate) fn set_current_version(
        &self,
        profile: &str,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<CurrentStatus> {
        self.put_json(
            "/api/current",
            &[],
            &serde_json::json!({
                "profile": profile,
                "category": &department_key.asset_key.category,
                "asset_code": &department_key.asset_key.asset_code,
                "department": &department_key.department,
                "version": version,
            }),
        )
    }

    pub(crate) fn apply_current_status(
        &self,
        profile: &str,
        status: &CurrentStatus,
    ) -> Result<CurrentStatus> {
        if status.explicit {
            let version = status.current.ok_or_else(|| {
                anyhow!(
                    "local current status is explicit but has no current version: {}/{}/{}",
                    status.department_key.asset_key.category,
                    status.department_key.asset_key.asset_code,
                    status.department_key.department
                )
            })?;
            self.set_current_version(profile, &status.department_key, version)
        } else {
            self.put_json(
                "/api/current",
                &[],
                &serde_json::json!({
                    "profile": profile,
                    "category": &status.department_key.asset_key.category,
                    "asset_code": &status.department_key.asset_key.asset_code,
                    "department": &status.department_key.department,
                    "reset": true,
                }),
            )
        }
    }

    pub(crate) fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path, query);
        let response = self.request(&url)?;
        decode_json_response(response, &url)
    }

    pub(crate) fn put_json<T, B>(&self, path: &str, query: &[(&str, String)], body: &B) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize,
    {
        let url = self.url(path, query);
        let body = serde_json::to_vec(body).context("failed to encode JSON request")?;
        let response = self.put_request(&url, "application/json", &body)?;
        decode_json_response(response, &url)
    }

    pub(crate) fn request(&self, url: &str) -> Result<ureq::Response> {
        match ureq::get(url)
            .set("Authorization", &format!("Bearer {}", self.auth_token))
            .call()
        {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(error) => Err(error).with_context(|| format!("remote request failed: {url}")),
        }
    }

    pub(crate) fn put_request(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<ureq::Response> {
        match ureq::put(url)
            .set("Authorization", &format!("Bearer {}", self.auth_token))
            .set("Content-Type", content_type)
            .send_bytes(body)
        {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(error) => Err(error).with_context(|| format!("remote request failed: {url}")),
        }
    }

    pub(crate) fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}/{}", self.server, path.trim_start_matches('/'));
        if !query.is_empty() {
            url.push('?');
            for (index, (key, value)) in query.iter().enumerate() {
                if index > 0 {
                    url.push('&');
                }
                url.push_str(&url_encode_component(key));
                url.push('=');
                url.push_str(&url_encode_component(value));
            }
        }
        url
    }
}

/// Hard ceiling for buffered response bodies. ureq's `into_string()` silently
/// truncates at 10 MB, which corrupted manifest JSON for large versions;
/// reading through `into_reader()` removes that cap, and this limit only
/// keeps a rogue server from streaming an unbounded reply into client memory.
const MAX_RESPONSE_BODY_BYTES: u64 = 1024 * 1024 * 1024;

fn read_response_text(response: ureq::Response, url: &str) -> Result<String> {
    let mut text = String::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BODY_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("failed to read response from {url}"))?;
    if text.len() as u64 > MAX_RESPONSE_BODY_BYTES {
        bail!("response from {url} exceeds {MAX_RESPONSE_BODY_BYTES} bytes");
    }
    Ok(text)
}

fn decode_json_response<T: serde::de::DeserializeOwned>(
    response: ureq::Response,
    url: &str,
) -> Result<T> {
    let status = response.status();
    let text = read_response_text(response, url)?;
    if !(200..300).contains(&status) {
        bail!("remote request failed {status} {url}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("failed to decode JSON from {url}"))
}
