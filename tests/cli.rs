use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn ads() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ads"))
}

#[test]
fn new_version_add_list_info_checkout_flow() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let checkout = temp.path().join("checkout");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    let created = new_version(&store, &workspace, "char", "hero");
    assert!(created.status.success(), "{}", stderr(&created));
    let v001 = version_folder(&workspace, "char", "hero", "v001");
    assert!(v001.is_dir());

    fs::create_dir(v001.join("geo")).unwrap();
    fs::write(v001.join("geo").join("model.usd"), "usd model").unwrap();
    fs::write(v001.join("notes.txt"), "notes").unwrap();

    let add = add_asset(&store, &workspace, "char", "hero", "v001");
    assert!(add.contains("created v001"));

    let list = ads()
        .arg("list")
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", stderr(&list));
    let list_stdout = stdout(&list);
    assert!(list_stdout.contains("char"));
    assert!(list_stdout.contains("hero"));
    assert!(list_stdout.contains("v001"));

    let info = ads()
        .arg("info")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001")
        .output()
        .unwrap();
    assert!(info.status.success(), "{}", stderr(&info));
    assert!(stdout(&info).contains("geo/model.usd"));
    assert!(stdout(&info).contains("char/hero/model/v001"));

    let checkout_result = checkout_asset(&store, "char", "hero", &checkout, false);
    assert!(
        checkout_result.status.success(),
        "{}",
        stderr(&checkout_result)
    );
    assert_eq!(
        fs::read_to_string(checkout.join("geo").join("model.usd")).unwrap(),
        "usd model"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("notes.txt")).unwrap(),
        "notes"
    );
}

#[test]
fn new_version_copies_latest_into_next_version_folder() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    let created = new_version(&store, &workspace, "prop", "crate");
    assert!(created.status.success(), "{}", stderr(&created));
    let v002_file = version_folder(&workspace, "prop", "crate", "v002").join("model.usd");
    assert_eq!(fs::read_to_string(v002_file).unwrap(), "v1");
}

#[test]
fn new_version_refuses_existing_non_empty_version_folder() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let v001 = version_folder(&workspace, "prop", "crate", "v001");
    fs::create_dir_all(&v001).unwrap();
    fs::write(v001.join("old.txt"), "old").unwrap();

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    let created = new_version(&store, &workspace, "prop", "crate");
    assert!(!created.status.success());
    assert!(stderr(&created).contains("not empty"));
}

#[test]
fn add_uses_version_folders_and_protects_registered_versions() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    let missing = ads()
        .arg("add")
        .arg("--store")
        .arg(&store)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("version folder does not exist"));

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    let v001 = version_folder(&workspace, "prop", "crate", "v001");
    fs::write(v001.join("model.usd"), "v1").unwrap();

    let first = add_asset(&store, &workspace, "prop", "crate", "v001");
    let second = add_asset(&store, &workspace, "prop", "crate", "v001");
    assert!(first.contains("created v001"));
    assert!(second.contains("reused v001"));

    fs::write(v001.join("model.usd"), "mutated v1").unwrap();
    let changed_registered = add_asset_output(&store, &workspace, "prop", "crate", "model", "v001");
    assert!(!changed_registered.status.success());
    assert!(stderr(&changed_registered).contains("different content"));

    let created = new_version(&store, &workspace, "prop", "crate");
    assert!(created.status.success(), "{}", stderr(&created));
    let v002 = version_folder(&workspace, "prop", "crate", "v002");
    fs::write(v002.join("model.usd"), "v2").unwrap();
    let third = add_asset(&store, &workspace, "prop", "crate", "v002");
    assert!(third.contains("created v002"));

    let list = ads()
        .arg("list")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", stderr(&list));
    let output = stdout(&list);
    assert!(output.contains("v001"));
    assert!(output.contains("v002"));
}

#[test]
fn nested_category_paths_create_expected_folders_and_resolve() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let category = "assets/characters/main";

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    let created = new_version(&store, &workspace, category, "hero");
    assert!(created.status.success(), "{}", stderr(&created));
    let v001 = version_folder(&workspace, category, "hero", "v001");
    assert!(v001.is_dir());
    fs::create_dir(v001.join("geo")).unwrap();
    fs::write(v001.join("geo").join("model.usd"), "nested category").unwrap();

    let add = add_asset(&store, &workspace, category, "hero", "v001");
    assert!(add.contains("created v001"));

    let list = ads()
        .arg("list")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains(category));

    let resolved = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://assets/characters/main/hero/model/v001/geo/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_eq!(
        stdout(&resolved).trim(),
        v001.join("geo").join("model.usd").display().to_string()
    );
}

