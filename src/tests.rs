use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

#[test]
fn version_id_parses_formats_and_increments() {
    let version = VersionId::from_str("v001").unwrap();
    assert_eq!(version, VersionId(1));
    assert_eq!(version.to_string(), "v001");
    assert_eq!(version.next().to_string(), "v002");
    assert_eq!(VersionId::from_str("v1000").unwrap().to_string(), "v1000");
    // Lenient parse: bare integers and zero-padded digits are canonical v8 forms.
    assert_eq!(VersionId::from_str("12").unwrap(), VersionId(12));
    assert_eq!(VersionId::from_str("001").unwrap(), VersionId(1));
    assert!(VersionId::from_str("v000").is_err());
    assert!(VersionId::from_str("0").is_err());
    assert!(VersionId::from_str("").is_err());
    assert!(VersionId::from_str("v").is_err());
    assert!(VersionId::from_str("v1a").is_err());
    // JSON canonical form is a number; both number and string deserialize.
    assert_eq!(serde_json::to_string(&VersionId(12)).unwrap(), "12");
    assert_eq!(
        serde_json::from_str::<VersionId>("12").unwrap(),
        VersionId(12)
    );
    assert_eq!(
        serde_json::from_str::<VersionId>("\"v012\"").unwrap(),
        VersionId(12)
    );
    // Fixed-width key encoding keeps lexicographic order numeric past v999.
    assert_eq!(VersionId(999).key_encode(), "0000000999");
    assert_eq!(VersionId(1000).key_encode(), "0000001000");
    assert!(VersionId(999).key_encode() < VersionId(1000).key_encode());
}

#[test]
fn object_path_uses_sha256_prefix() {
    let path = object_path(
        Path::new("store"),
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    );
    assert_eq!(
        path,
        Path::new("store")
            .join("objects")
            .join("sha256")
            .join("ab")
            .join("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
    );
}

#[test]
fn asset_file_kind_splits_composing_formats_from_leaves() {
    // Composing formats carry relative sibling references and resolve
    // through the manifest view.
    assert_eq!(asset_file_kind("hero.usd"), AssetFileKind::Composing);
    assert_eq!(asset_file_kind("geo/body.USDA"), AssetFileKind::Composing);
    assert_eq!(asset_file_kind("mtl/look.mtlx"), AssetFileKind::Composing);
    // Everything else is a leaf and resolves lazily to the flat blob
    // cache — textures, volumes, caches, and unknown formats alike.
    assert_eq!(asset_file_kind("body_diffuse.1001.tx"), AssetFileKind::Leaf);
    assert_eq!(asset_file_kind("vol/smoke.vdb"), AssetFileKind::Leaf);
    assert_eq!(asset_file_kind("cache/custom.bin"), AssetFileKind::Leaf);
    assert_eq!(asset_file_kind("source/readme"), AssetFileKind::Leaf);
}

#[test]
fn cache_object_path_uses_sha256_prefix_and_source_extension() {
    let entry = ManifestEntry {
        relative_path: "maps/body_diffuse.1001.TX".to_string(),
        sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
        size: 10,
        mode: 0o666,
    };
    let path = cache_object_path(Path::new("workspace"), &entry);
    assert_eq!(
        path,
        Path::new("workspace")
            .join(".ads-cache")
            .join("sha256")
            .join("ab")
            .join("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789.tx")
    );
}

#[test]
fn usd_asset_reference_scanner_extracts_at_paths() {
    let references = extract_usd_asset_references(
        r#"
            def "Root" (
                references = @ads://prop/crate/model/crate.usd?v=v001@
                payload = @../texture/v001/body.1001.tx@
            ) {}
            "#,
    );

    assert_eq!(
        references,
        vec![
            "ads://prop/crate/model/crate.usd?v=v001".to_string(),
            "../texture/v001/body.1001.tx".to_string(),
        ]
    );
}

#[test]
fn usd_asset_reference_scanner_handles_triple_at_escapes() {
    assert_eq!(extract_usd_asset_references("@a.usd@"), vec!["a.usd"]);
    assert_eq!(
        extract_usd_asset_references("@@@weird @ path.usd@@@"),
        vec!["weird @ path.usd"]
    );
    // Both delimiter forms on one line, in either order.
    assert_eq!(
        extract_usd_asset_references("references = @a.usd@, payload = @@@weird @ path.usd@@@"),
        vec!["a.usd", "weird @ path.usd"]
    );
    assert_eq!(
        extract_usd_asset_references("payload = @@@weird @ path.usd@@@, references = @a.usd@"),
        vec!["weird @ path.usd", "a.usd"]
    );
}

#[test]
fn publish_reference_validation_applies_v8_policy() {
    let mut report = PublishValidateReport {
        target: "version 1".to_string(),
        files_scanned: 1,
        references_checked: 0,
        warnings: Vec::new(),
        errors: Vec::new(),
    };
    let entry_paths: BTreeSet<String> = ["asset.usda", "geo/body.usd", "maps/d.tx"]
        .into_iter()
        .map(str::to_string)
        .collect();

    // ads:// and manifest-internal relative references are accepted.
    validate_publish_reference(
        &mut report,
        &entry_paths,
        "asset.usda",
        "ads://prop/crate/model/crate.usd?v=2",
    );
    validate_publish_reference(&mut report, &entry_paths, "asset.usda", "geo/body.usd");
    validate_publish_reference(&mut report, &entry_paths, "geo/body.usd", "../maps/d.tx");
    assert!(report.errors.is_empty());

    // Absolute paths, file URIs, missing siblings, and escapes are errors.
    validate_publish_reference(
        &mut report,
        &entry_paths,
        "asset.usda",
        r"D:\workspace\asset.usd",
    );
    validate_publish_reference(&mut report, &entry_paths, "asset.usda", "file:///tmp/a.usd");
    validate_publish_reference(&mut report, &entry_paths, "asset.usda", "geo/missing.usd");
    validate_publish_reference(&mut report, &entry_paths, "asset.usda", "../outside.usd");
    validate_publish_reference(
        &mut report,
        &entry_paths,
        "asset.usda",
        "https://example.com/asset.usd",
    );

    assert_eq!(report.errors.len(), 4);
    assert!(report.errors[0].contains("absolute path"));
    assert!(report.errors[1].contains("file URI"));
    assert!(report.errors[2].contains("missing from the version"));
    assert!(report.errors[3].contains("escapes the version root"));
    assert_eq!(report.warnings.len(), 1);
}

#[test]
fn remote_client_url_encodes_nested_category() {
    let client = RemoteClient::new("http://server:8787/", "secret").unwrap();
    let url = client.url(
        "/api/version",
        &[
            ("profile", "main".to_string()),
            ("category", "assets/characters/main".to_string()),
            ("asset_code", "hero".to_string()),
        ],
    );

    assert_eq!(
        url,
        "http://server:8787/api/version?profile=main&category=assets%2Fcharacters%2Fmain&asset_code=hero"
    );
}

#[test]
fn adsignore_matches_root_and_nested_files() {
    let rules = IgnoreRules {
        rules: vec![
            IgnoreRule::new("*.tmp").unwrap(),
            IgnoreRule::new("cache/").unwrap(),
            IgnoreRule::new("nested/generated.dat").unwrap(),
        ],
    };

    assert!(rules.is_ignored(Path::new("file.tmp"), false));
    assert!(rules.is_ignored(Path::new("nested/file.tmp"), false));
    assert!(rules.is_ignored(Path::new("cache"), true));
    assert!(!rules.is_ignored(Path::new("cache/file.txt"), false));
    assert!(rules.is_ignored(Path::new("nested/generated.dat"), false));
    assert!(!rules.is_ignored(Path::new("nested/kept.dat"), false));
    assert!(is_default_ignored(Path::new(".ads-cache"), true));
    assert!(is_default_ignored(
        Path::new(".ads-cache/sha256/ab/object.tx"),
        false
    ));
}

#[test]
fn manifest_hash_is_stable_after_sorting() {
    let mut first = Manifest {
        entries: vec![
            ManifestEntry {
                relative_path: "b.txt".to_string(),
                sha256: "b".repeat(64),
                size: 2,
                mode: 0o666,
            },
            ManifestEntry {
                relative_path: "a.txt".to_string(),
                sha256: "a".repeat(64),
                size: 1,
                mode: 0o666,
            },
        ],
    };
    let mut second = Manifest {
        entries: first.entries.iter().cloned().rev().collect(),
    };
    first
        .entries
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    second
        .entries
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    assert_eq!(
        first.canonical_hash().unwrap(),
        second.canonical_hash().unwrap()
    );
}

#[test]
fn init_twice_on_same_store_is_idempotent_and_keeps_data() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");

    let store = Store::init(&store_path).unwrap();
    store
        .set_remote_base_url("https://assets.example.com/objects/sha256")
        .unwrap();
    drop(store);

    let store = Store::init(&store_path).unwrap();
    assert_eq!(
        store.remote_base_url().unwrap().as_deref(),
        Some("https://assets.example.com/objects/sha256")
    );
}

