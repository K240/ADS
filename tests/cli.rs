use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn ads() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ads"))
}

struct ServeGuard {
    child: Child,
}

impl ServeGuard {
    fn spawn(addr: &str, store: &Path, workspace: &Path) -> Self {
        let child = ads()
            .arg("serve")
            .arg("--bind")
            .arg(addr)
            .arg("--auth-token")
            .arg("secret")
            .arg("--store")
            .arg(store)
            .arg("--workspace")
            .arg(workspace)
            .spawn()
            .unwrap();
        Self { child }
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_loopback_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

fn retry_command_success(mut run: impl FnMut() -> Output) -> Output {
    let mut last = run();
    for _ in 0..49 {
        if last.status.success() {
            return last;
        }
        thread::sleep(Duration::from_millis(100));
        last = run();
    }
    last
}

#[test]
fn new_version_add_list_info_checkout_flow() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let checkout = temp.path().join("checkout");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    new_version(&store, &workspace, "char", "hero");
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

    new_version(&store, &workspace, "prop", "crate");
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

    new_version(&store, &workspace, "prop", "crate");
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
fn sync_uses_resolver_environment_defaults() {
    let temp = TempDir::new().unwrap();
    let server_store = temp.path().join("server-store");
    let server_workspace = temp.path().join("server-workspace");
    let local_store = temp.path().join("local-store");

    assert!(
        ads()
            .arg("init")
            .arg(&server_store)
            .status()
            .unwrap()
            .success()
    );
    new_version(&server_store, &server_workspace, "prop", "crate");
    fs::write(
        version_folder(&server_workspace, "prop", "crate", "v001").join("crate.usd"),
        "server v1",
    )
    .unwrap();
    let add = add_asset(&server_store, &server_workspace, "prop", "crate", "v001");
    assert!(add.contains("created v001"));

    let addr = free_loopback_addr();
    let _server = ServeGuard::spawn(&addr, &server_store, &server_workspace);
    let server_url = format!("http://{addr}");

    let sync = retry_command_success(|| {
        let mut command = ads();
        command
            .arg("sync")
            .env("ADS_RESOLVER_SERVER", &server_url)
            .env("ADS_RESOLVER_API_TOKEN", "secret")
            .env("ADS_RESOLVER_PROFILE", "default")
            .env("ADS_RESOLVER_STORE", &local_store)
            .env_remove("ADS_WEB_TOKEN");
        command.output().unwrap()
    });
    assert!(sync.status.success(), "{}", stderr(&sync));
    assert!(stdout(&sync).contains("synced assets=1 versions=1"));

    let info = ads()
        .arg("info")
        .arg("--store")
        .arg(&local_store)
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
    assert!(info.status.success(), "{}", stderr(&info));
    assert!(stdout(&info).contains("crate.usd"));
}

#[test]
fn nested_category_paths_create_expected_folders_and_resolve() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let category = "assets/characters/main";

    assert!(ads().arg("init").arg(&store).status().unwrap().success());
    new_version(&store, &workspace, category, "hero");
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
    assert_view_resolution(&workspace, &resolved, "geo/model.usd", "nested category");
}

#[test]
fn departments_have_independent_version_sequences() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");

    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version_department(&store, &workspace, "char", "hero", "model");
    new_version_department(&store, &workspace, "char", "hero", "anim");
    let model_v001 = department_version_folder(&workspace, "char", "hero", "model", "v001");
    let anim_v001 = department_version_folder(&workspace, "char", "hero", "anim", "v001");
    fs::write(model_v001.join("model.usd"), "model v1").unwrap();
    fs::write(anim_v001.join("anim.usd"), "anim v1").unwrap();

    let model_add = add_asset_department(&store, &workspace, "char", "hero", "model", "v001");
    let anim_add = add_asset_department(&store, &workspace, "char", "hero", "anim", "v001");
    assert!(model_add.contains("created v001 char/hero/model"));
    assert!(anim_add.contains("created v001 char/hero/anim"));

    new_version_department(&store, &workspace, "char", "hero", "model");
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
    assert!(log_stdout.contains("\"model\": 1"));
    assert!(log_stdout.contains("\"anim\": 1"));
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

    new_version(&store, &workspace, "env/city", "street");
    assert!(version_folder(&workspace, "env/city", "street", "v001").is_dir());
}

