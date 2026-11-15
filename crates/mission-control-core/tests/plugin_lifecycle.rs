//! Task 12.4: installing a plugin from its zip, activating and deactivating it, loading the
//! active plugins at start, and recovering from a plugin that fails (Review Focus item 3). The
//! zips are built in the test with the `zip` crate's writer around the sample plugins of
//! `tests/plugins/`, compiled when the test runs (Ruling R99). The options are written to the
//! database in `MC_TEST_DATABASE_URL`, or to a fresh SQLite file when it is unset.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mission_control_core::content::{
    NewPost, Post, PostStatus, delete_post_meta, get_option, get_post_meta, set_option,
};
use mission_control_core::hooks::{Action, Filter, Hooks};
use mission_control_core::migrations::migrator;
use mission_control_core::plugins::{
    ACTIVE_PLUGINS_OPTION, HostError, InstallError, InstallLimits, LifecycleError, MANIFEST_FILE,
    MAX_ACTIVE_PLUGINS, MAX_NOTICE_CHARS, MAX_NOTICES, MODULE_FILE, ManifestError, NOTICES_OPTION,
    PluginLimits, Plugins,
};
use mission_control_core::rest::posts::PostTypes;
use mission_control_core::rest::terms::Taxonomies;
use mission_control_core::types::{Origin, Registry};
use mission_control_core::users::{ADMINISTRATOR, UserCaps};
use mission_control_testing::{TestDatabase, create_users_table, database, unique_email};
use orbit_orm::raw;
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// The filter `title_filter.wat` subscribes to.
const TITLE: Filter<String> = Filter::new("mc_test_title");
/// The action `typer.wat` subscribes to; its handler traps.
const TYPER: Action<i64> = Action::new("mc_test_typer");

/// The sample plugin `name` in `tests/plugins/`, compiled to a binary component.
fn sample(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/plugins/{name}", env!("CARGO_MANIFEST_DIR"));
    wat::parse_file(&path).unwrap_or_else(|e| panic!("compiles {path}: {e}"))
}

/// The manifest of plugin `slug` with `permissions`, a TOML list's items.
fn manifest(slug: &str, permissions: &str) -> String {
    format!(
        "slug = \"{slug}\"\nname = \"Sample\"\nversion = \"1.0.0\"\n\
         license = \"GPL-2.0-or-later\"\nrequires = \"0.1\"\npermissions = [{permissions}]\n"
    )
}

