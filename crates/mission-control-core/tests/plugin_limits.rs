//! Task 12.3: the limits of a plugin call, fuel (CPU) and memory (design section 4.2). The
//! sample plugins are components written in the component model's text format in
//! `tests/plugins/` and compiled when the test runs (Ruling R99). The host runs on the database
//! in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset; only the get-post
//! loop reads content.

use std::sync::{Arc, Mutex, Once};

use mission_control_core::hooks::{Action, Filter, Hooks};
use mission_control_core::migrations::migrator;
use mission_control_core::plugins::{
    HostError, MAX_NESTED_PLUGIN_CALLS, Manifest, Plugin, PluginHost, PluginLimits,
};
use mission_control_testing::{TestDatabase, create_users_table, database};

/// The action `looper.wat` subscribes to; its handler loops forever.
const SPIN: Action<i64> = Action::new("mc_test_spin");
/// The filter `looper.wat` subscribes to; its handler loops forever.
const SPUN: Filter<String> = Filter::new("mc_test_spun");
/// The action `grower.wat` subscribes to; its handler grows its memory until refused.
const GROW: Action<i64> = Action::new("mc_test_grow");
/// The filter `scanner.wat` subscribes to; it uppercases ASCII letters byte by byte.
const SCAN: Filter<String> = Filter::new("mc_test_scan");
/// The filter `realloc_spinner.wat` subscribes to; its allocator loops forever.
const SLOW: Filter<String> = Filter::new("mc_test_slow");
/// The action `recurser.wat` subscribes to; its handler recurses until the stack overflows.
const DEEP: Action<i64> = Action::new("mc_test_deep");
/// The filter `title_filter.wat` subscribes to; it gives "Filtered".
const TITLE: Filter<String> = Filter::new("mc_test_title");

/// A post's content of 256 KiB, the size the default fuel is chosen for.
const POST_BYTES: usize = 256 * 1024;

/// The log lines of this test binary, captured so a test can see what the host told the owner.
static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target() == "mission_control::plugins"
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            LINES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

/// Starts capturing the plugin host's log lines (once per test binary).
fn capture_logs() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&Capture).expect("installs the capturing logger");
        log::set_max_level(log::LevelFilter::Info);
    });
}

/// How many captured log lines are about plugin `slug` and contain `text`.
fn logged(slug: &str, text: &str) -> usize {
    let tag = format!("[{slug}]");
    LINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|line| line.contains(&tag) && line.contains(text))
        .count()
}

/// The sample plugin `name` in `tests/plugins/`, compiled to a binary component.
fn sample(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/plugins/{name}", env!("CARGO_MANIFEST_DIR"));
    wat::parse_file(&path).unwrap_or_else(|e| panic!("compiles {path}: {e}"))
}

/// A manifest for the plugin `slug` with `permissions`, a TOML list's items such as `"hooks"`.
fn manifest(slug: &str, permissions: &str) -> Manifest {
    Manifest::parse(&format!(
        "slug = \"{slug}\"\nname = \"Sample\"\nversion = \"1.0.0\"\n\
         license = \"GPL-2.0-or-later\"\nrequires = \"0.1\"\npermissions = [{permissions}]\n"
    ))
    .expect("a valid manifest")
}

/// A database, the site's hooks and a host on them with `limits`.
async fn setup(limits: PluginLimits) -> (TestDatabase, Arc<Hooks>, PluginHost) {
    let test = database().await;
    capture_logs();
    let hooks = Arc::new(Hooks::new());
    let host = PluginHost::with_limits(Arc::clone(&hooks), test.shared(), limits)
        .expect("starts the host");
    (test, hooks, host)
}

/// Loads the sample `file` as the plugin `slug` with the `hooks` permission and runs its `init`.
fn activated(host: &PluginHost, slug: &str, file: &str) -> Arc<Plugin> {
    let plugin = host
        .load(manifest(slug, "\"hooks\""), &sample(file))
        .expect("loads the plugin");
    plugin.init().expect("starts the plugin");
    plugin
}

/// A JSON string's text of `POST_BYTES` lowercase letters, as a post's content.
fn post_content() -> String {
    "abcdefghij".repeat(POST_BYTES / 10)
}