#[test]
fn departments_have_independent_version_sequences() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version_department(&store, &workspace, "char", "hero", "model")
            .status
            .success()
    );
    assert!(
        new_version_department(&store, &workspace, "char", "hero", "anim")
            .status
            .success()
    );
    let model_v001 = department_version_folder(&workspace, "char", "hero", "model", "v001");
    let anim_v001 = department_version_folder(&workspace, "char", "hero", "anim", "v001");
    fs::write(model_v001.join("model.usd"), "model v1").unwrap();
    fs::write(anim_v001.join("anim.usd"), "anim v1").unwrap();

    let model_add = add_asset_department(&store, &workspace, "char", "hero", "model", "v001");
    let anim_add = add_asset_department(&store, &workspace, "char", "hero", "anim", "v001");
    assert!(model_add.contains("created v001 char/hero/model"));
    assert!(anim_add.contains("created v001 char/hero/anim"));

    let model_next = new_version_department(&store, &workspace, "char", "hero", "model");
    assert!(model_next.status.success(), "{}", stderr(&model_next));
    assert!(department_version_folder(&workspace, "char", "hero", "model", "v002").is_dir());
    assert!(!department_version_folder(&workspace, "char", "hero", "anim", "v002").exists());

    let list_model = ads()
        .arg("list")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(list_model.status.success(), "{}", stderr(&list_model));
    let list_model_stdout = stdout(&list_model);
    assert!(list_model_stdout.contains("model"));
    assert!(!list_model_stdout.contains("anim"));

    let log = asset_log(&store, "char", "hero");
    assert!(log.status.success(), "{}", stderr(&log));
    let log_stdout = stdout(&log);
    assert!(log_stdout.contains("\"model\": \"v001\""));
    assert!(log_stdout.contains("\"anim\": \"v001\""));
}

#[test]
fn asset_create_and_asset_log_work_before_versions_exist() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    let created = asset_create(&store, &workspace, "env/city", "street");
    assert!(created.status.success(), "{}", stderr(&created));
    assert!(asset_folder(&workspace, "env/city", "street").is_dir());

    let log = asset_log(&store, "env/city", "street");
    assert!(log.status.success(), "{}", stderr(&log));
    let log_stdout = stdout(&log);
    assert!(log_stdout.contains("\"category\": \"env/city\""));
    assert!(log_stdout.contains("\"asset_code\": \"street\""));
    assert!(log_stdout.contains("\"latest_versions\": {}"));
    assert!(log_stdout.contains("\"versions\": []"));

    let duplicate = asset_create(&store, &workspace, "env/city", "street");
    assert!(!duplicate.status.success());
    assert!(stderr(&duplicate).contains("already exists"));

    let new_version = new_version(&store, &workspace, "env/city", "street");
    assert!(new_version.status.success(), "{}", stderr(&new_version));
    assert!(version_folder(&workspace, "env/city", "street", "v001").is_dir());
}

#[test]
fn pull_restores_latest_and_requires_force_for_different_content() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    let v002 = version_folder(&workspace, "prop", "crate", "v002");
    fs::write(v002.join("model.usd"), "v2").unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    fs::remove_dir_all(&v002).unwrap();
    let pulled = pull_latest(&store, &workspace, false);
    assert!(pulled.status.success(), "{}", stderr(&pulled));
    assert_eq!(fs::read_to_string(v002.join("model.usd")).unwrap(), "v2");

    let same = pull_latest(&store, &workspace, false);
    assert!(same.status.success(), "{}", stderr(&same));
    assert!(stdout(&same).contains("already pulled"));

    fs::write(v002.join("model.usd"), "local edits").unwrap();
    let blocked = pull_latest(&store, &workspace, false);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("not empty"));

    let forced = pull_latest(&store, &workspace, true);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(fs::read_to_string(v002.join("model.usd")).unwrap(), "v2");

    let v001 = version_folder(&workspace, "prop", "crate", "v001");
    fs::remove_dir_all(&v001).unwrap();
    let restored = restore_version(&store, &workspace, "v001", false);
    assert!(restored.status.success(), "{}", stderr(&restored));
    assert!(stdout(&restored).contains("restored v001"));
    assert_eq!(fs::read_to_string(v001.join("model.usd")).unwrap(), "v1");
}