#[test]
fn init_refuses_to_restamp_an_older_schema_store() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");

    {
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open(&options, db_path(&store_path)).unwrap();
        db.put(key_meta("schema_version"), b"7").unwrap();
    }

    let err = match Store::init(&store_path) {
        Ok(_) => panic!("init must fail on an older-schema store"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("unsupported store schema version 7"), "{err}");
    assert!(err.contains(&format!("expected {SCHEMA_VERSION}")), "{err}");

    // The failed init must leave the v7 marker untouched.
    let db = DB::open_for_read_only(&Options::default(), db_path(&store_path), false).unwrap();
    assert_eq!(
        db.get(key_meta("schema_version")).unwrap().as_deref(),
        Some(b"7".as_slice())
    );
}

#[test]
fn init_refuses_a_database_without_schema_marker() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");

    // A DB directory with content but no marker is either foreign or a
    // half-initialized store; init must not stamp it as current.
    {
        let mut options = Options::default();
        options.create_if_missing(true);
        DB::open(&options, db_path(&store_path)).unwrap();
    }

    let err = match Store::init(&store_path) {
        Ok(_) => panic!("init must fail on a database without a schema marker"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("no schema_version marker"), "{err}");
}

#[test]
fn add_reuses_identical_manifest() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");

    let store = Store::init(&store_path).unwrap();
    let key = AssetKey::new("char".to_string(), "hero".to_string()).unwrap();
    let department_key = DepartmentKey::new(key, "model".to_string()).unwrap();
    fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
    fs::write(
        version_folder(&workspace, &department_key, VersionId(1)).join("asset.usd"),
        "usd content",
    )
    .unwrap();
    let first = store
        .add_version_folder(&workspace, &department_key, VersionId(1))
        .unwrap();
    let second = store
        .add_version_folder(&workspace, &department_key, VersionId(1))
        .unwrap();

    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.version, second.version);
}