#[test]
fn checkout_materializes_current_latest_and_respects_force() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("model.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    let dest = temp.path().join("out");
    let current = checkout_asset(&store, "prop", "crate", &dest, false);
    assert!(current.status.success(), "{}", stderr(&current));
    assert_eq!(fs::read_to_string(dest.join("model.usd")).unwrap(), "v2");

    fs::write(dest.join("model.usd"), "local edits").unwrap();
    let blocked = checkout_asset(&store, "prop", "crate", &dest, false);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("not empty"));
    let forced = checkout_asset(&store, "prop", "crate", &dest, true);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(fs::read_to_string(dest.join("model.usd")).unwrap(), "v2");

    let pinned_dest = temp.path().join("out-v1");
    let pinned = checkout_version(&store, "1", &pinned_dest);
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    assert_eq!(
        fs::read_to_string(pinned_dest.join("model.usd")).unwrap(),
        "v1"
    );
}

#[test]
fn wip_stream_promotes_resolves_and_gc_prunes() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let staging = temp.path().join("staging");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    // Three writes into the WIP stream; the unchanged middle write is deduped.
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("hero.usd"), "wip-1").unwrap();
    let first = wip_add(&store, &staging);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stdout(&first).contains("registered wip seq=1"));

    let unchanged = wip_add(&store, &staging);
    assert!(unchanged.status.success(), "{}", stderr(&unchanged));
    assert!(stdout(&unchanged).contains("unchanged wip seq=1"));

    fs::write(staging.join("hero.usd"), "wip-2").unwrap();
    let second = wip_add(&store, &staging);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stdout(&second).contains("registered wip seq=2"));

    // ?v=wip resolves the head through the manifest view (local-only).
    let wip_resolved = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://char/hero/model/hero.usd?v=wip",
    );
    assert!(wip_resolved.status.success(), "{}", stderr(&wip_resolved));
    assert_view_resolution(&workspace, &wip_resolved, "hero.usd", "wip-2");

    let remote_blocked = resolve_asset(
        &store,
        &workspace,
        "remote",
        None,
        "ads://char/hero/model/hero.usd?v=wip",
    );
    assert!(!remote_blocked.status.success());
    assert!(stderr(&remote_blocked).contains("local-only"));

    // Promotion assigns the next publish version without copying files.
    let promoted = publish_promote(&store, false);
    assert!(promoted.status.success(), "{}", stderr(&promoted));
    assert!(stdout(&promoted).contains("promoted v001"));
    let promoted_again = publish_promote(&store, false);
    assert!(
        promoted_again.status.success(),
        "{}",
        stderr(&promoted_again)
    );
    assert!(stdout(&promoted_again).contains("reused v001"));
    let resolved = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://char/hero/model/hero.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_view_resolution(&workspace, &resolved, "hero.usd", "wip-2");

    // GC with retention 1: the dry run reports without deleting, the real
    // run prunes the old micro-version and its now-orphaned object.
    let dry = gc_run(&store, 1, 0, true);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert!(stdout(&dry).contains("\"pruned_wips\": 1"));
    assert!(stdout(&dry).contains("\"deleted_objects\": 1"));
    let wips = wip_list(&store);
    assert_eq!(stdout(&wips).matches("\"seq\"").count(), 2);

    let swept = gc_run(&store, 1, 0, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(stdout(&swept).contains("\"pruned_wips\": 1"));
    let wips = wip_list(&store);
    assert_eq!(stdout(&wips).matches("\"seq\"").count(), 1);

    // The promoted version still resolves after the sweep: its objects are roots.
    let after = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://char/hero/model/hero.usd?v=1",
    );
    assert!(after.status.success(), "{}", stderr(&after));
}

#[test]
fn publish_validate_accepts_ads_and_manifest_internal_references() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let staging = temp.path().join("staging");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    // ads:// for cross-asset references, relative for siblings of the same
    // version: both are valid under the v8 policy.
    fs::create_dir_all(staging.join("geo")).unwrap();
    fs::write(
        staging.join("hero.usda"),
        r#"def "Hero" (references = [@ads://prop/crate/model/crate.usd?v=2@, @geo/body.usd@]) {}"#,
    )
    .unwrap();
    fs::write(staging.join("geo").join("body.usd"), "body").unwrap();

    let source_check = publish_validate_source(&staging);
    assert!(source_check.status.success(), "{}", stderr(&source_check));
    assert!(stdout(&source_check).contains("references_checked=2"));

    let registered = wip_add(&store, &staging);
    assert!(registered.status.success(), "{}", stderr(&registered));
    let promoted = publish_promote(&store, false);
    assert!(promoted.status.success(), "{}", stderr(&promoted));
    assert!(stdout(&promoted).contains("promoted v001"));

    let validate = publish_validate_version(&store, "1");
    assert!(validate.status.success(), "{}", stderr(&validate));
    assert!(stdout(&validate).contains("references_checked=2"));
}