#[test]
fn current_pointer_defaults_to_latest_and_can_be_pinned() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("crate.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("crate.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    fs::remove_dir_all(version_folder(&workspace, "prop", "crate", "v002")).unwrap();
    let materialized_default = pull_current(&store, &workspace, false);
    assert!(
        materialized_default.status.success(),
        "{}",
        stderr(&materialized_default)
    );
    assert_eq!(
        fs::read_to_string(version_folder(&workspace, "prop", "crate", "v002").join("crate.usd"))
            .unwrap(),
        "v2"
    );

    let set = ads()
        .arg("current")
        .arg("set")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001")
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", stderr(&set));
    assert!(stdout(&set).contains("latest=v002"));

    let get = current_get(&store, "prop", "crate", "model");
    assert!(get.status.success(), "{}", stderr(&get));
    assert_eq!(stdout(&get).trim(), "v001");

    fs::remove_dir_all(version_folder(&workspace, "prop", "crate", "v001")).unwrap();
    let materialized_pinned = pull_current(&store, &workspace, false);
    assert!(
        materialized_pinned.status.success(),
        "{}",
        stderr(&materialized_pinned)
    );
    assert_eq!(
        fs::read_to_string(version_folder(&workspace, "prop", "crate", "v001").join("crate.usd"))
            .unwrap(),
        "v1"
    );

    let status = ads()
        .arg("current")
        .arg("status")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", stderr(&status));
    let status_stdout = stdout(&status);
    assert!(status_stdout.contains("v001"));
    assert!(status_stdout.contains("v002"));
    assert!(status_stdout.contains("explicit"));

    let reset = ads()
        .arg("current")
        .arg("reset")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(reset.status.success(), "{}", stderr(&reset));

    let get_after_reset = current_get(&store, "prop", "crate", "model");
    assert!(
        get_after_reset.status.success(),
        "{}",
        stderr(&get_after_reset)
    );
    assert_eq!(stdout(&get_after_reset).trim(), "v002");
}

#[test]
fn resolve_supports_local_remote_auto_and_latest_asset_paths() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    let local = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/v001/model.usd",
    );
    assert!(local.status.success(), "{}", stderr(&local));
    assert_eq!(
        stdout(&local).trim(),
        version_folder(&workspace, "prop", "crate", "v001")
            .join("model.usd")
            .display()
            .to_string()
    );

    let remote = resolve_asset(
        &store,
        &workspace,
        "remote",
        Some("https://assets.example.com/objects/sha256"),
        "prop/crate/model/v001/model.usd",
    );
    assert!(remote.status.success(), "{}", stderr(&remote));
    let v1_hash = sha256_hex(b"v1");
    assert_eq!(
        stdout(&remote).trim(),
        format!(
            "https://assets.example.com/objects/sha256/{}/{}",
            &v1_hash[0..2],
            v1_hash
        )
    );

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("model.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    let latest = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/latest/model.usd",
    );
    assert!(latest.status.success(), "{}", stderr(&latest));
    assert_eq!(
        stdout(&latest).trim(),
        version_folder(&workspace, "prop", "crate", "v002")
            .join("model.usd")
            .display()
            .to_string()
    );

    fs::remove_dir_all(version_folder(&workspace, "prop", "crate", "v002")).unwrap();
    let local_missing = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/latest/model.usd",
    );
    assert!(!local_missing.status.success());
    assert!(stderr(&local_missing).contains("does not exist"));
}

