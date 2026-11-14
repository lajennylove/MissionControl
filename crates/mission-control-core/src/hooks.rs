//! The hook registry: WordPress-style actions and filters.
//!
//! Core code, themes and (later) sandboxed WASM plugins register callbacks on
//! one [`Hooks`] value. Hook keys are typed constants, so a payload mismatch is
//! a compile error. Payloads are `Serialize + Deserialize` so the WASM bridge
//! can pass them across the sandbox boundary.
//!
//! ```
//! use mission_control_core::hooks::{Filter, Hooks, Owner, DEFAULT_PRIORITY};
//!
//! const THE_TITLE: Filter<String> = Filter::new("the_title");
//!
//! let hooks = Hooks::new();
//! hooks.add_filter(THE_TITLE, DEFAULT_PRIORITY, Owner::Core, |t| t.to_uppercase());
//! assert_eq!(hooks.apply_filters(THE_TITLE, "hi".to_string()), "HI");
//! ```

use std::any::{Any, type_name};
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The priority WordPress gives a callback when none is chosen. Lower runs first.
pub const DEFAULT_PRIORITY: i32 = 10;

/// Marker for types that can travel through a hook: serialisable for the WASM
/// bridge, and shareable across threads.
pub trait Payload: Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> Payload for T {}

/// A typed action key, declared once as a constant:
/// `const SAVE_POST: Action<SavePost> = Action::new("save_post");`
pub struct Action<A>(&'static str, PhantomData<fn(A)>);

impl<A> Action<A> {
    /// Declares an action with the given hook name.
    pub const fn new(name: &'static str) -> Self {
        Self(name, PhantomData)
    }

    /// The hook name.
    pub const fn name(&self) -> &'static str {
        self.0
    }
}

impl<A> Clone for Action<A> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<A> Copy for Action<A> {}
impl<A> fmt::Debug for Action<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Action({:?})", self.0)
    }
}

/// A typed filter key, declared once as a constant:
/// `const THE_TITLE: Filter<String> = Filter::new("the_title");`
pub struct Filter<V>(&'static str, PhantomData<fn(V) -> V>);

impl<V> Filter<V> {
    /// Declares a filter with the given hook name.
    pub const fn new(name: &'static str) -> Self {
        Self(name, PhantomData)
    }

    /// The hook name.
    pub const fn name(&self) -> &'static str {
        self.0
    }
}

impl<V> Clone for Filter<V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for Filter<V> {}
impl<V> fmt::Debug for Filter<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Filter({:?})", self.0)
    }
}

/// WordPress's `the_content` filter: a post's body on its way to the page. Shortcodes run on it
/// at priority 11 (see `Shortcodes::register_the_content` in the `content` module).
pub const THE_CONTENT: Filter<String> = Filter::new("the_content");

/// Who registered a callback, so deactivating a plugin or theme can remove
/// everything it added.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    /// Mission Control itself.
    Core,
    /// The active theme, by slug.
    Theme(String),
    /// A plugin, by slug.
    Plugin(String),
}

type ActionFn<A> = Box<dyn Fn(&A) + Send + Sync>;
type FilterFn<V> = Box<dyn Fn(V) -> V + Send + Sync>;

struct Entry {
    priority: i32,
    seq: u64,
    owner: Owner,
    /// A type-erased `ActionFn<A>` or `FilterFn<V>`.
    callback: Arc<dyn Any + Send + Sync>,
}

#[derive(Default)]
struct Registry {
    next_seq: u64,
    actions: HashMap<&'static str, Vec<Entry>>,
    filters: HashMap<&'static str, Vec<Entry>>,
}

/// The hook registry. Methods take `&self`: the registry changes while the
/// site serves, and callbacks never run under the lock, so a callback may
/// itself register or run hooks.
#[derive(Default)]
pub struct Hooks {
    inner: RwLock<Registry>,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

impl Hooks {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, Registry> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Registry> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(
        list: fn(&mut Registry) -> &mut HashMap<&'static str, Vec<Entry>>,
        reg: &mut Registry,
        name: &'static str,
        priority: i32,
        owner: Owner,
        callback: Arc<dyn Any + Send + Sync>,
    ) {
        let seq = reg.next_seq;
        reg.next_seq += 1;
        let entries = list(reg).entry(name).or_default();
        entries.push(Entry {
            priority,
            seq,
            owner,
            callback,
        });
        entries.sort_by_key(|e| (e.priority, e.seq));
    }

    fn snapshot(
        map: &HashMap<&'static str, Vec<Entry>>,
        name: &str,
    ) -> Vec<Arc<dyn Any + Send + Sync>> {
        map.get(name)
            .map(|v| v.iter().map(|e| e.callback.clone()).collect())
            .unwrap_or_default()
    }

    /// Registers `f` on an action. Lower `priority` runs first; equal
    /// priorities run in registration order.
    pub fn add_action<A: Payload>(
        &self,
        hook: Action<A>,
        priority: i32,
        owner: Owner,
        f: impl Fn(&A) + Send + Sync + 'static,
    ) {
        let cb: ActionFn<A> = Box::new(f);
        Self::insert(
            |r| &mut r.actions,
            &mut self.write(),
            hook.0,
            priority,
            owner,
            Arc::new(cb),
        );
    }

    /// Runs every callback on the action, in priority order.
    pub fn do_action<A: Payload>(&self, hook: Action<A>, payload: &A) {
        let callbacks = Self::snapshot(&self.read().actions, hook.0);
        for cb in callbacks {
            match cb.downcast_ref::<ActionFn<A>>() {
                Some(f) => f(payload),
                None => type_clash(hook.0, type_name::<A>()),
            }
        }
    }

    /// Registers `f` on a filter. Lower `priority` runs first; equal
    /// priorities run in registration order.
    pub fn add_filter<V: Payload>(
        &self,
        hook: Filter<V>,
        priority: i32,
        owner: Owner,
        f: impl Fn(V) -> V + Send + Sync + 'static,
    ) {
        let cb: FilterFn<V> = Box::new(f);
        Self::insert(
            |r| &mut r.filters,
            &mut self.write(),
            hook.0,
            priority,
            owner,
            Arc::new(cb),
        );
    }

    /// Passes `value` through every callback on the filter, in priority order.
    pub fn apply_filters<V: Payload>(&self, hook: Filter<V>, value: V) -> V {
        let callbacks = Self::snapshot(&self.read().filters, hook.0);
        let mut value = value;
        for cb in callbacks {
            match cb.downcast_ref::<FilterFn<V>>() {
                Some(f) => value = f(value),
                None => type_clash(hook.0, type_name::<V>()),
            }
        }
        value
    }

    /// Removes every action and filter callback registered by `owner`.
    pub fn remove_owner(&self, owner: &Owner) {
        let mut guard = self.write();
        let reg = &mut *guard;
        for map in [&mut reg.actions, &mut reg.filters] {
            for entries in map.values_mut() {
                entries.retain(|e| &e.owner != owner);
            }
            map.retain(|_, entries| !entries.is_empty());
        }
    }
}

/// Two different types were used under one hook name. Typed keys make this
/// impossible unless names collide: loud in debug builds, skipped in release.
fn type_clash(hook: &str, expected: &str) {
    debug_assert!(
        false,
        "hook `{hook}` was registered with a different payload type than `{expected}`"
    );
}