#[test]
fn promote_gate_rejects_escaping_and_absolute_references() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let staging = temp.path().join("staging");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    fs::create_dir_all(&staging).unwrap();
    fs::write(
        staging.join("hero.usda"),
        r#"def "Hero" (references = [@../outside/body.usd@, @D:/abs/tex.tx@]) {}"#,
    )
    .unwrap();

    let registered = wip_add(&store, &staging);
    assert!(registered.status.success(), "{}", stderr(&registered));

    // The default promote gate refuses to publish unmanaged references.
    let blocked = publish_promote(&store, false);
    assert!(!blocked.status.success());
    let err = stderr(&blocked);
    assert!(err.contains("escapes the version root"));
    assert!(err.contains("absolute path"));
    assert!(err.contains("promote validation gate failed"));

    // --no-validate bypasses the gate explicitly.
    let forced = publish_promote(&store, true);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(stdout(&forced).contains("promoted v001"));
}

#[test]
fn current_pointer_defaults_to_latest_and_can_be_pinned() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("crate.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("crate.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");

    let default_dest = temp.path().join("out-default");
    let materialized_default = checkout_asset(&store, "prop", "crate", &default_dest, false);
    assert!(
        materialized_default.status.success(),
        "{}",
        stderr(&materialized_default)
    );
    assert_eq!(
        fs::read_to_string(default_dest.join("crate.usd")).unwrap(),
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

    let pinned_dest = temp.path().join("out-pinned");
    let materialized_pinned = checkout_asset(&store, "prop", "crate", &pinned_dest, false);
    assert!(
        materialized_pinned.status.success(),
        "{}",
        stderr(&materialized_pinned)
    );
    assert_eq!(
        fs::read_to_string(pinned_dest.join("crate.usd")).unwrap(),
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

    new_version(&store, &workspace, "prop", "crate");
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
    assert_view_resolution(&workspace, &local, "model.usd", "v1");

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

    new_version(&store, &workspace, "prop", "crate");
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
    assert_view_resolution(&workspace, &latest, "model.usd", "v2");

    // v8: removing the workspace folder no longer breaks local resolve —
    // the manifest view is materialized from the store instead.
    fs::remove_dir_all(version_folder(&workspace, "prop", "crate", "v002")).unwrap();
    let local_after_workspace_removal = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/latest/model.usd",
    );
    assert!(
        local_after_workspace_removal.status.success(),
        "{}",
        stderr(&local_after_workspace_removal)
    );
    assert_view_resolution(
        &workspace,
        &local_after_workspace_removal,
        "model.usd",
        "v2",
    );
}

#[test]
fn resolve_textures_to_hash_cache_path() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version_department(&store, &workspace, "prop", "crate", "texture");
    let texture_v001 = department_version_folder(&workspace, "prop", "crate", "texture", "v001");
    fs::create_dir(texture_v001.join("maps")).unwrap();
    fs::write(
        texture_v001.join("maps").join("body_diffuse.1001.tx"),
        "texture payload",
    )
    .unwrap();
    add_asset_department(&store, &workspace, "prop", "crate", "texture", "v001");
    fs::remove_dir_all(&texture_v001).unwrap();

    let resolved = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/texture/maps/body_diffuse.1001.tx",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    let texture_hash = sha256_hex(b"texture payload");
    let expected = workspace
        .join(".ads-cache")
        .join("sha256")
        .join(&texture_hash[0..2])
        .join(format!("{texture_hash}.tx"));
    assert_eq!(stdout(&resolved).trim(), expected.display().to_string());
    assert_eq!(fs::read_to_string(expected).unwrap(), "texture payload");
}