#[test]
fn resolve_accepts_simplified_uri_current_and_query_version() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("crate.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("crate.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    let implicit_current_without_category = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://crate/model/crate.usd",
    );
    assert!(
        implicit_current_without_category.status.success(),
        "{}",
        stderr(&implicit_current_without_category)
    );
    assert_eq!(
        stdout(&implicit_current_without_category).trim(),
        version_folder(&workspace, "prop", "crate", "v002")
            .join("crate.usd")
            .display()
            .to_string()
    );

    let explicit_v001 = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/crate.usd?v=v001",
    );
    assert!(explicit_v001.status.success(), "{}", stderr(&explicit_v001));
    assert_eq!(
        stdout(&explicit_v001).trim(),
        version_folder(&workspace, "prop", "crate", "v001")
            .join("crate.usd")
            .display()
            .to_string()
    );

    let default_file = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model?v=v001",
    );
    assert!(default_file.status.success(), "{}", stderr(&default_file));
    assert_eq!(
        stdout(&default_file).trim(),
        version_folder(&workspace, "prop", "crate", "v001")
            .join("crate.usd")
            .display()
            .to_string()
    );

    let set_current = ads()
        .arg("current")
        .arg("set")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001")
        .output()
        .unwrap();
    assert!(set_current.status.success(), "{}", stderr(&set_current));

    let implicit_current = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/crate.usd",
    );
    assert!(
        implicit_current.status.success(),
        "{}",
        stderr(&implicit_current)
    );
    assert_eq!(
        stdout(&implicit_current).trim(),
        version_folder(&workspace, "prop", "crate", "v001")
            .join("crate.usd")
            .display()
            .to_string()
    );

    let explicit_latest = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/crate.usd?v=latest",
    );
    assert!(
        explicit_latest.status.success(),
        "{}",
        stderr(&explicit_latest)
    );
    assert_eq!(
        stdout(&explicit_latest).trim(),
        version_folder(&workspace, "prop", "crate", "v002")
            .join("crate.usd")
            .display()
            .to_string()
    );
}

#[test]
fn thumbnails_are_version_metadata_and_use_object_store() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let out = temp.path().join("thumb-out.png");
    let thumb = temp.path().join("thumb.png");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");
    fs::write(&thumb, png_1x1()).unwrap();

    let set = thumbnail_set(&store, "prop", "crate", "model", "v001", &thumb);
    assert!(set.status.success(), "{}", stderr(&set));
    assert!(stdout(&set).contains("image/png"));

    let info = ads()
        .arg("thumbnail")
        .arg("info")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(info.status.success(), "{}", stderr(&info));
    let info_stdout = stdout(&info);
    assert!(info_stdout.contains("\"version\": \"v001\""));
    assert!(info_stdout.contains("\"mime_type\": \"image/png\""));
    assert!(info_stdout.contains("\"width\": 1"));
    assert!(info_stdout.contains("\"height\": 1"));

    let get = thumbnail_get(&store, "prop", "crate", "model", None, &out, false);
    assert!(get.status.success(), "{}", stderr(&get));
    assert_eq!(fs::read(&out).unwrap(), png_1x1());

    let blocked = thumbnail_get(&store, "prop", "crate", "model", None, &out, false);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("destination exists"));

    let forced = thumbnail_get(&store, "prop", "crate", "model", None, &out, true);
    assert!(forced.status.success(), "{}", stderr(&forced));

    let list = ads()
        .arg("thumbnail")
        .arg("list")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", stderr(&list));
    let list_stdout = stdout(&list);
    assert!(list_stdout.contains("v001"));
    assert!(list_stdout.contains("image/png"));

    let thumb_hash = sha256_hex(png_1x1());
    assert!(
        store
            .join("objects")
            .join("sha256")
            .join(&thumb_hash[0..2])
            .join(&thumb_hash)
            .exists()
    );

    let remove = ads()
        .arg("thumbnail")
        .arg("remove")
        .arg("--store")
        .arg(&store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001")
        .output()
        .unwrap();
    assert!(remove.status.success(), "{}", stderr(&remove));

    let missing = thumbnail_get(&store, "prop", "crate", "model", Some("v001"), &out, true);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("thumbnail not found"));
}

#[test]
fn verify_detects_missing_thumbnail_object() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let thumb = temp.path().join("thumb.png");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");
    fs::write(&thumb, png_1x1()).unwrap();
    let set = thumbnail_set(&store, "prop", "crate", "model", "v001", &thumb);
    assert!(set.status.success(), "{}", stderr(&set));

    let thumb_hash = sha256_hex(png_1x1());
    fs::remove_file(
        store
            .join("objects")
            .join("sha256")
            .join(&thumb_hash[0..2])
            .join(&thumb_hash),
    )
    .unwrap();

    let verify = ads()
        .arg("verify")
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(!verify.status.success());
    assert!(stderr(&verify).contains("thumbnail object missing"));
}