#[test]
fn scan_hash_cache_memoizes_by_stat_and_never_corrupts_the_store() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let source = temp.path().join("source");
    let store = Store::init(&store_path).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.usd"), "content-a").unwrap();
    fs::write(source.join("b.usd"), "content-b").unwrap();

    let manifest = store.build_manifest(&source).unwrap();
    let sha_a = sha256_bytes(b"content-a");
    let sha_b = sha256_bytes(b"content-b");
    assert_eq!(manifest.entries[0].sha256, sha_a);

    // The memo is consulted while the stat is unchanged: poison the recorded
    // hash for a.usd with b's (already stored) object and watch it be
    // believed on the next scan.
    let index_path = source.join(".ads-cache").join("hash-index.json");
    let mut index: HashIndex = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    index.entries.get_mut("a.usd").unwrap().sha256 = sha_b.clone();
    fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    let memoized = store.build_manifest(&source).unwrap();
    assert_eq!(memoized.entries[0].sha256, sha_b);

    // A content change moves the stat, invalidating the memo.
    fs::write(source.join("a.usd"), "content-a2").unwrap();
    let rehashed = store.build_manifest(&source).unwrap();
    assert_eq!(rehashed.entries[0].sha256, sha256_bytes(b"content-a2"));

    // A memoized hash without a stored object is never trusted when
    // persisting: objects must only be written under hashes computed from
    // their bytes, so a stale memo cannot corrupt the store.
    let mut index: HashIndex = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    let bogus = "0".repeat(64);
    index.entries.get_mut("a.usd").unwrap().sha256 = bogus.clone();
    fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    let recovered = store.build_manifest(&source).unwrap();
    assert_eq!(recovered.entries[0].sha256, sha256_bytes(b"content-a2"));
    assert!(!object_path(&store_path, &bogus).exists());
}