#[test]
fn resolving_updated_texture_name_uses_new_hash_cache_file() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version_department(&store, &workspace, "prop", "crate", "texture");
    let v001 = department_version_folder(&workspace, "prop", "crate", "texture", "v001");
    fs::write(v001.join("body_diffuse.1001.tx"), "texture v1").unwrap();
    add_asset_department(&store, &workspace, "prop", "crate", "texture", "v001");

    new_version_department(&store, &workspace, "prop", "crate", "texture");
    let v002 = department_version_folder(&workspace, "prop", "crate", "texture", "v002");
    fs::write(v002.join("body_diffuse.1001.tx"), "texture v2").unwrap();
    add_asset_department(&store, &workspace, "prop", "crate", "texture", "v002");

    fs::remove_dir_all(&v001).unwrap();
    fs::remove_dir_all(&v002).unwrap();

    let current = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/texture/body_diffuse.1001.tx",
    );
    assert!(current.status.success(), "{}", stderr(&current));
    let v2_hash = sha256_hex(b"texture v2");
    let v2_cache = workspace
        .join(".ads-cache")
        .join("sha256")
        .join(&v2_hash[0..2])
        .join(format!("{v2_hash}.tx"));
    assert_eq!(stdout(&current).trim(), v2_cache.display().to_string());
    assert_eq!(fs::read_to_string(&v2_cache).unwrap(), "texture v2");

    let pinned = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/texture/body_diffuse.1001.tx?v=v001",
    );
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    let v1_hash = sha256_hex(b"texture v1");
    let v1_cache = workspace
        .join(".ads-cache")
        .join("sha256")
        .join(&v1_hash[0..2])
        .join(format!("{v1_hash}.tx"));
    assert_eq!(stdout(&pinned).trim(), v1_cache.display().to_string());
    assert_eq!(fs::read_to_string(&v1_cache).unwrap(), "texture v1");
    assert_ne!(v1_cache, v2_cache);
}

#[test]
fn resolve_accepts_simplified_uri_current_and_query_version() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("crate.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    new_version(&store, &workspace, "prop", "crate");
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
    assert_view_resolution(
        &workspace,
        &implicit_current_without_category,
        "crate.usd",
        "v2",
    );

    let explicit_v001 = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/crate.usd?v=v001",
    );
    assert!(explicit_v001.status.success(), "{}", stderr(&explicit_v001));
    assert_view_resolution(&workspace, &explicit_v001, "crate.usd", "v1");

    let default_file = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model?v=v001",
    );
    assert!(default_file.status.success(), "{}", stderr(&default_file));
    assert_view_resolution(&workspace, &default_file, "crate.usd", "v1");

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
    assert_view_resolution(&workspace, &implicit_current, "crate.usd", "v1");

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
    assert_view_resolution(&workspace, &explicit_latest, "crate.usd", "v2");
}

#[test]
fn cache_gc_prunes_stale_views_blobs_and_staging_runs() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    // Two versions; resolving each materializes its manifest view.
    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");
    let pinned = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd?v=1",
    );
    assert!(pinned.status.success(), "{}", stderr(&pinned));

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v002").join("model.usd"),
        "v2",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v002");
    let current = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(current.status.success(), "{}", stderr(&current));

    // A stale staging run left behind by a crashed write.
    fs::create_dir_all(workspace.join(".ads-staging").join("run-dead")).unwrap();
    fs::write(
        workspace
            .join(".ads-staging")
            .join("run-dead")
            .join("x.usd"),
        "orphan",
    )
    .unwrap();

    // Dry run reports the v001 view and blob without deleting anything.
    let dry = cache_gc_run(&store, &workspace, true);
    assert!(dry.status.success(), "{}", stderr(&dry));
    let dry_stdout = stdout(&dry);
    assert!(dry_stdout.contains("\"retained_views\": 1"), "{dry_stdout}");
    assert!(dry_stdout.contains("\"deleted_views\": 1"), "{dry_stdout}");
    assert!(dry_stdout.contains("\"deleted_blobs\": 1"), "{dry_stdout}");
    assert!(
        dry_stdout.contains("\"deleted_staging_runs\": 1"),
        "{dry_stdout}"
    );
    assert!(workspace.join(".ads-staging").join("run-dead").exists());

    let swept = cache_gc_run(&store, &workspace, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(!workspace.join(".ads-staging").join("run-dead").exists());

    // The kept view (current=latest=v002) still resolves, and the pruned
    // pinned view simply re-materializes from the store on demand.
    let still_current = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(still_current.status.success(), "{}", stderr(&still_current));
    assert_view_resolution(&workspace, &still_current, "model.usd", "v2");
    let repinned = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd?v=1",
    );
    assert!(repinned.status.success(), "{}", stderr(&repinned));
    assert_view_resolution(&workspace, &repinned, "model.usd", "v1");
}