#[test]
fn thumbnail_url_uses_configured_or_overridden_remote_base_url() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let thumb = temp.path().join("thumb.png");
    assert!(
        ads()
            .arg("init")
            .arg(&store)
            .arg("--remote-base-url")
            .arg("https://assets.example.com/objects/sha256/")
            .status()
            .unwrap()
            .success()
    );

    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");
    fs::write(&thumb, png_1x1()).unwrap();
    let set = thumbnail_set(&store, "prop", "crate", "model", "v001", &thumb);
    assert!(set.status.success(), "{}", stderr(&set));

    let thumb_hash = sha256_hex(png_1x1());
    let configured = thumbnail_url(&store, "prop", "crate", "model", None, None);
    assert!(configured.status.success(), "{}", stderr(&configured));
    assert_eq!(
        stdout(&configured).trim(),
        format!(
            "https://assets.example.com/objects/sha256/{}/{}",
            &thumb_hash[0..2],
            thumb_hash
        )
    );

    let overridden = thumbnail_url(
        &store,
        "prop",
        "crate",
        "model",
        Some("v001"),
        Some("https://cdn.example.com/thumbs/"),
    );
    assert!(overridden.status.success(), "{}", stderr(&overridden));
    assert_eq!(
        stdout(&overridden).trim(),
        format!(
            "https://cdn.example.com/thumbs/{}/{}",
            &thumb_hash[0..2],
            thumb_hash
        )
    );
}

#[test]
fn set_remote_config_is_used_by_auto_resolve_when_local_file_is_missing() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(
        ads()
            .arg("init")
            .arg(&store)
            .arg("--remote-base-url")
            .arg("https://assets.example.com/objects/sha256/")
            .status()
            .unwrap()
            .success()
    );

    assert!(
        new_version(&store, &workspace, "char", "hero")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "char", "hero", "v001").join("model.usd"),
        "hero",
    )
    .unwrap();
    add_asset(&store, &workspace, "char", "hero", "v001");
    fs::remove_dir_all(version_folder(&workspace, "char", "hero", "v001")).unwrap();

    let resolved = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://char/hero/model/v001/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    let hero_hash = sha256_hex(b"hero");
    assert_eq!(
        stdout(&resolved).trim(),
        format!(
            "https://assets.example.com/objects/sha256/{}/{}",
            &hero_hash[0..2],
            hero_hash
        )
    );

    let set_remote = ads()
        .arg("set-remote")
        .arg("--store")
        .arg(&store)
        .arg("--remote-base-url")
        .arg("https://cdn.example.com/assets")
        .output()
        .unwrap();
    assert!(set_remote.status.success(), "{}", stderr(&set_remote));
    let resolved_after_update = resolve_asset(
        &store,
        &workspace,
        "remote",
        None,
        "ads://char/hero/model/v001/model.usd",
    );
    assert!(
        resolved_after_update.status.success(),
        "{}",
        stderr(&resolved_after_update)
    );
    assert!(stdout(&resolved_after_update).starts_with("https://cdn.example.com/assets/"));
}

#[test]
fn checkout_refuses_non_empty_destination_unless_forced() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let checkout = temp.path().join("checkout");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("old.txt"), "old").unwrap();

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "asset",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    let blocked = checkout_asset(&store, "prop", "crate", &checkout, false);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("not empty"));

    let forced = checkout_asset(&store, "prop", "crate", &checkout, true);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(!checkout.join("old.txt").exists());
    assert_eq!(
        fs::read_to_string(checkout.join("model.usd")).unwrap(),
        "asset"
    );
}

#[test]
fn verify_detects_missing_object() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    assert!(
        new_version(&store, &workspace, "prop", "crate")
            .status
            .success()
    );
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "asset",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    let objects = store.join("objects").join("sha256");
    let object_file = find_first_file(&objects);
    fs::remove_file(object_file).unwrap();

    let verify = ads()
        .arg("verify")
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(!verify.status.success());
    assert!(stderr(&verify).contains("object missing"));
}

fn new_version(store: &Path, workspace: &Path, category: &str, asset_code: &str) -> Output {
    new_version_department(store, workspace, category, asset_code, "model")
}

fn new_version_department(
    store: &Path,
    workspace: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
) -> Output {
    ads()
        .arg("new-version")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department)
        .output()
        .unwrap()
}

fn asset_create(store: &Path, workspace: &Path, category: &str, asset_code: &str) -> Output {
    ads()
        .arg("asset")
        .arg("create")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .output()
        .unwrap()
}

fn asset_log(store: &Path, category: &str, asset_code: &str) -> Output {
    ads()
        .arg("asset")
        .arg("log")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .output()
        .unwrap()
}