#[tokio::test]
async fn web_api_requires_bearer_token_and_serves_static_ui() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let public = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::OK);
    assert!(response_text(public).await.contains("ADS Asset Browser"));

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profiles")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let wrong = app
        .clone()
        .oneshot(api_request("GET", "/api/profiles", "wrong", Body::empty()))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let ok = app
        .clone()
        .oneshot(api_request("GET", "/api/profiles", "secret", Body::empty()))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let body = response_json(ok).await;
    assert_eq!(body["profiles"][0]["name"], "main");

    let missing_profile = app
        .oneshot(api_request(
            "GET",
            "/api/assets?profile=missing",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(missing_profile.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn web_api_lists_assets_updates_current_and_pulls() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let store = Store::init(&store_path).unwrap();
    store
        .set_remote_base_url("https://assets.example.com/objects/sha256")
        .unwrap();
    let department_key = DepartmentKey::new(
        AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
    fs::write(
        version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
        "v1",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &department_key, VersionId(1))
        .unwrap();
    fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(2))).unwrap();
    fs::write(
        version_folder(&workspace, &department_key, VersionId(2)).join("crate.usd"),
        "v2",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &department_key, VersionId(2))
        .unwrap();
    let thumb = temp.path().join("thumb.png");
    fs::write(&thumb, test_png_1x1()).unwrap();
    store
        .set_thumbnail(&department_key, VersionId(2), &thumb)
        .unwrap();
    let nested_department_key = DepartmentKey::new(
        AssetKey::new("prop/vehicle".to_string(), "truck".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    fs::create_dir_all(version_folder(
        &workspace,
        &nested_department_key,
        VersionId(1),
    ))
    .unwrap();
    fs::write(
        version_folder(&workspace, &nested_department_key, VersionId(1)).join("truck.usd"),
        "truck-v1",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &nested_department_key, VersionId(1))
        .unwrap();
    // Sibling category sharing the `prop` spelling as a raw prefix; the
    // category filter must not match it.
    let propx_department_key = DepartmentKey::new(
        AssetKey::new("propx".to_string(), "box".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    fs::create_dir_all(version_folder(
        &workspace,
        &propx_department_key,
        VersionId(1),
    ))
    .unwrap();
    fs::write(
        version_folder(&workspace, &propx_department_key, VersionId(1)).join("box.usd"),
        "box-v1",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &propx_department_key, VersionId(1))
        .unwrap();
    drop(store);

    let app = web_app(test_web_state(&store_path, &workspace));
    let assets = app
        .clone()
        .oneshot(api_request(
            "GET",
            "/api/assets?profile=main",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(assets.status(), StatusCode::OK);
    let assets = response_json(assets).await;
    assert_eq!(assets["assets"][0]["category"], "prop");
    assert_eq!(assets["assets"][0]["asset_code"], "crate");
    assert_eq!(assets["assets"][0]["department"], "model");
    assert_eq!(assets["assets"][0]["current"], 2);
    assert!(
        assets["assets"][0]["thumbnail_url"]
            .as_str()
            .unwrap()
            .starts_with("https://assets.example.com/objects/sha256/")
    );
    assert_eq!(assets["assets"][0]["thumbnail_mime_type"], "image/png");
    assert_eq!(
        assets["assets"][0]["thumbnail_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    // Category filters match whole segments: `prop` covers the nested
    // `prop/vehicle` but not the raw-prefix sibling `propx`.
    let filtered_assets = app
        .clone()
        .oneshot(api_request(
            "GET",
            "/api/assets?profile=main&category=prop",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(filtered_assets.status(), StatusCode::OK);
    let filtered_assets = response_json(filtered_assets).await;
    let filtered_categories: Vec<&str> = filtered_assets["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asset| asset["category"].as_str().unwrap())
        .collect();
    assert_eq!(filtered_categories, vec!["prop", "prop/vehicle"]);

    let nested_assets = app
        .clone()
        .oneshot(api_request(
            "GET",
            "/api/assets?profile=main&category=prop/vehicle",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(nested_assets.status(), StatusCode::OK);
    let nested_assets = response_json(nested_assets).await;
    assert_eq!(nested_assets["assets"].as_array().unwrap().len(), 1);
    assert_eq!(nested_assets["assets"][0]["category"], "prop/vehicle");
    assert_eq!(nested_assets["assets"][0]["asset_code"], "truck");

    let version_info = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/version?profile=main&category=prop&asset_code=crate&department=model&version=v002",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
    assert_eq!(version_info.status(), StatusCode::OK);
    let version_info = response_json(version_info).await;
    assert_eq!(version_info["version"]["version"], 2);
    assert_eq!(
        version_info["manifest"]["entries"][0]["relative_path"],
        "crate.usd"
    );

    let v2_hash = sha256_bytes(b"v2");
    let object = app
        .clone()
        .oneshot(api_request(
            "GET",
            &format!("/api/object?profile=main&sha256={v2_hash}"),
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(object.status(), StatusCode::OK);
    assert_eq!(
        object.headers().get("x-ads-sha256").unwrap(),
        v2_hash.as_str()
    );
    assert_eq!(response_bytes(object).await, b"v2");

    let set_current = app
            .clone()
            .oneshot(api_request(
                "PUT",
                "/api/current",
                "secret",
                Body::from(
                    r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model","version":"v001"}"#,
                ),
            ))
            .await
            .unwrap();
    assert_eq!(set_current.status(), StatusCode::OK);
    let set_current = response_json(set_current).await;
    assert_eq!(set_current["current"], 1);
    assert_eq!(set_current["explicit"], true);

    // Schema v8: pull seeds the department work folder (no v### name),
    // the same root the WIP staging processor redirects from.
    fs::remove_dir_all(department_folder(&workspace, &department_key)).unwrap();
    let pull = app
            .oneshot(api_request(
                "POST",
                "/api/pull",
                "secret",
                Body::from(
                    r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model","version":"v001"}"#,
                ),
            ))
            .await
            .unwrap();
    assert_eq!(pull.status(), StatusCode::OK);
    assert_eq!(
        fs::read_to_string(department_folder(&workspace, &department_key).join("crate.usd"))
            .unwrap(),
        "v1"
    );
}

#[tokio::test]
async fn web_api_uploads_thumbnail_and_returns_url() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let store = Store::init(&store_path).unwrap();
    store
        .set_remote_base_url("https://assets.example.com/objects/sha256")
        .unwrap();
    let department_key = DepartmentKey::new(
        AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
    fs::write(
        version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
        "v1",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &department_key, VersionId(1))
        .unwrap();
    drop(store);

    let app = web_app(test_web_state(&store_path, &workspace));
    let boundary = "ADSBOUNDARY";
    let body = multipart_thumbnail_body(boundary);
    let upload = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/thumbnails")
                .header(header::AUTHORIZATION, "Bearer secret")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    let upload = response_json(upload).await;
    assert_eq!(upload["mime_type"], "image/png");
    assert_eq!(upload["width"], 1);

    let thumb_hash = sha256_bytes(test_png_1x1());
    let url = app
            .oneshot(api_request(
                "GET",
                "/api/thumbnail-url?profile=main&category=prop&asset_code=crate&department=model&version=v001",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
    assert_eq!(url.status(), StatusCode::OK);
    let url = response_json(url).await;
    assert_eq!(
        url.as_str().unwrap(),
        format!(
            "https://assets.example.com/objects/sha256/{}/{}",
            &thumb_hash[0..2],
            thumb_hash
        )
    );
}

#[tokio::test]
async fn web_api_accepts_object_and_version_import() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let object_bytes = b"remote-v1";
    let object_hash = sha256_bytes(object_bytes);
    let status = app
        .clone()
        .oneshot(api_request(
            "GET",
            &format!(
                "/api/object/status?profile=main&sha256={object_hash}&size={}",
                object_bytes.len()
            ),
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(response_json(status).await["exists"], false);

    let upload = app
        .clone()
        .oneshot(api_request(
            "PUT",
            &format!("/api/object?profile=main&sha256={object_hash}"),
            "secret",
            Body::from(object_bytes.as_slice()),
        ))
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    let upload = response_json(upload).await;
    assert_eq!(upload["sha256"], object_hash);
    assert_eq!(upload["reused"], false);

    let department_key = DepartmentKey::new(
        AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    let manifest = Manifest {
        entries: vec![ManifestEntry {
            relative_path: "crate.usd".to_string(),
            sha256: object_hash.clone(),
            size: object_bytes.len() as u64,
            mode: 0o666,
        }],
    };
    let version_info = VersionInfo {
        version: VersionRecord {
            department_key: department_key.clone(),
            version: VersionId(1),
            manifest_hash: manifest.canonical_hash().unwrap(),
            created_at: "2026-05-27T00:00:00Z".to_string(),
            source_path: "prop/crate/model/v001".to_string(),
            file_count: 1,
            total_bytes: object_bytes.len() as u64,
            promoted_from: None,
        },
        manifest,
    };
    let import = app
        .clone()
        .oneshot(api_request(
            "PUT",
            "/api/version",
            "secret",
            Body::from(
                serde_json::to_vec(&VersionImportRequest {
                    profile: "main".to_string(),
                    version_info,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(import.status(), StatusCode::OK);
    assert_eq!(response_json(import).await["version"], 1);

    let fetched = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/version?profile=main&category=prop&asset_code=crate&department=model&version=v001",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched = response_json(fetched).await;
    assert_eq!(fetched["manifest"]["entries"][0]["sha256"], object_hash);

    let thumbnail = ThumbnailRecord {
        department_key,
        version: VersionId(1),
        sha256: object_hash.clone(),
        size: object_bytes.len() as u64,
        mime_type: "image/png".to_string(),
        width: Some(256),
        height: Some(256),
        created_at: "2026-05-27T00:00:00Z".to_string(),
        source_path: "thumbnail.png".to_string(),
    };
    let import_thumbnail = app
        .oneshot(api_request(
            "PUT",
            "/api/thumbnail",
            "secret",
            Body::from(
                serde_json::to_vec(&ThumbnailImportRequest {
                    profile: "main".to_string(),
                    thumbnail,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(import_thumbnail.status(), StatusCode::OK);
    let import_thumbnail = response_json(import_thumbnail).await;
    assert_eq!(import_thumbnail["sha256"], object_hash);
    assert_eq!(import_thumbnail["width"], 256);
}

#[tokio::test]
async fn web_api_object_get_streams_full_and_ranged_reads() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let payload = b"0123456789abcdefghij";
    let hash = sha256_bytes(payload);
    let uri = format!("/api/object?profile=main&sha256={hash}");
    let upload = app
        .clone()
        .oneshot(api_request(
            "PUT",
            &uri,
            "secret",
            Body::from(payload.as_slice()),
        ))
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);

    let full = app
        .clone()
        .oneshot(api_request("GET", &uri, "secret", Body::empty()))
        .await
        .unwrap();
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers()[header::CONTENT_LENGTH], "20");
    assert_eq!(full.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(full.headers()["x-ads-sha256"], hash.as_str());
    assert_eq!(response_bytes(full).await, payload);

    let bounded = app
        .clone()
        .oneshot(ranged_request(&uri, "bytes=2-5"))
        .await
        .unwrap();
    assert_eq!(bounded.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(bounded.headers()[header::CONTENT_RANGE], "bytes 2-5/20");
    assert_eq!(bounded.headers()[header::CONTENT_LENGTH], "4");
    assert_eq!(response_bytes(bounded).await, b"2345");

    let open_ended = app
        .clone()
        .oneshot(ranged_request(&uri, "bytes=15-"))
        .await
        .unwrap();
    assert_eq!(open_ended.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        open_ended.headers()[header::CONTENT_RANGE],
        "bytes 15-19/20"
    );
    assert_eq!(response_bytes(open_ended).await, b"fghij");

    let suffix = app
        .clone()
        .oneshot(ranged_request(&uri, "bytes=-4"))
        .await
        .unwrap();
    assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(suffix.headers()[header::CONTENT_RANGE], "bytes 16-19/20");
    assert_eq!(response_bytes(suffix).await, b"ghij");

    // An end past the object clamps instead of failing.
    let clamped = app
        .clone()
        .oneshot(ranged_request(&uri, "bytes=18-99"))
        .await
        .unwrap();
    assert_eq!(clamped.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(clamped.headers()[header::CONTENT_RANGE], "bytes 18-19/20");
    assert_eq!(response_bytes(clamped).await, b"ij");

    let unsatisfiable = app
        .clone()
        .oneshot(ranged_request(&uri, "bytes=20-"))
        .await
        .unwrap();
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */20");

    // Malformed and multi-range specs are ignored per RFC 9110: full 200.
    for ignored in ["bytes=5-2", "bytes=0-1,3-4", "lines=1-2", "bytes=abc-def"] {
        let response = app
            .clone()
            .oneshot(ranged_request(&uri, ignored))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{ignored}");
        assert_eq!(response_bytes(response).await, payload, "{ignored}");
    }
}

#[tokio::test]
async fn web_api_object_put_streams_chunked_bodies_and_reuses_duplicates() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let payload: Vec<u8> = (0u8..=255).cycle().take(256 * 1024).collect();
    let hash = sha256_bytes(&payload);
    let uri = format!("/api/object?profile=main&sha256={hash}");
    let chunks: Vec<std::result::Result<axum::body::Bytes, std::io::Error>> = payload
        .chunks(64 * 1024)
        .map(|chunk| Ok(axum::body::Bytes::copy_from_slice(chunk)))
        .collect();
    let upload = app
        .clone()
        .oneshot(api_request(
            "PUT",
            &uri,
            "secret",
            Body::from_stream(futures_util::stream::iter(chunks)),
        ))
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
    let upload = response_json(upload).await;
    assert_eq!(upload["reused"], false);
    assert_eq!(upload["size"], 256 * 1024);
    assert_eq!(fs::read(object_path(&store_path, &hash)).unwrap(), payload);

    let again = app
        .clone()
        .oneshot(api_request(
            "PUT",
            &uri,
            "secret",
            Body::from(payload.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(response_json(again).await["reused"], true);
    // Streaming left no temp files next to the finished object.
    assert_eq!(store_object_file_count(&store_path), 1);
}

#[tokio::test]
async fn web_api_object_put_rejects_hash_mismatch_and_cleans_up() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let claimed = sha256_bytes(b"expected bytes");
    let mismatch = app
        .oneshot(api_request(
            "PUT",
            &format!("/api/object?profile=main&sha256={claimed}"),
            "secret",
            Body::from(b"actual bytes".as_slice()),
        ))
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let error = response_json(mismatch).await;
    assert!(
        error["error"].as_str().unwrap().contains("hash mismatch"),
        "{error}"
    );
    assert!(!object_path(&store_path, &claimed).exists());
    assert_eq!(store_object_file_count(&store_path), 0);
}

#[tokio::test]
async fn web_api_object_put_enforces_upload_cap_with_413() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state_with_object_cap(&store_path, &workspace, 8));

    let payload = b"way past the eight byte cap";
    let hash = sha256_bytes(payload);
    let response = app
        .oneshot(api_request(
            "PUT",
            &format!("/api/object?profile=main&sha256={hash}"),
            "secret",
            Body::from(payload.as_slice()),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!object_path(&store_path, &hash).exists());
    assert_eq!(store_object_file_count(&store_path), 0);
}

#[tokio::test]
async fn remote_client_streams_objects_end_to_end() {
    let temp = TempDir::new().unwrap();
    let server_store = temp.path().join("server-store");
    let server_workspace = temp.path().join("server-workspace");
    Store::init(&server_store).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = test_web_state(&server_store, &server_workspace);
    let server = tokio::spawn(async move {
        axum::serve(listener, web_app(state)).await.unwrap();
    });

    let client_store_path = temp.path().join("client-store");
    let client_store = Store::init(&client_store_path).unwrap();
    let fresh_store_path = temp.path().join("fresh-store");
    let fresh_store = Store::init(&fresh_store_path).unwrap();
    let payload: Vec<u8> = (0u8..=255).cycle().take(128 * 1024).collect();
    let sha256 = sha256_bytes(&payload);
    let source = temp.path().join("payload.bin");
    fs::write(&source, &payload).unwrap();

    // RemoteClient is sync ureq, so drive it off the async runtime.
    tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&format!("http://{addr}"), "secret").unwrap();

        let uploaded = client.upload_object("main", &sha256, &source).unwrap();
        assert_eq!(uploaded.size, payload.len() as u64);
        assert!(!uploaded.reused);
        assert_eq!(
            fs::read(object_path(&server_store, &sha256)).unwrap(),
            payload
        );
        let again = client.upload_object("main", &sha256, &source).unwrap();
        assert!(again.reused);

        let downloaded = client.fetch_object("main", &sha256, &client_store).unwrap();
        assert_eq!(downloaded, payload.len() as u64);
        assert_eq!(
            fs::read(object_path(&client_store_path, &sha256)).unwrap(),
            payload
        );

        // A corrupted server object must fail the client's incremental hash
        // verification and leave nothing behind in the destination store.
        fs::write(object_path(&server_store, &sha256), b"corrupted").unwrap();
        let error = client
            .fetch_object("main", &sha256, &fresh_store)
            .unwrap_err();
        assert!(format!("{error:#}").contains("hash mismatch"), "{error:#}");
        assert!(!object_path(&fresh_store_path, &sha256).exists());
        assert_eq!(store_object_file_count(&fresh_store_path), 0);
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn web_api_lists_wips_promotes_and_runs_gc() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let store = Store::init(&store_path).unwrap();
    let department_key = DepartmentKey::new(
        AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    let work = department_folder(&workspace, &department_key);
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("crate.usd"), "wip-1").unwrap();
    store.add_wip_from_source(&work, &department_key).unwrap();
    fs::write(work.join("crate.usd"), "wip-2").unwrap();
    store.add_wip_from_source(&work, &department_key).unwrap();
    drop(store);

    let app = web_app(test_web_state(&store_path, &workspace));
    let wips = app
        .clone()
        .oneshot(api_request(
            "GET",
            "/api/wips?profile=main&category=prop&asset_code=crate&department=model",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(wips.status(), StatusCode::OK);
    let wips = response_json(wips).await;
    assert_eq!(wips["wips"].as_array().unwrap().len(), 2);
    assert_eq!(wips["wips"][1]["seq"], 2);
    assert_eq!(wips["wips"][1]["source_path"], "prop/crate/model");

    // Promote defaults to the head and passes the validation gate.
    let promote = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/promote",
            "secret",
            Body::from(
                r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model"}"#,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(promote.status(), StatusCode::OK);
    let promote = response_json(promote).await;
    assert_eq!(promote["outcome"]["created"], true);
    assert_eq!(promote["outcome"]["version"], 1);
    assert_eq!(promote["validation"]["errors"].as_array().unwrap().len(), 0);

    // Promoting the unchanged head again reuses the publish version.
    let reused = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/promote",
            "secret",
            Body::from(
                r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model"}"#,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(reused.status(), StatusCode::OK);
    let reused = response_json(reused).await;
    assert_eq!(reused["outcome"]["created"], false);
    assert_eq!(reused["outcome"]["version"], 1);

    // GC with retention 1 prunes the older wip; the promoted manifest stays.
    let gc = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/gc",
            "secret",
            Body::from(r#"{"profile":"main","retention":1,"grace_hours":0}"#),
        ))
        .await
        .unwrap();
    assert_eq!(gc.status(), StatusCode::OK);
    let gc = response_json(gc).await;
    assert_eq!(gc["dry_run"], false);
    assert_eq!(gc["pruned_wips"], 1);

    let remaining = app
        .oneshot(api_request(
            "GET",
            "/api/wips?profile=main&category=prop&asset_code=crate&department=model",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    let remaining = response_json(remaining).await;
    assert_eq!(remaining["wips"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["wips"][0]["seq"], 2);
}

#[tokio::test]
async fn web_api_maps_domain_errors_to_404_and_400() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let store = Store::init(&store_path).unwrap();
    let department_key = DepartmentKey::new(
        AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
        "model".to_string(),
    )
    .unwrap();
    fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
    fs::write(
        version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
        "v1",
    )
    .unwrap();
    store
        .add_version_folder(&workspace, &department_key, VersionId(1))
        .unwrap();
    drop(store);
    let app = web_app(test_web_state(&store_path, &workspace));

    // Lookup misses carry the NotFound marker through the anyhow chain.
    for (uri, expected) in [
        (
            "/api/asset?profile=main&category=prop&asset_code=missing",
            "asset not found",
        ),
        (
            "/api/version?profile=main&category=prop&asset_code=crate&department=model&version=v009",
            "department has no selected version",
        ),
        (
            "/api/version?profile=main&category=prop&asset_code=other&department=model",
            "department has no selected version",
        ),
        ("/api/assets?profile=missing", "profile not found"),
    ] {
        let response = app
            .clone()
            .oneshot(api_request("GET", uri, "secret", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        let error = response_json(response).await;
        assert!(
            error["error"].as_str().unwrap().contains(expected),
            "{uri}: {error}"
        );
    }

    let missing_object = app
        .clone()
        .oneshot(api_request(
            "GET",
            &format!("/api/object?profile=main&sha256={}", "0".repeat(64)),
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(missing_object.status(), StatusCode::NOT_FOUND);

    // Input errors keep their message and stay 400.
    let invalid = app
        .oneshot(api_request(
            "GET",
            "/api/version?profile=main&category=..&asset_code=crate&department=model",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let error = response_json(invalid).await;
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("category must not contain . or .. components"),
        "{error}"
    );
}

#[tokio::test]
async fn web_api_sends_cors_headers_only_for_configured_origins() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();

    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/api/profiles")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
            .body(Body::empty())
            .unwrap()
    };
    let simple = |origin: &str| {
        Request::builder()
            .method("GET")
            .uri("/api/profiles")
            .header(header::ORIGIN, origin)
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap()
    };

    // Default: no CORS layer at all, so neither preflight nor simple
    // requests are granted cross-origin access.
    let app = web_app(test_web_state(&store_path, &workspace));
    let response = app
        .clone()
        .oneshot(preflight("https://tools.example.com"))
        .await
        .unwrap();
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
    let response = app
        .oneshot(simple("https://tools.example.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );

    // With the flag, exactly the configured origins are allowed.
    let app = web_app(test_web_state_with_cors(
        &store_path,
        &workspace,
        &["https://tools.example.com"],
    ));
    let response = app
        .clone()
        .oneshot(preflight("https://tools.example.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://tools.example.com"
    );
    let allow_headers = response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allow_headers.contains("authorization"), "{allow_headers}");
    assert!(allow_headers.contains("content-type"), "{allow_headers}");

    let response = app
        .clone()
        .oneshot(simple("https://tools.example.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://tools.example.com"
    );

    let response = app
        .oneshot(simple("https://evil.example.com"))
        .await
        .unwrap();
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
}

#[tokio::test]
async fn web_api_registers_wip_from_uploaded_objects_and_promotes() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let object_bytes = b"wip-usd-1";
    let object_hash = sha256_bytes(object_bytes);
    let upload = app
        .clone()
        .oneshot(api_request(
            "PUT",
            &format!("/api/object?profile=main&sha256={object_hash}"),
            "secret",
            Body::from(object_bytes.as_slice()),
        ))
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);

    let manifest = Manifest {
        entries: vec![ManifestEntry {
            relative_path: "crate.usd".to_string(),
            sha256: object_hash.clone(),
            size: object_bytes.len() as u64,
            mode: 0o666,
        }],
    };
    let wip_request = |source_path: Option<&str>| {
        Body::from(
            serde_json::to_vec(&WipImportRequest {
                profile: "main".to_string(),
                category: "prop".to_string(),
                asset_code: "crate".to_string(),
                department: "model".to_string(),
                manifest: manifest.clone(),
                source_path: source_path.map(str::to_string),
            })
            .unwrap(),
        )
    };

    let registered = app
        .clone()
        .oneshot(api_request("POST", "/api/wip", "secret", wip_request(None)))
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::OK);
    let registered = response_json(registered).await;
    assert_eq!(registered["created"], true);
    assert_eq!(registered["seq"], 1);
    assert_eq!(
        registered["manifest_hash"],
        manifest.canonical_hash().unwrap().as_str()
    );

    // Re-registering the unchanged manifest returns the existing head.
    let unchanged = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/wip",
            "secret",
            wip_request(Some("houdini:/obj/geo1")),
        ))
        .await
        .unwrap();
    assert_eq!(unchanged.status(), StatusCode::OK);
    let unchanged = response_json(unchanged).await;
    assert_eq!(unchanged["created"], false);
    assert_eq!(unchanged["seq"], 1);

    let wips = app
        .clone()
        .oneshot(api_request(
            "GET",
            "/api/wips?profile=main&category=prop&asset_code=crate&department=model",
            "secret",
            Body::empty(),
        ))
        .await
        .unwrap();
    let wips = response_json(wips).await;
    assert_eq!(wips["wips"].as_array().unwrap().len(), 1);
    assert_eq!(wips["wips"][0]["seq"], 1);
    assert_eq!(wips["wips"][0]["source_path"], "prop/crate/model");

    let promote = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/promote",
            "secret",
            Body::from(
                r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model"}"#,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(promote.status(), StatusCode::OK);
    let promote = response_json(promote).await;
    assert_eq!(promote["outcome"]["created"], true);
    assert_eq!(promote["outcome"]["version"], 1);

    // A manifest referencing objects that were never uploaded is rejected
    // with the missing hashes listed.
    let missing_hash = sha256_bytes(b"never uploaded");
    let missing = app
        .oneshot(api_request(
            "POST",
            "/api/wip",
            "secret",
            Body::from(
                serde_json::to_vec(&WipImportRequest {
                    profile: "main".to_string(),
                    category: "prop".to_string(),
                    asset_code: "crate".to_string(),
                    department: "model".to_string(),
                    manifest: Manifest {
                        entries: vec![ManifestEntry {
                            relative_path: "crate.usd".to_string(),
                            sha256: missing_hash.clone(),
                            size: 4,
                            mode: 0o666,
                        }],
                    },
                    source_path: None,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let error = response_json(missing).await;
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("objects missing or invalid"), "{message}");
    assert!(message.contains(&missing_hash), "{message}");
}

#[tokio::test]
async fn web_api_wip_rejects_non_canonical_manifests() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    let app = web_app(test_web_state(&store_path, &workspace));

    let mut hashes = Vec::new();
    for bytes in [b"aaa".as_slice(), b"bbb".as_slice()] {
        let hash = sha256_bytes(bytes);
        let upload = app
            .clone()
            .oneshot(api_request(
                "PUT",
                &format!("/api/object?profile=main&sha256={hash}"),
                "secret",
                Body::from(bytes),
            ))
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::OK);
        hashes.push(hash);
    }
    let entry = |path: &str, hash: &str| ManifestEntry {
        relative_path: path.to_string(),
        sha256: hash.to_string(),
        size: 3,
        mode: 0o666,
    };
    let wip_body = |entries: Vec<ManifestEntry>| {
        Body::from(
            serde_json::to_vec(&WipImportRequest {
                profile: "main".to_string(),
                category: "prop".to_string(),
                asset_code: "crate".to_string(),
                department: "model".to_string(),
                manifest: Manifest { entries },
                source_path: None,
            })
            .unwrap(),
        )
    };

    // Duplicate relative_path: resolve (first match) and checkout (last
    // write) would disagree about the version's content.
    let duplicate = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/wip",
            "secret",
            wip_body(vec![
                entry("crate.usd", &hashes[0]),
                entry("crate.usd", &hashes[1]),
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    let error = response_json(duplicate).await;
    assert!(
        error["error"].as_str().unwrap().contains("more than once"),
        "{error}"
    );

    // Unsorted entries hash differently from the canonical manifest built
    // locally for the same content, silently defeating dedup.
    let unsorted = app
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/wip",
            "secret",
            wip_body(vec![entry("b.usd", &hashes[1]), entry("a.usd", &hashes[0])]),
        ))
        .await
        .unwrap();
    assert_eq!(unsorted.status(), StatusCode::BAD_REQUEST);
    let error = response_json(unsorted).await;
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("sorted by relative_path"),
        "{error}"
    );

    // The same gate guards PUT /api/version, which shares the store-layer
    // validation through import_version_info.
    let manifest = Manifest {
        entries: vec![entry("b.usd", &hashes[1]), entry("a.usd", &hashes[0])],
    };
    let version_info = VersionInfo {
        version: VersionRecord {
            department_key: DepartmentKey::new(
                AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
                "model".to_string(),
            )
            .unwrap(),
            version: VersionId(1),
            manifest_hash: manifest.canonical_hash().unwrap(),
            created_at: Utc::now().to_rfc3339(),
            source_path: "import".to_string(),
            file_count: 2,
            total_bytes: 6,
            promoted_from: None,
        },
        manifest,
    };
    let import = app
        .oneshot(api_request(
            "PUT",
            "/api/version",
            "secret",
            Body::from(
                serde_json::to_vec(&VersionImportRequest {
                    profile: "main".to_string(),
                    version_info,
                })
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(import.status(), StatusCode::BAD_REQUEST);
    let error = response_json(import).await;
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("sorted by relative_path"),
        "{error}"
    );
}

#[tokio::test]
async fn web_api_wip_body_limit_uses_object_cap_and_json_errors() {
    let temp = TempDir::new().unwrap();
    let store_path = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    Store::init(&store_path).unwrap();
    // A cap far below the default 2 MiB proves the route's limit is wired to
    // the configured object cap rather than axum's extractor default.
    let app = web_app(test_web_state_with_object_cap(&store_path, &workspace, 256));

    let filler = "x".repeat(1024);
    let body = format!(
        r#"{{"profile":"main","category":"prop","asset_code":"crate","department":"model","source_path":"{filler}","manifest":{{"entries":[]}}}}"#
    );
    let response = app
        .oneshot(api_request("POST", "/api/wip", "secret", Body::from(body)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Extractor rejections keep the {"error": ...} shape of every other
    // API error instead of axum's plain-text default.
    let error = response_json(response).await;
    assert!(error["error"].is_string(), "{error}");
}

#[test]
fn map_domain_error_treats_rocksdb_errors_as_internal() {
    let temp = TempDir::new().unwrap();
    // A real rocksdb::Error (its constructor is private): read-only opens
    // never create, so pointing one at an empty path must fail.
    let rocks_error = rocksdb::DB::open_for_read_only(
        &rocksdb::Options::default(),
        temp.path().join("missing-db"),
        false,
    )
    .expect_err("read-only open of a missing db must fail");
    let mapped = map_domain_error(anyhow::Error::new(rocks_error));
    assert_eq!(mapped.status, StatusCode::INTERNAL_SERVER_ERROR);
    // The body stays generic: rocksdb messages embed server paths.
    assert_eq!(mapped.message, "internal server error");
}

#[test]
fn exact_len_reader_truncates_growth_and_fails_on_shrink() {
    use std::io::Read;

    // Growth: the source has more bytes than declared; the reader stops at
    // the declared length so the HTTP framing stays intact.
    let mut grown = ExactLenReader {
        inner: std::io::Cursor::new(b"0123456789".to_vec()),
        remaining: 4,
    };
    let mut sent = Vec::new();
    grown.read_to_end(&mut sent).unwrap();
    assert_eq!(sent, b"0123");

    // Shrink: the source ends early; the reader fails fast instead of
    // leaving the server waiting for the missing bytes.
    let mut shrunk = ExactLenReader {
        inner: std::io::Cursor::new(b"01".to_vec()),
        remaining: 4,
    };
    let mut sent = Vec::new();
    let error = shrunk.read_to_end(&mut sent).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

fn test_web_state(store_path: &Path, workspace: &Path) -> Arc<WebState> {
    test_web_state_with_object_cap(store_path, workspace, 1024 * 1024 * 1024)
}

fn test_web_state_with_object_cap(
    store_path: &Path,
    workspace: &Path,
    max_object_upload_bytes: usize,
) -> Arc<WebState> {
    test_web_state_with_config(store_path, workspace, max_object_upload_bytes, Vec::new())
}

fn test_web_state_with_cors(
    store_path: &Path,
    workspace: &Path,
    origins: &[&str],
) -> Arc<WebState> {
    let origins = origins
        .iter()
        .map(|origin| origin.parse().unwrap())
        .collect();
    test_web_state_with_config(store_path, workspace, 1024 * 1024 * 1024, origins)
}

fn test_web_state_with_config(
    store_path: &Path,
    workspace: &Path,
    max_object_upload_bytes: usize,
    cors_allow_origins: Vec<axum::http::HeaderValue>,
) -> Arc<WebState> {
    let profile = ServeProfile::new(
        "main".to_string(),
        store_path.to_path_buf(),
        workspace.to_path_buf(),
    )
    .unwrap();
    Arc::new(
        WebState::try_new(ServeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            auth_token: "secret".to_string(),
            profiles: BTreeMap::from([(profile.name.clone(), profile)]),
            max_upload_bytes: 10 * 1024 * 1024,
            max_object_upload_bytes,
            cors_allow_origins,
        })
        .unwrap(),
    )
}

fn ranged_request(uri: &str, range: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer secret")
        .header(header::RANGE, range)
        .body(Body::empty())
        .unwrap()
}

/// Counts every file under the store's objects tree — finished objects and
/// leftover temps alike — so tests can assert nothing leaked.
fn store_object_file_count(store_path: &Path) -> usize {
    WalkDir::new(objects_root(store_path))
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .count()
}

fn api_request(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn response_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn response_bytes(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn response_json(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn multipart_thumbnail_body(boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in [
        ("profile", "main"),
        ("category", "prop"),
        ("asset_code", "crate"),
        ("department", "model"),
        ("version", "v001"),
    ] {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"thumb.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(test_png_1x1());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn test_png_1x1() -> &'static [u8] {
    &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ]
}