fn cache_gc_run(store: &Path, workspace: &Path, dry_run: bool) -> Output {
    let mut command = ads();
    command
        .arg("cache")
        .arg("gc")
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(workspace)
        .arg("--staging-hours")
        .arg("0");
    if dry_run {
        command.arg("--dry-run");
    }
    command.output().unwrap()
}

#[test]
fn interrupted_view_materialization_leftover_is_ignored_and_swept() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    // A crashed materialization leaves its half-built sibling temp folder.
    let manifests = workspace.join(".ads-cache").join("manifests");
    let leftover = manifests.join("deadbeef.tmp.4242.7");
    fs::create_dir_all(&leftover).unwrap();
    fs::write(leftover.join("model.usd"), "partial").unwrap();

    let resolved = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_view_resolution(&workspace, &resolved, "model.usd", "v1");

    // Grace 0: the leftover counts as stale; the completed view survives.
    let swept = cache_gc_run(&store, &workspace, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(!leftover.exists());
    let again = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert_view_resolution(&workspace, &again, "model.usd", "v1");
}

#[test]
fn cache_gc_skips_in_flight_view_build_holding_its_lock() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    // A build in progress in another process: temp folder plus a held
    // exclusive lock on the sibling `.lock` file (the builder takes the lock
    // before creating the folder).
    let manifests = workspace.join(".ads-cache").join("manifests");
    let in_flight = manifests.join("cafef00d.tmp.31337.0");
    let lock_path = manifests.join("cafef00d.tmp.31337.0.lock");
    fs::create_dir_all(&in_flight).unwrap();
    fs::write(in_flight.join("model.usd"), "partial").unwrap();
    let lock_file = fs::File::create(&lock_path).unwrap();
    lock_file.try_lock().unwrap();

    // Grace 0 (see cache_gc_run) would sweep by age alone; the held lock
    // must win.
    let swept = cache_gc_run(&store, &workspace, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(in_flight.exists());
    assert!(lock_path.exists());

    // Builder gone (lock released): the leftover and its lock are swept.
    drop(lock_file);
    let swept = cache_gc_run(&store, &workspace, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(!in_flight.exists());
    assert!(!lock_path.exists());
}

#[test]
fn cache_gc_removes_orphan_view_build_locks() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    // A crashed builder can leave the lock file with no temp folder at all
    // (the lock is created first); a released lock with no folder is trash.
    let manifests = workspace.join(".ads-cache").join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    let orphan_lock = manifests.join("deadbeef.tmp.4242.7.lock");
    fs::write(&orphan_lock, b"").unwrap();

    let swept = cache_gc_run(&store, &workspace, false);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(!orphan_lock.exists());
}

#[test]
fn view_missing_its_marker_is_rematerialized() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    let resolved = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_view_resolution(&workspace, &resolved, "model.usd", "v1");
    let view_root = PathBuf::from(stdout(&resolved).trim())
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(view_root.join(".ads-complete").is_file());

    // Simulate the pre-atomic failure mode: view files on disk but no
    // completeness marker anywhere. Delete a file (not rewrite it — view
    // files are hardlinks into the blob cache) to prove a rebuild happens.
    fs::remove_file(view_root.join(".ads-complete")).unwrap();
    let hash = view_root.file_name().unwrap().to_string_lossy().to_string();
    let _ = fs::remove_file(view_root.parent().unwrap().join(format!("{hash}.complete")));
    fs::remove_file(view_root.join("model.usd")).unwrap();

    let again = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert_view_resolution(&workspace, &again, "model.usd", "v1");
    assert!(view_root.join(".ads-complete").is_file());
}

#[test]
fn read_commands_work_while_a_writer_holds_the_store() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
    fs::write(
        version_folder(&workspace, "prop", "crate", "v001").join("model.usd"),
        "v1",
    )
    .unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    // Hold a writer handle the way `ads serve` does.
    let _writer = ads::Store::open(&store).unwrap();

    // Read commands open the store read-only and keep working.
    let list = ads()
        .arg("list")
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", stderr(&list));
    let resolved = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
    assert_view_resolution(&workspace, &resolved, "model.usd", "v1");

    // Writes still need exclusive access and fail on the RocksDB lock.
    let staging = temp.path().join("staging");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("x.usd"), "wip").unwrap();
    let blocked = wip_add(&store, &staging);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).to_lowercase().contains("lock"));
}