fn add_asset(
    store: &Path,
    workspace: &Path,
    category: &str,
    asset_code: &str,
    version: &str,
) -> String {
    add_asset_department(store, workspace, category, asset_code, "model", version)
}

fn add_asset_department(
    store: &Path,
    workspace: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: &str,
) -> String {
    let output = add_asset_output(store, workspace, category, asset_code, department, version);
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output)
}

fn add_asset_output(
    store: &Path,
    workspace: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: &str,
) -> Output {
    ads()
        .arg("add")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department)
        .arg("--version")
        .arg(version)
        .output()
        .unwrap()
}

fn pull_latest(store: &Path, workspace: &Path, force: bool) -> Output {
    let mut command = ads();
    command
        .arg("pull")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--latest");
    if force {
        command.arg("--force");
    }
    command.output().unwrap()
}

fn pull_current(store: &Path, workspace: &Path, force: bool) -> Output {
    let mut command = ads();
    command
        .arg("pull")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model");
    if force {
        command.arg("--force");
    }
    command.output().unwrap()
}

fn restore_version(store: &Path, workspace: &Path, version: &str, force: bool) -> Output {
    let mut command = ads();
    command
        .arg("restore")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg(version);
    if force {
        command.arg("--force");
    }
    command.output().unwrap()
}

fn current_get(store: &Path, category: &str, asset_code: &str, department: &str) -> Output {
    ads()
        .arg("current")
        .arg("get")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department)
        .output()
        .unwrap()
}

fn thumbnail_set(
    store: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: &str,
    image: &Path,
) -> Output {
    ads()
        .arg("thumbnail")
        .arg("set")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department)
        .arg("--version")
        .arg(version)
        .arg(image)
        .output()
        .unwrap()
}

fn thumbnail_get(
    store: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: Option<&str>,
    dest: &Path,
    force: bool,
) -> Output {
    let mut command = ads();
    command
        .arg("thumbnail")
        .arg("get")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department);
    if let Some(version) = version {
        command.arg("--version").arg(version);
    }
    if force {
        command.arg("--force");
    }
    command.arg(dest).output().unwrap()
}

fn thumbnail_url(
    store: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: Option<&str>,
    remote_base_url: Option<&str>,
) -> Output {
    let mut command = ads();
    command
        .arg("thumbnail")
        .arg("url")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg(department);
    if let Some(version) = version {
        command.arg("--version").arg(version);
    }
    if let Some(remote_base_url) = remote_base_url {
        command.arg("--remote-base-url").arg(remote_base_url);
    }
    command.output().unwrap()
}

fn resolve_asset(
    store: &Path,
    workspace: &Path,
    mode: &str,
    remote_base_url: Option<&str>,
    asset_path: &str,
) -> Output {
    let mut command = ads();
    command
        .arg("resolve")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--mode")
        .arg(mode);
    if let Some(remote_base_url) = remote_base_url {
        command.arg("--remote-base-url").arg(remote_base_url);
    }
    command.arg(asset_path).output().unwrap()
}

fn checkout_asset(
    store: &Path,
    category: &str,
    asset_code: &str,
    dest: &Path,
    force: bool,
) -> Output {
    let mut command = ads();
    command
        .arg("checkout")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg(category)
        .arg("--asset-code")
        .arg(asset_code)
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg("v001");
    if force {
        command.arg("--force");
    }
    command.arg(dest).output().unwrap()
}

fn version_folder(workspace: &Path, category: &str, asset_code: &str, version: &str) -> PathBuf {
    department_version_folder(workspace, category, asset_code, "model", version)
}

fn department_version_folder(
    workspace: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
    version: &str,
) -> PathBuf {
    let mut path = workspace.to_path_buf();
    for component in category.split('/') {
        path.push(component);
    }
    path.join(asset_code).join(department).join(version)
}

fn asset_folder(workspace: &Path, category: &str, asset_code: &str) -> PathBuf {
    let mut path = workspace.to_path_buf();
    for component in category.split('/') {
        path.push(component);
    }
    path.join(asset_code)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn find_first_file(root: &Path) -> PathBuf {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_dir() {
            let nested = find_first_file(&entry.path());
            if nested.exists() {
                return nested;
            }
        } else {
            return entry.path();
        }
    }
    panic!("no object file found");
}

fn png_1x1() -> &'static [u8] {
    &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ]
}