#[test]
fn the_default_limits_are_the_documented_ones() {
    let limits = PluginLimits::default();
    assert_eq!(limits.fuel_per_call, 1_000_000_000);
    assert_eq!(limits.memory_max_bytes, 64 * 1024 * 1024);
    assert_eq!(limits.table_max_elements, 100_000);
    assert_eq!(limits.instances_max, 64);
    assert_eq!(limits.memories_max, 8);
    assert_eq!(limits.tables_max, 64);
    assert_eq!(limits.fuel_per_host_call, 1_000_000);
    assert_eq!(limits.log_lines_per_call, 100);
    assert_eq!(limits.log_bytes_per_call, 64 * 1024);
    assert_eq!(MAX_NESTED_PLUGIN_CALLS, 4);
}

#[tokio::test]
async fn limits_that_leave_a_plugin_nothing_are_refused() {
    let test = database().await;
    let refused = |limits: PluginLimits| {
        PluginHost::with_limits(Arc::new(Hooks::new()), test.shared(), limits).unwrap_err()
    };
    let default = PluginLimits::default();
    for limits in [
        PluginLimits {
            fuel_per_call: 0,
            ..default
        },
        PluginLimits {
            fuel_per_host_call: 0,
            ..default
        },
        PluginLimits {
            fuel_per_host_call: default.fuel_per_call + 1,
            ..default
        },
        PluginLimits {
            memory_max_bytes: 65_535,
            ..default
        },
        PluginLimits {
            instances_max: 0,
            ..default
        },
        PluginLimits {
            memories_max: 0,
            ..default
        },
    ] {
        assert!(
            matches!(refused(limits), HostError::InvalidLimits(_)),
            "{limits:?}"
        );
    }
}

#[tokio::test]
async fn a_host_made_with_new_has_the_default_limits() {
    let test = database().await;
    let host = PluginHost::new(Arc::new(Hooks::new()), test.shared()).expect("starts the host");
    assert_eq!(host.limits(), &PluginLimits::default());
}

#[tokio::test]
async fn an_init_that_loops_forever_runs_out_of_fuel() {
    let limits = PluginLimits {
        fuel_per_call: 1_000_000,
        ..PluginLimits::default()
    };
    let (_test, _hooks, host) = setup(limits).await;
    let plugin = host
        .load(manifest("spinner", ""), &sample("spinner.wat"))
        .expect("loads the plugin");

    assert_eq!(plugin.init(), Err(HostError::OutOfFuel { fuel: 1_000_000 }));
    assert!(plugin.trapped());
    assert_eq!(
        plugin.failure(),
        Some(HostError::OutOfFuel { fuel: 1_000_000 })
    );
}