#[test]
fn leaf_files_resolve_lazily_and_fall_back_to_remote() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(
        ads()
            .arg("init")
            .arg(&store)
            .arg("--remote-base-url")
            .arg("https://assets.example.com/objects/sha256")
            .status()
            .unwrap()
            .success()
    );

    new_version(&store, &workspace, "prop", "crate");
    let v001 = version_folder(&workspace, "prop", "crate", "v001");
    fs::create_dir(v001.join("sim")).unwrap();
    fs::write(v001.join("model.usd"), "usd").unwrap();
    fs::write(v001.join("sim").join("smoke.vdb"), "vdb data").unwrap();
    add_asset(&store, &workspace, "prop", "crate", "v001");

    // Any non-composing format is a leaf: a direct URI resolves lazily to the
    // flat blob cache without materializing the whole manifest view.
    let leaf = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://prop/crate/model/sim/smoke.vdb",
    );
    assert!(leaf.status.success(), "{}", stderr(&leaf));
    let resolved = stdout(&leaf).trim().replace('\\', "/");
    assert!(resolved.contains(".ads-cache/sha256/"), "{resolved}");
    assert_eq!(fs::read_to_string(resolved.as_str()).unwrap(), "vdb data");
    assert!(
        !workspace.join(".ads-cache").join("manifests").exists(),
        "leaf resolution must not materialize the manifest view"
    );

    // Auto mode falls back to the remote object URL for leaves too once the
    // local object is gone.
    let vdb_hash = sha256_hex(b"vdb data");
    fs::remove_file(
        store
            .join("objects")
            .join("sha256")
            .join(&vdb_hash[0..2])
            .join(&vdb_hash),
    )
    .unwrap();
    let fallback = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://prop/crate/model/sim/smoke.vdb",
    );
    assert!(fallback.status.success(), "{}", stderr(&fallback));
    assert_eq!(
        stdout(&fallback).trim(),
        format!(
            "https://assets.example.com/objects/sha256/{}/{}",
            &vdb_hash[0..2],
            vdb_hash
        )
    );
}

#[test]
fn resolve_materializes_manifest_view_with_sibling_files_and_integer_pin() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "char", "hero");
    let v001 = version_folder(&workspace, "char", "hero", "v001");
    fs::create_dir(v001.join("geo")).unwrap();
    fs::write(v001.join("hero.usd"), "root layer").unwrap();
    fs::write(v001.join("geo").join("body.usd"), "sibling layer").unwrap();
    add_asset(&store, &workspace, "char", "hero", "v001");
    fs::remove_dir_all(&v001).unwrap();

    // Integer version pins are the canonical v8 form.
    let pinned = resolve_asset(
        &store,
        &workspace,
        "local",
        None,
        "ads://char/hero/model/hero.usd?v=1",
    );
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    assert_view_resolution(&workspace, &pinned, "hero.usd", "root layer");

    // The whole manifest is materialized, so sibling-relative references
    // inside USD layers keep working from the view.
    let resolved_root = PathBuf::from(stdout(&pinned).trim());
    let sibling = resolved_root.parent().unwrap().join("geo").join("body.usd");
    assert_eq!(fs::read_to_string(&sibling).unwrap(), "sibling layer");
}

