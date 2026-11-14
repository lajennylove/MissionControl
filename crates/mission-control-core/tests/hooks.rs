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
