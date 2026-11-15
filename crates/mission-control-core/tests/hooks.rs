//! Tests for the hook registry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mission_control_core::hooks::{Action, DEFAULT_PRIORITY, Filter, Hooks, Owner};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct SavePost {
    id: u64,
}

const SAVE_POST: Action<SavePost> = Action::new("save_post");
const THE_TITLE: Filter<String> = Filter::new("the_title");
#[cfg(debug_assertions)]
const CLONE_A: Action<u32> = Action::new("clash");
#[cfg(debug_assertions)]
const CLONE_B: Action<String> = Action::new("clash");

type Log = Arc<Mutex<Vec<&'static str>>>;

fn recorder() -> (Log, impl Fn(&'static str) -> Log) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    (log, move |_| l.clone())
}

#[test]
fn default_priority_is_ten() {
    assert_eq!(DEFAULT_PRIORITY, 10);
}

#[test]
fn lower_priority_runs_first() {
    let hooks = Hooks::new();
    let (log, mk) = recorder();
    for (name, prio) in [("late", 20), ("early", 1), ("mid", 10)] {
        let log = mk(name);
        hooks.add_action(SAVE_POST, prio, Owner::Core, move |_| {
            log.lock().unwrap().push(name)
        });
    }
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(*log.lock().unwrap(), vec!["early", "mid", "late"]);
}

#[test]
fn equal_priority_runs_in_registration_order() {
    let hooks = Hooks::default();
    let (log, mk) = recorder();
    for name in ["a", "b", "c", "d"] {
        let log = mk(name);
        hooks.add_action(SAVE_POST, 10, Owner::Core, move |_| {
            log.lock().unwrap().push(name)
        });
    }
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(*log.lock().unwrap(), vec!["a", "b", "c", "d"]);
}

#[test]
fn filter_chain_changes_value() {
    let hooks = Hooks::new();
    hooks.add_filter(THE_TITLE, 20, Owner::Core, |v| format!("{v}!"));
    hooks.add_filter(THE_TITLE, 10, Owner::Core, |v| v.to_uppercase());
    assert_eq!(
        hooks.apply_filters(THE_TITLE, "hello".to_string()),
        "HELLO!"
    );
}

#[test]
fn action_with_no_subscribers_is_a_no_op() {
    let hooks = Hooks::new();
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(hooks.apply_filters(THE_TITLE, "x".to_string()), "x");
}

#[test]
fn callback_may_register_another_hook() {
    let hooks = Arc::new(Hooks::new());
    let count = Arc::new(AtomicUsize::new(0));
    let (h, c) = (hooks.clone(), count.clone());
    hooks.add_action(SAVE_POST, 10, Owner::Core, move |_| {
        let c = c.clone();
        h.add_action(SAVE_POST, 10, Owner::Core, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
    });
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(count.load(Ordering::SeqCst), 0);
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn adding_a_hook_while_another_thread_runs_do_action() {
    let hooks = Arc::new(Hooks::new());
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    hooks.add_action(SAVE_POST, 10, Owner::Core, move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });
    let runner = {
        let hooks = hooks.clone();
        std::thread::spawn(move || {
            for i in 0..2000 {
                hooks.do_action(SAVE_POST, &SavePost { id: i });
            }
        })
    };
    for n in 0..200 {
        let c = count.clone();
        hooks.add_action(SAVE_POST, 10, Owner::Plugin(format!("p{n}")), move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
    }
    runner.join().unwrap();
    let before = count.load(Ordering::SeqCst);
    hooks.do_action(SAVE_POST, &SavePost { id: 0 });
    assert_eq!(count.load(Ordering::SeqCst) - before, 201);
}

#[test]
fn remove_owner_removes_only_that_owners_callbacks() {
    let hooks = Hooks::new();
    let (log, mk) = recorder();
    let plugin = Owner::Plugin("seo".into());
    for (name, owner) in [
        ("core", Owner::Core),
        ("seo", plugin.clone()),
        ("theme", Owner::Theme("twenty".into())),
        ("other", Owner::Plugin("other".into())),
    ] {
        let log = mk(name);
        hooks.add_action(SAVE_POST, 10, owner.clone(), move |_| {
            log.lock().unwrap().push(name)
        });
        hooks.add_filter(THE_TITLE, 10, owner, move |v| format!("{v}{name};"));
    }
    hooks.remove_owner(&plugin);
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });
    assert_eq!(*log.lock().unwrap(), vec!["core", "theme", "other"]);
    assert_eq!(
        hooks.apply_filters(THE_TITLE, String::new()),
        "core;theme;other;"
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "clash")]
fn same_name_with_different_types_panics_in_debug() {
    let hooks = Hooks::new();
    hooks.add_action(CLONE_A, 10, Owner::Core, |_| {});
    hooks.do_action(CLONE_B, &"x".to_string());
}

#[test]
fn json_action_callbacks_get_the_payload_as_json_in_priority_order() {
    let hooks = Hooks::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    hooks.add_json_action("save_post", 20, Owner::Plugin("late".into()), move |json| {
        s.lock().unwrap().push(format!("json {json}"));
    });
    let s = seen.clone();
    hooks.add_action(SAVE_POST, DEFAULT_PRIORITY, Owner::Core, move |p| {
        s.lock().unwrap().push(format!("typed {}", p.id));
    });

    hooks.do_action(SAVE_POST, &SavePost { id: 7 });

    assert_eq!(*seen.lock().unwrap(), ["typed 7", r#"json {"id":7}"#]);
}

#[test]
fn json_filter_callbacks_change_the_value_or_keep_it() {
    let hooks = Hooks::new();
    hooks.add_filter(THE_TITLE, 1, Owner::Core, |t| format!("{t}!"));
    hooks.add_json_filter("the_title", 5, Owner::Plugin("upper".into()), |json| {
        let value: String = serde_json::from_str(json).ok()?;
        serde_json::to_string(&value.to_uppercase()).ok()
    });
    // `None` keeps the value; a value of the wrong type is refused and kept too.
    hooks.add_json_filter("the_title", 6, Owner::Plugin("none".into()), |_| None);
    hooks.add_json_filter("the_title", 7, Owner::Plugin("wrong".into()), |_| {
        Some("42".to_owned())
    });
    hooks.add_filter(THE_TITLE, 9, Owner::Core, |t| format!("<{t}>"));

    assert_eq!(hooks.apply_filters(THE_TITLE, "hi".to_owned()), "<HI!>");
}

#[test]
fn remove_owner_removes_json_callbacks() {
    let hooks = Hooks::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    hooks.add_json_action(
        "save_post",
        DEFAULT_PRIORITY,
        Owner::Plugin("p".into()),
        move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        },
    );
    hooks.add_json_filter(
        "the_title",
        DEFAULT_PRIORITY,
        Owner::Plugin("p".into()),
        |_| Some("\"x\"".to_owned()),
    );

    hooks.remove_owner(&Owner::Plugin("p".into()));
    hooks.do_action(SAVE_POST, &SavePost { id: 1 });

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(hooks.apply_filters(THE_TITLE, "t".to_owned()), "t");
}

#[test]
fn remove_plugin_removes_every_load_of_the_slug_and_nothing_else() {
    let hooks = Hooks::new();
    let owners = [
        Owner::Plugin("p".into()),
        Owner::PluginLoad {
            slug: "p".into(),
            load: 1,
        },
        Owner::PluginLoad {
            slug: "p".into(),
            load: 2,
        },
        Owner::PluginLoad {
            slug: "q".into(),
            load: 3,
        },
        Owner::Theme("p".into()),
    ];
    for owner in owners {
        hooks.add_filter(THE_TITLE, DEFAULT_PRIORITY, owner, |t| format!("{t}+"));
    }

    hooks.remove_owner(&Owner::PluginLoad {
        slug: "p".into(),
        load: 1,
    });
    assert_eq!(hooks.apply_filters(THE_TITLE, String::new()), "++++");
    hooks.remove_plugin("p");
    assert_eq!(hooks.apply_filters(THE_TITLE, String::new()), "++");
}