#[test]
fn thumbnails_are_version_metadata_and_use_object_store() {
    let temp = TempDir::new().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let out = temp.path().join("thumb-out.png");
    let thumb = temp.path().join("thumb.png");
    assert!(ads().arg("init").arg(&store).status().unwrap().success());

    new_version(&store, &workspace, "prop", "crate");
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
    assert!(info_stdout.contains("\"version\": 1"));
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
    new_version(&store, &workspace, "prop", "crate");
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

    new_version(&store, &workspace, "prop", "crate");
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
fn set_remote_config_is_used_by_auto_resolve_when_local_object_is_missing() {
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

    new_version(&store, &workspace, "char", "hero");
    fs::write(
        version_folder(&workspace, "char", "hero", "v001").join("model.usd"),
        "hero",
    )
    .unwrap();
    add_asset(&store, &workspace, "char", "hero", "v001");
    fs::remove_dir_all(version_folder(&workspace, "char", "hero", "v001")).unwrap();
    // v8: auto resolve materializes the manifest view from store objects, so
    // the remote fallback only triggers once the object itself is missing.
    let hero_hash = sha256_hex(b"hero");
    fs::remove_file(
        store
            .join("objects")
            .join("sha256")
            .join(&hero_hash[0..2])
            .join(&hero_hash),
    )
    .unwrap();

    let resolved = resolve_asset(
        &store,
        &workspace,
        "auto",
        None,
        "ads://char/hero/model/v001/model.usd",
    );
    assert!(resolved.status.success(), "{}", stderr(&resolved));
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
    new_version(&store, &workspace, "prop", "crate");
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
    new_version(&store, &workspace, "prop", "crate");
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

fn new_version(store: &Path, workspace: &Path, category: &str, asset_code: &str) -> PathBuf {
    new_version_department(store, workspace, category, asset_code, "model")
}

/// Creates the next conventional v### working folder directly: schema v8
/// removed the new-version command, so workspaces are plain scratch space.
fn new_version_department(
    _store: &Path,
    workspace: &Path,
    category: &str,
    asset_code: &str,
    department: &str,
) -> PathBuf {
    let mut department_dir = workspace.to_path_buf();
    for part in category.split('/') {
        department_dir.push(part);
    }
    department_dir.push(asset_code);
    department_dir.push(department);
    let next = (1..)
        .find(|n| !department_dir.join(format!("v{n:03}")).exists())
        .unwrap();
    let path = department_dir.join(format!("v{next:03}"));
    fs::create_dir_all(&path).unwrap();
    path
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

fn wip_add(store: &Path, source: &Path) -> Output {
    ads()
        .arg("wip")
        .arg("add")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .arg("--source")
        .arg(source)
        .output()
        .unwrap()
}

fn wip_list(store: &Path) -> Output {
    ads()
        .arg("wip")
        .arg("list")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .output()
        .unwrap()
}

fn publish_promote(store: &Path, no_validate: bool) -> Output {
    let mut command = ads();
    command
        .arg("publish")
        .arg("promote")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model");
    if no_validate {
        command.arg("--no-validate");
    }
    command.output().unwrap()
}

fn gc_run(store: &Path, retention: usize, grace_hours: u64, dry_run: bool) -> Output {
    let mut command = ads();
    command
        .arg("gc")
        .arg("--store")
        .arg(store)
        .arg("--retention")
        .arg(retention.to_string())
        .arg("--grace-hours")
        .arg(grace_hours.to_string());
    if dry_run {
        command.arg("--dry-run");
    }
    command.output().unwrap()
}

fn checkout_version(store: &Path, version: &str, dest: &Path) -> Output {
    ads()
        .arg("checkout")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg("prop")
        .arg("--asset-code")
        .arg("crate")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg(version)
        .arg(dest)
        .output()
        .unwrap()
}

fn publish_validate_version(store: &Path, version: &str) -> Output {
    ads()
        .arg("publish")
        .arg("validate")
        .arg("--store")
        .arg(store)
        .arg("--category")
        .arg("char")
        .arg("--asset-code")
        .arg("hero")
        .arg("--department")
        .arg("model")
        .arg("--version")
        .arg(version)
        .output()
        .unwrap()
}

fn publish_validate_source(source: &Path) -> Output {
    ads()
        .arg("publish")
        .arg("validate")
        .arg("--source")
        .arg(source)
        .output()
        .unwrap()
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

/// Asserts that a resolve output points into the immutable manifest view
/// cache and carries the expected file content (schema v8: local/auto USD
/// resolution returns view paths instead of workspace version folders).
fn assert_view_resolution(workspace: &Path, output: &Output, relative_path: &str, content: &str) {
    let resolved = stdout(output).trim().to_string();
    let view_prefix = workspace.join(".ads-cache").join("manifests");
    assert!(
        Path::new(&resolved).starts_with(&view_prefix),
        "resolved location {resolved} is not under {}",
        view_prefix.display()
    );
    assert!(
        resolved.replace('\\', "/").ends_with(relative_path),
        "resolved location {resolved} does not end with {relative_path}"
    );
    assert_eq!(
        fs::read_to_string(&resolved).unwrap(),
        content,
        "unexpected content at {resolved}"
    );
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
        .arg("model");
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