#[tokio::test]
async fn an_action_that_loops_forever_is_stopped_and_the_site_goes_on() {
    let (_test, hooks, host) = setup(PluginLimits::default()).await;
    let plugin = activated(&host, "looper-action", "looper.wat");

    // The default fuel stops it: the call returns, and so does the next one.
    hooks.do_action(SPIN, &1);
    hooks.do_action(SPIN, &2);

    assert!(plugin.trapped());
    assert_eq!(
        plugin.failure(),
        Some(HostError::OutOfFuel {
            fuel: PluginLimits::default().fuel_per_call
        })
    );
    // Its hooks are removed: the filter it subscribed no longer reaches it.
    assert_eq!(hooks.apply_filters(SPUN, "kept".to_owned()), "kept");
    // The owner is told once, in the log, why it stopped.
    assert_eq!(logged("looper-action", "fuel"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filter_that_loops_forever_on_a_runtime_worker_keeps_the_value() {
    let limits = PluginLimits {
        fuel_per_call: 1_000_000,
        // `init` makes host calls; keep them cheap so the call itself runs out.
        fuel_per_host_call: 1_000,
        ..PluginLimits::default()
    };
    let (_test, hooks, host) = setup(limits).await;
    let plugin = activated(&host, "looper-filter", "looper.wat");

    assert_eq!(hooks.apply_filters(SPUN, "kept".to_owned()), "kept");

    assert!(plugin.trapped());
    assert_eq!(
        plugin.failure(),
        Some(HostError::OutOfFuel { fuel: 1_000_000 })
    );
}

#[tokio::test]
async fn fuel_is_given_afresh_for_each_call() {
    // One call over 256 KiB uses about 6.03 million units (measured); with 9 million per call,
    // three calls run, where one budget shared by the calls would run out in the second.
    let limits = PluginLimits {
        fuel_per_call: 9_000_000,
        ..PluginLimits::default()
    };
    let (_test, hooks, host) = setup(limits).await;
    let plugin = activated(&host, "scanner-fresh", "scanner.wat");
    let content = post_content();

    for _ in 0..3 {
        assert_eq!(
            hooks.apply_filters(SCAN, content.clone()),
            content.to_uppercase()
        );
    }
    assert!(!plugin.trapped());
}

#[tokio::test]
async fn a_normal_filter_over_a_post_fits_the_default_limits() {
    let (_test, hooks, host) = setup(PluginLimits::default()).await;
    let plugin = activated(&host, "scanner-post", "scanner.wat");
    let content = post_content();

    assert_eq!(
        hooks.apply_filters(SCAN, content.clone()),
        content.to_uppercase()
    );
    assert!(!plugin.trapped());
    assert_eq!(plugin.failure(), None);
}

#[tokio::test]
async fn a_plugin_that_grows_its_memory_past_the_limit_is_stopped() {
    let (_test, hooks, host) = setup(PluginLimits::default()).await;
    let plugin = activated(&host, "grower", "grower.wat");

    hooks.do_action(GROW, &1);

    assert!(plugin.trapped());
    assert!(
        matches!(plugin.failure(), Some(HostError::OutOfMemory(ref m)) if m.contains("64 MiB")),
        "{:?}",
        plugin.failure()
    );
    assert_eq!(logged("grower", "memory"), 1);
}

#[tokio::test]
async fn a_component_whose_memory_starts_over_the_limit_does_not_load() {
    let (_test, _hooks, host) = setup(PluginLimits::default()).await;

    let error = host
        .load(manifest("big-memory", ""), &sample("big_memory.wat"))
        .expect_err("refuses the plugin");

    assert!(
        matches!(error, HostError::OutOfMemory(ref m) if m.contains("64 MiB")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_value_the_host_copies_into_the_plugin_is_bounded_by_its_memory_limit() {
    // Host to plugin only: a value larger than the plugin's memory may hold is refused when the
    // host asks the plugin's allocator for room, and the plugin is stopped. (Plugin to host, a
    // string lies in the plugin's memory, so it is at most the limit by construction.)
    let limits = PluginLimits {
        memory_max_bytes: 1024 * 1024,
        ..PluginLimits::default()
    };
    let (_test, hooks, host) = setup(limits).await;
    let plugin = activated(&host, "scanner-big", "scanner.wat");
    let content = "a".repeat(2 * 1024 * 1024);

    assert_eq!(hooks.apply_filters(SCAN, content.clone()), content);

    assert!(plugin.trapped());
    assert!(
        matches!(plugin.failure(), Some(HostError::OutOfMemory(_))),
        "{:?}",
        plugin.failure()
    );
}

#[tokio::test]
async fn too_few_instances_refuse_the_component() {
    // `title_filter.wat` makes two core instances.
    let limits = PluginLimits {
        instances_max: 1,
        ..PluginLimits::default()
    };
    let (_test, _hooks, host) = setup(limits).await;

    let error = host
        .load(
            manifest("few-instances", "\"hooks\""),
            &sample("title_filter.wat"),
        )
        .expect_err("refuses the plugin");

    assert!(matches!(error, HostError::Instantiate(_)), "{error:?}");
}

#[tokio::test]
async fn a_normal_plugin_still_works_after_another_was_stopped() {
    let (_test, hooks, host) = setup(PluginLimits::default()).await;
    let looper = activated(&host, "looper-other", "looper.wat");
    let grower = activated(&host, "grower-other", "grower.wat");
    let title = activated(&host, "title-other", "title_filter.wat");

    hooks.do_action(SPIN, &1);
    hooks.do_action(GROW, &1);
    assert!(looper.trapped());
    assert!(grower.trapped());

    assert_eq!(hooks.apply_filters(TITLE, "Hello".to_owned()), "Filtered");
    assert!(!title.trapped());
}

#[tokio::test]
async fn a_plugin_that_logs_forever_is_cut_off_and_stopped() {
    let (_test, _hooks, host) = setup(PluginLimits::default()).await;
    let plugin = host
        .load(manifest("logger", ""), &sample("logger.wat"))
        .expect("loads the plugin");

    // Each log call costs fuel, so the loop ends.
    assert!(
        matches!(plugin.init(), Err(HostError::OutOfFuel { .. })),
        "{:?}",
        plugin.failure()
    );
    // The call's lines past its budget are dropped, after one warning.
    assert_eq!(logged("logger", "spam"), 100);
    assert_eq!(logged("logger", "lines are dropped"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_reads_content_forever_runs_out_of_fuel() {
    let test = database().await;
    let db = test.db();
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    capture_logs();
    // Logs are not the limit here: the loop logs once per query, so the lines count them.
    let limits = PluginLimits {
        log_lines_per_call: 1_000_000,
        log_bytes_per_call: 1_000_000_000,
        ..PluginLimits::default()
    };
    let host = PluginHost::with_limits(Arc::new(Hooks::new()), test.shared(), limits)
        .expect("starts the host");
    let plugin = host
        .load(
            manifest("reader", "\"read-content\""),
            &sample("reader.wat"),
        )
        .expect("loads the plugin");

    assert!(
        matches!(plugin.init(), Err(HostError::OutOfFuel { .. })),
        "{:?}",
        plugin.failure()
    );
    // Two host calls per round at a millionth of the call's fuel each: at most 500 queries.
    let queries = logged("reader", "queried");
    assert!(queries > 0 && queries <= 500, "{queries} queries");
}

#[tokio::test]
async fn a_table_over_the_limit_does_not_load() {
    let limits = PluginLimits {
        table_max_elements: 100,
        ..PluginLimits::default()
    };
    let (_test, _hooks, host) = setup(limits).await;

    let error = host
        .load(manifest("big-table", ""), &sample("big_table.wat"))
        .expect_err("refuses the plugin");

    assert!(
        matches!(error, HostError::OutOfMemory(ref m) if m.contains("200 elements")),
        "{error:?}"
    );
}

#[tokio::test]
async fn fuel_runs_out_in_the_allocator_while_the_host_copies_arguments_in() {
    let limits = PluginLimits {
        fuel_per_call: 1_000_000,
        // `init` makes host calls; keep them cheap so the call itself runs out.
        fuel_per_host_call: 1_000,
        ..PluginLimits::default()
    };
    let (_test, hooks, host) = setup(limits).await;
    let plugin = activated(&host, "realloc-spinner", "realloc_spinner.wat");

    assert_eq!(hooks.apply_filters(SLOW, "kept".to_owned()), "kept");

    assert!(plugin.trapped());
    assert_eq!(
        plugin.failure(),
        Some(HostError::OutOfFuel { fuel: 1_000_000 })
    );
}

#[tokio::test]
async fn a_start_function_that_loops_forever_does_not_load() {
    let limits = PluginLimits {
        fuel_per_call: 1_000_000,
        ..PluginLimits::default()
    };
    let (_test, _hooks, host) = setup(limits).await;

    let error = host
        .load(manifest("start-spinner", ""), &sample("start_spinner.wat"))
        .expect_err("refuses the plugin");

    assert_eq!(error, HostError::OutOfFuel { fuel: 1_000_000 });
}

#[tokio::test]
async fn a_stack_overflow_stops_the_plugin() {
    let (_test, hooks, host) = setup(PluginLimits::default()).await;
    let plugin = activated(&host, "recurser", "recurser.wat");

    hooks.do_action(DEEP, &1);

    assert!(plugin.trapped());
    assert!(
        matches!(plugin.failure(), Some(HostError::Trap(_))),
        "{:?}",
        plugin.failure()
    );
}