/// A zip of `entries` (name, bytes), deflated.
fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in entries {
        if name.ends_with('/') {
            writer.add_directory(*name, options).unwrap();
        } else {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
}

/// The zip of plugin `slug`, in one folder named by its slug, as WordPress plugin zips are.
fn plugin_zip(slug: &str, permissions: &str, wasm: &[u8]) -> Vec<u8> {
    let toml = manifest(slug, permissions);
    zip(&[
        (&format!("{slug}/"), b""),
        (&format!("{slug}/{MANIFEST_FILE}"), toml.as_bytes()),
        (&format!("{slug}/{MODULE_FILE}"), wasm),
        (&format!("{slug}/assets/readme.txt"), b"Read me."),
    ])
}

fn admin() -> UserCaps {
    UserCaps::with_default_roles(1, &[ADMINISTRATOR], &BTreeMap::new())
}

/// A site: a database with the core tables, its hooks and type registry, and a plugins folder.
struct Site {
    test: TestDatabase,
    hooks: Arc<Hooks>,
    registry: Arc<Registry>,
    dir: tempfile::TempDir,
}

impl Site {
    async fn new() -> Site {
        let test = database().await;
        migrator()
            .unwrap()
            .run(test.db())
            .await
            .expect("runs the migrations");
        Site {
            test,
            hooks: Arc::new(Hooks::new()),
            registry: Arc::new(Registry::new(
                Arc::new(PostTypes::new()),
                Arc::new(Taxonomies::new()),
            )),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// The plugins folder, inside the temporary folder so a escape would land beside it.
    fn plugins_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("plugins")
    }

    fn plugins(&self) -> Plugins {
        Plugins::new(
            Arc::clone(&self.hooks),
            self.test.shared(),
            Arc::clone(&self.registry),
            self.plugins_dir(),
            PluginLimits::default(),
        )
        .expect("starts the plugin host")
    }

    /// What the `active_plugins` option holds.
    async fn active(&self) -> Value {
        get_option(self.test.db(), ACTIVE_PLUGINS_OPTION)
            .await
            .unwrap()
            .unwrap_or(json!([]))
    }

    /// The owner's notices, as stored.
    async fn notices(&self) -> Vec<Value> {
        match get_option(self.test.db(), NOTICES_OPTION).await.unwrap() {
            Some(Value::Array(notices)) => notices,
            None => Vec::new(),
            Some(other) => panic!("the notices are not a list: {other}"),
        }
    }

    fn title(&self) -> String {
        self.hooks.apply_filters(TITLE, "Hello".to_owned())
    }
}

/// Every file and folder under `dir`, relative, sorted.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(root).unwrap().display().to_string());
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_zip_is_installed_activated_and_deactivated() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let installed = plugins
        .install(&plugin_zip(
            "title-filter",
            "\"hooks\"",
            &sample("title_filter.wat"),
        ))
        .expect("installs the plugin");
    assert_eq!(installed.slug, "title-filter");
    assert_eq!(
        tree(&site.plugins_dir()),
        [
            "title-filter",
            "title-filter/assets",
            "title-filter/assets/readme.txt",
            "title-filter/plugin.toml",
            "title-filter/plugin.wasm",
        ]
    );
    // Installed is not active, as in WordPress.
    assert_eq!(site.title(), "Hello");
    assert!(!plugins.is_active("title-filter"));

    plugins
        .activate("title-filter", admin())
        .await
        .expect("activates the plugin");
    assert_eq!(site.title(), "Filtered");
    assert!(plugins.is_active("title-filter"));
    assert_eq!(site.active().await, json!(["title-filter"]));
    // Activating it again changes nothing, as `activate_plugin` on an active plugin.
    plugins.activate("title-filter", admin()).await.unwrap();
    assert_eq!(site.active().await, json!(["title-filter"]));

    assert!(plugins.deactivate("title-filter", admin()).await.unwrap());
    assert_eq!(site.title(), "Hello");
    assert!(!plugins.is_active("title-filter"));
    assert_eq!(site.active().await, json!([]));
    assert!(!plugins.deactivate("title-filter", admin()).await.unwrap());

    // It may be activated again: a new load.
    plugins.activate("title-filter", admin()).await.unwrap();
    assert_eq!(site.title(), "Filtered");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zip_with_the_files_at_its_root_is_installed_in_a_folder_named_by_the_slug() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let toml = manifest("root-plugin", "\"hooks\"");
    let wasm = sample("title_filter.wat");
    // Finder's `__MACOSX` folder is skipped, as WordPress's unzip skips it.
    let bytes = zip(&[
        (MANIFEST_FILE, toml.as_bytes()),
        (MODULE_FILE, &wasm),
        ("__MACOSX/._plugin.toml", b"junk"),
    ]);
    plugins.install(&bytes).expect("installs the plugin");
    assert_eq!(
        tree(&site.plugins_dir()),
        [
            "root-plugin",
            "root-plugin/plugin.toml",
            "root-plugin/plugin.wasm"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_or_truncated_zip_is_refused_and_the_site_keeps_serving() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "title-filter",
            "\"hooks\"",
            &sample("title_filter.wat"),
        ))
        .unwrap();
    plugins.activate("title-filter", admin()).await.unwrap();
    let before = tree(&site.plugins_dir());

    let good = plugin_zip("other", "\"hooks\"", &sample("title_filter.wat"));
    let truncated = good[..good.len() / 2].to_vec();
    let no_end = good[..good.len() - 10].to_vec();
    // A byte of the deflated module changed: the entry fails its CRC or does not inflate.
    let mut flipped = good.clone();
    let module_at = find(&flipped, b"other/plugin.wasm").expect("the module's local header") + 64;
    flipped[module_at] ^= 0xFF;
    for (case, bytes) in [
        ("empty", Vec::new()),
        ("not a zip", b"PK\x03\x04 this is not a zip at all".to_vec()),
        ("truncated", truncated),
        ("without its end", no_end),
        ("corrupt data", flipped),
    ] {
        let error = plugins.install(&bytes).expect_err(case);
        assert!(
            matches!(error, InstallError::Corrupt(_)),
            "{case}: {error:?}"
        );
        assert!(!error.to_string().is_empty());
        assert_eq!(tree(&site.plugins_dir()), before, "{case}");
    }
    // The site keeps serving: the active plugin still runs.
    assert_eq!(site.title(), "Filtered");
    assert_eq!(site.active().await, json!(["title-filter"]));
}

/// Where `needle` first appears in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_that_leave_the_plugin_folder_are_refused() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let toml = manifest("evil", "\"hooks\"");
    let wasm = sample("title_filter.wat");
    for bad in [
        "../escaped.txt",
        "evil/../../escaped.txt",
        "/etc/escaped.txt",
        "C:/escaped.txt",
        "evil\\..\\escaped.txt",
        "evil//double.txt",
        "evil/./dot.txt",
        "evil/new\nline.txt",
        "evil/a\u{202E}txt.exe",
    ] {
        let bytes = zip(&[
            ("evil/plugin.toml", toml.as_bytes()),
            ("evil/plugin.wasm", &wasm),
            (bad, b"x"),
        ]);
        let error = plugins.install(&bytes).expect_err(bad);
        assert!(
            matches!(error, InstallError::UnsafePath { .. }),
            "{bad:?}: {error:?}"
        );
    }
    // Two names that are one file on a case-insensitive disk.
    let bytes = zip(&[
        ("evil/plugin.toml", toml.as_bytes()),
        ("evil/plugin.wasm", &wasm),
        ("evil/Readme.txt", b"x"),
        ("evil/README.txt", b"y"),
    ]);
    assert!(matches!(
        plugins.install(&bytes),
        Err(InstallError::DuplicatePath { .. })
    ));

    // A symbolic link.
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    writer.start_file("evil/plugin.toml", options).unwrap();
    writer.write_all(toml.as_bytes()).unwrap();
    writer.start_file("evil/plugin.wasm", options).unwrap();
    writer.write_all(&wasm).unwrap();
    writer
        .add_symlink("evil/link", "/etc/passwd", options)
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let error = plugins.install(&bytes).unwrap_err();
    assert!(
        matches!(error, InstallError::SpecialFile { ref path } if path == "evil/link"),
        "{error:?}"
    );

    assert_eq!(tree(site.dir.path()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn zip_bombs_and_oversized_zips_are_refused() {
    let site = Site::new().await;
    let toml = manifest("bomb", "\"hooks\"");
    let wasm = sample("title_filter.wat");
    let limits = InstallLimits {
        zip_max_bytes: 512 * 1024,
        entries_max: 8,
        unpacked_max_bytes: 256 * 1024,
        compression_ratio_max: 50,
    };
    let plugins = site.plugins().with_install_limits(limits);

    // 4 MiB of zeros deflate to about 4 KiB: a ratio of about 1000. Refused for its ratio,
    // under a total limit it would fit.
    let zeros = vec![0u8; 4 * 1024 * 1024];
    let bytes = zip(&[
        ("bomb/plugin.toml", toml.as_bytes()),
        ("bomb/plugin.wasm", &wasm),
        ("bomb/zeros.bin", &zeros),
    ]);
    let roomy = site.plugins().with_install_limits(InstallLimits {
        unpacked_max_bytes: 64 * 1024 * 1024,
        ..limits
    });
    let error = roomy.install(&bytes).unwrap_err();
    assert!(
        matches!(error, InstallError::CompressionRatio { ref path } if path == "bomb/zeros.bin"),
        "{error:?}"
    );

    // Too much once unpacked, though each file compresses little.
    let noise: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let bytes = zip(&[
        ("bomb/plugin.toml", toml.as_bytes()),
        ("bomb/plugin.wasm", &wasm),
        ("bomb/a.bin", &noise),
        ("bomb/b.bin", &noise),
    ]);
    assert!(matches!(
        plugins.install(&bytes),
        Err(InstallError::UnpackedTooLarge { max }) if max == 256 * 1024
    ));

    // Too many entries.
    let names: Vec<String> = (0..9).map(|i| format!("bomb/f{i}.txt")).collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        ("bomb/plugin.toml", toml.as_bytes()),
        ("bomb/plugin.wasm", &wasm),
    ];
    entries.extend(names.iter().map(|n| (n.as_str(), &b"x"[..])));
    assert!(matches!(
        plugins.install(&zip(&entries)),
        Err(InstallError::TooManyEntries { count: 11, max: 8 })
    ));

    // A zip over the size limit is not read at all.
    let big = vec![7u8; 512 * 1024 + 1];
    assert!(matches!(
        plugins.install(&big),
        Err(InstallError::TooLarge { .. })
    ));

    assert_eq!(tree(site.dir.path()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zip_without_a_valid_plugin_is_refused_before_anything_is_written() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let wasm = sample("title_filter.wat");
    let good = manifest("good", "\"hooks\"");

    let error = plugins
        .install(&zip(&[("good/plugin.wasm", &wasm)]))
        .unwrap_err();
    assert!(matches!(error, InstallError::Layout(_)), "{error:?}");
    let error = plugins
        .install(&zip(&[("good/plugin.toml", good.as_bytes())]))
        .unwrap_err();
    assert!(matches!(error, InstallError::Layout(_)), "{error:?}");
    // Two folders at the top.
    let error = plugins
        .install(&zip(&[
            ("good/plugin.toml", good.as_bytes()),
            ("good/plugin.wasm", &wasm),
            ("other/x.txt", b"x"),
        ]))
        .unwrap_err();
    assert!(matches!(error, InstallError::Layout(_)), "{error:?}");
    // The folder is not the slug.
    let error = plugins
        .install(&zip(&[
            ("renamed/plugin.toml", good.as_bytes()),
            ("renamed/plugin.wasm", &wasm),
        ]))
        .unwrap_err();
    assert!(
        matches!(&error, InstallError::Layout(m) if m.contains("renamed") && m.contains("good")),
        "{error:?}"
    );
    let error = plugins
        .install(&zip(&[
            ("good/plugin.toml", b"slug = \"good\"\n"),
            ("good/plugin.wasm", &wasm),
        ]))
        .unwrap_err();
    assert!(
        matches!(error, InstallError::Manifest(ManifestError::Missing { .. })),
        "{error:?}"
    );
    let error = plugins
        .install(&zip(&[
            ("good/plugin.toml", good.as_bytes()),
            ("good/plugin.wasm", b"\0asm\x01\0\0\0"),
        ]))
        .unwrap_err();
    assert!(
        matches!(error, InstallError::Module(HostError::Compile(_))),
        "{error:?}"
    );
    // The module imports what the manifest does not grant.
    let error = plugins
        .install(&zip(&[
            ("good/plugin.toml", manifest("good", "").as_bytes()),
            ("good/plugin.wasm", &wasm),
        ]))
        .unwrap_err();
    assert!(
        matches!(
            error,
            InstallError::Module(HostError::MissingPermission { .. })
        ),
        "{error:?}"
    );
    assert_eq!(tree(site.dir.path()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installed_slug_is_not_replaced() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let wasm = sample("title_filter.wat");
    plugins
        .install(&plugin_zip("same", "\"hooks\"", &wasm))
        .unwrap();
    let readme = site.plugins_dir().join("same/assets/readme.txt");
    std::fs::write(&readme, "kept").unwrap();
    let error = plugins
        .install(&plugin_zip("same", "\"hooks\"", &wasm))
        .unwrap_err();
    assert!(
        matches!(error, InstallError::AlreadyInstalled { ref slug } if slug == "same"),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "kept");
    // No temporary folder is left behind.
    assert_eq!(
        std::fs::read_dir(site.plugins_dir()).unwrap().count(),
        1,
        "{:?}",
        tree(&site.plugins_dir())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_needs_a_newer_api_is_refused_at_install_and_activation() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let toml = manifest("future", "\"hooks\"").replace("requires = \"0.1\"", "requires = \"9.0\"");
    // Ruling R106: refused at upload, as WordPress 5.5's `check_package` refuses it.
    let error = plugins
        .install(&zip(&[
            ("future/plugin.toml", toml.as_bytes()),
            ("future/plugin.wasm", &sample("title_filter.wat")),
        ]))
        .unwrap_err();
    assert!(
        matches!(
            error,
            InstallError::Manifest(ManifestError::Incompatible { .. })
        ),
        "{error:?}"
    );
    assert_eq!(tree(site.dir.path()), Vec::<String>::new());
    // Checked again at activation: a folder put in place by hand.
    plugins
        .install(&plugin_zip(
            "future",
            "\"hooks\"",
            &sample("title_filter.wat"),
        ))
        .unwrap();
    std::fs::write(site.plugins_dir().join("future/plugin.toml"), &toml).unwrap();
    let error = plugins.activate("future", admin()).await.unwrap_err();
    assert!(
        matches!(
            error,
            LifecycleError::Manifest(ManifestError::Incompatible { .. })
        ),
        "{error:?}"
    );
    assert_eq!(site.active().await, json!([]));
    assert!(matches!(
        plugins.activate("missing", admin()).await,
        Err(LifecycleError::NotInstalled { .. })
    ));
    assert!(matches!(
        plugins.activate("../etc", admin()).await,
        Err(LifecycleError::NotInstalled { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn deactivation_removes_the_types_the_plugin_registered() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "typer",
            "\"hooks\", \"register-types\"",
            &sample("typer.wat"),
        ))
        .unwrap();
    plugins.activate("typer", admin()).await.unwrap();
    let book = site.registry.post_type("lcbook").expect("registered");
    assert_eq!(book.origin, Origin::Plugin("typer".into()));
    assert!(site.registry.post_types().registered("lcbook").is_some());

    plugins.deactivate("typer", admin()).await.unwrap();
    assert!(site.registry.post_type("lcbook").is_none());
    assert!(site.registry.post_types().registered("lcbook").is_none());
    assert_eq!(site.active().await, json!([]));

    // Activated again, it registers them again.
    plugins.activate("typer", admin()).await.unwrap();
    assert!(site.registry.post_type("lcbook").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn types_need_the_register_types_permission() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let error = plugins
        .install(&plugin_zip("typer", "\"hooks\"", &sample("typer.wat")))
        .unwrap_err();
    assert!(
        matches!(&error, InstallError::Module(HostError::MissingPermission { import, .. }) if import.contains("/types@")),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_traps_on_activation_is_left_inactive_and_the_owner_is_told() {
    let site = Site::new().await;
    let plugins = site.plugins();
    for (slug, permissions, wasm) in [
        ("title-filter", "\"hooks\"", "title_filter.wat"),
        ("crasher", "\"register-types\"", "type_crasher.wat"),
    ] {
        plugins
            .install(&plugin_zip(slug, permissions, &sample(wasm)))
            .unwrap();
    }
    plugins.activate("title-filter", admin()).await.unwrap();

    let error = plugins.activate("crasher", admin()).await.unwrap_err();
    assert!(
        matches!(error, LifecycleError::Host(HostError::Trap(_))),
        "{error:?}"
    );
    assert!(!plugins.is_active("crasher"));
    // The type it registered before it trapped is gone.
    assert!(site.registry.post_type("lcbook").is_none());
    assert_eq!(site.active().await, json!(["title-filter"]));
    let notices = site.notices().await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["slug"], "crasher");
    let message = notices[0]["message"].as_str().unwrap();
    assert!(message.contains("unreachable"), "{message}");
    assert!(notices[0]["time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(plugins.notices().await.unwrap()[0].slug, "crasher");

    // The site still serves.
    assert_eq!(site.title(), "Filtered");

    assert!(plugins.dismiss_notice("crasher").await.unwrap());
    assert!(site.notices().await.is_empty());
    assert!(!plugins.dismiss_notice("crasher").await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_traps_at_run_time_is_deactivated_and_the_owner_is_told() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "typer",
            "\"hooks\", \"register-types\"",
            &sample("typer.wat"),
        ))
        .unwrap();
    plugins
        .install(&plugin_zip(
            "title-filter",
            "\"hooks\"",
            &sample("title_filter.wat"),
        ))
        .unwrap();
    plugins.activate("typer", admin()).await.unwrap();
    plugins.activate("title-filter", admin()).await.unwrap();
    assert_eq!(site.active().await, json!(["title-filter", "typer"]));

    // The action reaches the plugin, which traps; the action returns.
    site.hooks.do_action(TYPER, &1);
    assert!(!plugins.is_active("typer"));
    // Its hooks and types are gone at once; the option is written by the recovery.
    assert!(site.registry.post_type("lcbook").is_none());
    assert_eq!(plugins.recover().await.unwrap(), 1);
    assert_eq!(site.active().await, json!(["title-filter"]));
    let notices = site.notices().await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["slug"], "typer");
    assert_eq!(plugins.recover().await.unwrap(), 0);

    // The site keeps serving: the action fires again without it, the other plugin runs.
    site.hooks.do_action(TYPER, &1);
    assert_eq!(site.title(), "Filtered");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_task_writes_a_run_time_failure_by_itself() {
    let site = Site::new().await;
    let plugins = Arc::new(site.plugins());
    let task = plugins.spawn_recovery();
    plugins
        .install(&plugin_zip(
            "typer",
            "\"hooks\", \"register-types\"",
            &sample("typer.wat"),
        ))
        .unwrap();
    plugins.activate("typer", admin()).await.unwrap();

    // Fired from a blocking thread, as the scheduler fires hooks.
    let hooks = Arc::clone(&site.hooks);
    tokio::task::spawn_blocking(move || hooks.do_action(TYPER, &1))
        .await
        .unwrap();
    let mut waited = Duration::ZERO;
    while site.active().await != json!([]) {
        assert!(waited < Duration::from_secs(10), "the recovery never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(site.notices().await.len(), 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_activates_the_active_plugins_and_deactivates_those_that_fail() {
    let site = Site::new().await;
    {
        let plugins = site.plugins();
        for (slug, permissions, wasm) in [
            ("title-filter", "\"hooks\"", "title_filter.wat"),
            ("typer", "\"hooks\", \"register-types\"", "typer.wat"),
            ("crasher", "\"register-types\"", "type_crasher.wat"),
        ] {
            plugins
                .install(&plugin_zip(slug, permissions, &sample(wasm)))
                .unwrap();
        }
    }
    // A broken module, installed by hand.
    std::fs::write(site.plugins_dir().join("typer/plugin.wasm"), b"broken").unwrap();
    set_option(
        site.test.db(),
        ACTIVE_PLUGINS_OPTION,
        &json!(["crasher", "missing", "title-filter", "typer", "../etc", 7]),
    )
    .await
    .unwrap();

    // A new start: fresh hooks and registry.
    let site = Site {
        hooks: Arc::new(Hooks::new()),
        registry: Arc::new(Registry::default()),
        ..site
    };
    let plugins = site.plugins();
    let failures = plugins.boot().await.expect("reads the options");
    let mut failed: Vec<&str> = failures.iter().map(|(slug, _)| slug.as_str()).collect();
    failed.sort_unstable();
    assert_eq!(failed, ["crasher", "missing", "typer"]);
    assert!(matches!(
        failures.iter().find(|(s, _)| s == "missing").unwrap().1,
        LifecycleError::NotInstalled { .. }
    ));

    assert!(plugins.is_active("title-filter"));
    assert_eq!(site.title(), "Filtered");
    assert_eq!(site.active().await, json!(["title-filter"]));
    assert!(site.registry.post_type("lcbook").is_none());
    let mut told: Vec<String> = site
        .notices()
        .await
        .iter()
        .map(|n| n["slug"].as_str().unwrap().to_owned())
        .collect();
    told.sort_unstable();
    assert_eq!(told, ["crasher", "missing", "typer"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn notices_are_bounded() {
    let site = Site::new().await;
    let plugins = site.plugins();
    // A stored option that is not a list is read as none, and replaced.
    set_option(site.test.db(), NOTICES_OPTION, &json!({"x": 1}))
        .await
        .unwrap();
    assert!(plugins.notices().await.unwrap().is_empty());
    let long = format!("line\nbreak {}", "x".repeat(10_000));
    plugins.notify_owner("boom", &long).await.unwrap();
    let notices = plugins.notices().await.unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].message.chars().count() <= MAX_NOTICE_CHARS + 1);
    assert!(notices[0].message.starts_with("line\\nbreak"));
    // One notice per plugin: the latest.
    plugins.notify_owner("boom", "again").await.unwrap();
    assert_eq!(plugins.notices().await.unwrap()[0].message, "again");
}

/// Creates a user and post 1, the post `activator.wat` writes meta on.
async fn first_post(site: &Site) -> i64 {
    let db = site.test.db();
    create_users_table(db).await;
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts the user");
    let author: i64 = raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .unwrap();
    let post = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "First".into(),
            ..NewPost::new(author, "post")
        },
    )
    .await
    .expect("creates the post");
    assert_eq!(post.id, 1, "a fresh posts table starts at 1");
    post.id
}

#[tokio::test(flavor = "multi_thread")]
async fn activate_runs_once_as_the_owner_and_boot_runs_init_only() {
    let site = Site::new().await;
    let post = first_post(&site).await;
    {
        let plugins = site.plugins();
        plugins
            .install(&plugin_zip(
                "activator",
                "\"write-content\"",
                &sample("activator.wat"),
            ))
            .unwrap();
        plugins.activate("activator", admin()).await.unwrap();
        assert_eq!(
            get_post_meta(site.test.db(), post, "mc_activated")
                .await
                .unwrap(),
            Some(json!(true))
        );
    }
    delete_post_meta(site.test.db(), post, "mc_activated")
        .await
        .unwrap();

    // A new start: `init` only, so the meta is not written again (a visitor could not), and the
    // plugin stays active.
    let site = Site {
        hooks: Arc::new(Hooks::new()),
        registry: Arc::new(Registry::default()),
        ..site
    };
    let plugins = site.plugins();
    assert!(plugins.boot().await.unwrap().is_empty());
    assert!(plugins.is_active("activator"));
    assert_eq!(site.active().await, json!(["activator"]));
    assert_eq!(
        get_post_meta(site.test.db(), post, "mc_activated")
            .await
            .unwrap(),
        None
    );

    // Its `deactivate` runs as the owner: it deletes the meta it set.
    mission_control_core::content::set_post_meta(
        site.test.db(),
        post,
        "mc_activated",
        &json!(true),
    )
    .await
    .unwrap();
    assert!(plugins.deactivate("activator", admin()).await.unwrap());
    assert_eq!(
        get_post_meta(site.test.db(), post, "mc_activated")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_activate_the_user_may_not_do_leaves_the_plugin_inactive() {
    let site = Site::new().await;
    first_post(&site).await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "activator",
            "\"write-content\"",
            &sample("activator.wat"),
        ))
        .unwrap();
    let visitor = UserCaps::from_roles(0, &[], &BTreeMap::new());
    let error = plugins.activate("activator", visitor).await.unwrap_err();
    assert_eq!(
        error,
        LifecycleError::Host(HostError::Activation("set-post-meta failed".into()))
    );
    assert!(!plugins.is_active("activator"));
    assert_eq!(site.active().await, json!([]));
    // A refusal is not a fatal error: no notice.
    assert!(site.notices().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_deactivate_is_logged_and_the_plugin_is_deactivated_anyway() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "bad-deactivator",
            "\"hooks\"",
            &sample("bad_deactivator.wat"),
        ))
        .unwrap();
    plugins.activate("bad-deactivator", admin()).await.unwrap();
    assert_eq!(site.title(), "Filtered");
    assert!(
        plugins
            .deactivate("bad-deactivator", admin())
            .await
            .unwrap()
    );
    assert_eq!(site.title(), "Hello");
    assert!(!plugins.is_active("bad-deactivator"));
    assert_eq!(site.active().await, json!([]));
    // It trapped: the owner is told, as for any fatal error.
    assert_eq!(site.notices().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_of_an_older_load_leaves_the_newer_load_active() {
    let site = Site::new().await;
    let plugins = site.plugins();
    plugins
        .install(&plugin_zip(
            "typer",
            "\"hooks\", \"register-types\"",
            &sample("typer.wat"),
        ))
        .unwrap();
    plugins.activate("typer", admin()).await.unwrap();
    // The first load traps; before its failure is written, the owner activates it again.
    site.hooks.do_action(TYPER, &1);
    assert!(!plugins.is_active("typer"));
    plugins.activate("typer", admin()).await.unwrap();
    assert!(plugins.is_active("typer"));

    assert_eq!(plugins.recover().await.unwrap(), 1);
    // Only the notice is kept: the newer load, its types and the option stay.
    assert!(plugins.is_active("typer"));
    assert!(site.registry.post_type("lcbook").is_some());
    assert_eq!(site.active().await, json!(["typer"]));
    assert_eq!(site.notices().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_deactivates_a_plugin_whose_init_refuses() {
    let site = Site::new().await;
    site.plugins()
        .install(&plugin_zip(
            "half-activator",
            "\"hooks\"",
            &sample("half_activator.wat"),
        ))
        .unwrap();
    set_option(
        site.test.db(),
        ACTIVE_PLUGINS_OPTION,
        &json!(["half-activator"]),
    )
    .await
    .unwrap();
    let plugins = site.plugins();
    let failures = plugins.boot().await.unwrap();
    assert_eq!(
        failures,
        [(
            "half-activator".to_owned(),
            LifecycleError::Host(HostError::Init("not today".into()))
        )]
    );
    assert_eq!(site.active().await, json!([]));
    assert_eq!(site.title(), "Hello");
    let notices = site.notices().await;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]["message"]
            .as_str()
            .unwrap()
            .contains("not today")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_leaves_plugins_past_the_limit_inactive_with_a_notice() {
    let site = Site::new().await;
    let slugs: Vec<String> = (0..=MAX_ACTIVE_PLUGINS)
        .map(|i| format!("p{i:04}"))
        .collect();
    set_option(site.test.db(), ACTIVE_PLUGINS_OPTION, &json!(slugs))
        .await
        .unwrap();
    let plugins = site.plugins();
    let failures = plugins.boot().await.unwrap();
    assert_eq!(failures.len(), MAX_ACTIVE_PLUGINS + 1);
    let (last, error) = failures.last().unwrap();
    assert_eq!(last, &slugs[MAX_ACTIVE_PLUGINS]);
    assert!(matches!(error, LifecycleError::TooManyActive { .. }));
    assert_eq!(site.active().await, json!([]));
    let notices = site.notices().await;
    assert_eq!(notices.len(), MAX_NOTICES);
    assert_eq!(notices.last().unwrap()["slug"], slugs[MAX_ACTIVE_PLUGINS]);
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_removes_temporary_folders_and_a_missing_folder_is_no_plugins() {
    let site = Site::new().await;
    let plugins = site.plugins();
    // No plugins folder at all.
    assert!(plugins.boot().await.unwrap().is_empty());
    let temp = site.plugins_dir().join(".mc-install.1.2");
    std::fs::create_dir_all(temp.join("inner")).unwrap();
    plugins.boot().await.unwrap();
    assert!(!temp.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_task_ends_when_the_plugins_are_dropped() {
    let site = Site::new().await;
    let plugins = Arc::new(site.plugins());
    let task = plugins.spawn_recovery();
    drop(plugins);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the task ends")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn more_unsafe_names_are_refused() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let toml = manifest("evil", "\"hooks\"");
    let wasm = sample("title_filter.wat");
    for bad in [
        "C:\\escaped.txt",
        "evil\\x.txt",
        "evil/caf\u{e9}.txt",
        "evil/cafe\u{301}.txt",
    ] {
        let bytes = zip(&[
            ("evil/plugin.toml", toml.as_bytes()),
            ("evil/plugin.wasm", &wasm),
            (bad, b"x"),
        ]);
        let error = plugins.install(&bytes).expect_err(bad);
        assert!(
            matches!(error, InstallError::UnsafePath { .. }),
            "{bad:?}: {error:?}"
        );
    }
    assert!(matches!(
        plugins.notify_owner("../x", "m").await,
        Err(LifecycleError::NotInstalled { .. })
    ));
    assert!(matches!(
        plugins.dismiss_notice("").await,
        Err(LifecycleError::NotInstalled { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zip_written_with_data_descriptors_is_installed() {
    let site = Site::new().await;
    let plugins = site.plugins();
    let toml = manifest("streamed", "\"hooks\"");
    let wasm = sample("title_filter.wat");
    let mut writer = ZipWriter::new_stream(Vec::new());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in [
        ("streamed/plugin.toml", toml.as_bytes()),
        ("streamed/plugin.wasm", &wasm[..]),
        ("streamed/readme.txt", b"Read me."),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    let bytes = writer.finish().unwrap().get_ref().clone();
    // General purpose flag bit 3: sizes and checksum follow the data.
    assert_ne!(bytes[6] & 0x08, 0, "the writer used data descriptors");
    plugins.install(&bytes).expect("installs the plugin");
    plugins.activate("streamed", admin()).await.unwrap();
    assert_eq!(site.title(), "Filtered");
}
