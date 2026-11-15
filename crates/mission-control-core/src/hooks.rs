//! The hook registry: WordPress-style actions and filters.
//!
//! Core code, themes and sandboxed WASM plugins register callbacks on one
//! [`Hooks`] value. Hook keys are typed constants, so a payload mismatch is a
//! compile error. Payloads are `Serialize + Deserialize` so the WASM bridge can
//! pass them across the sandbox boundary: a plugin subscribes by name with
//! [`Hooks::add_json_action`] and [`Hooks::add_json_filter`], and its callback
//! gets the payload as JSON text, in the same priority order as typed callbacks.
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
    /// One load of a WASM plugin: its slug and a number the plugin host gives each load, so
    /// removing an old instance's callbacks never touches a newer instance of the same plugin.
    /// [`Hooks::remove_plugin`] removes every load of a slug.
    PluginLoad {
        /// The plugin's slug.
        slug: String,
        /// The load's number, unique in its host.
        load: u64,
    },
}

impl Owner {
    /// The plugin slug of a [`Owner::Plugin`] or [`Owner::PluginLoad`].
    fn plugin_slug(&self) -> Option<&str> {
        match self {
            Self::Plugin(slug) | Self::PluginLoad { slug, .. } => Some(slug),
            Self::Core | Self::Theme(_) => None,
        }
    }
}

type ActionFn<A> = Box<dyn Fn(&A) + Send + Sync>;
type FilterFn<V> = Box<dyn Fn(V) -> V + Send + Sync>;
type JsonActionFn = Arc<dyn Fn(&str) + Send + Sync>;
type JsonFilterFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A registered callback.
#[derive(Clone)]
enum Callback {
    /// A type-erased `ActionFn<A>` or `FilterFn<V>`.
    Typed(Arc<dyn Any + Send + Sync>),
    /// An action callback that takes the payload as JSON text.
    JsonAction(JsonActionFn),
    /// A filter callback that takes the value as JSON text and gives the new value as JSON
    /// text, or `None` to keep it.
    JsonFilter(JsonFilterFn),
}

struct Entry {
    priority: i32,
    seq: u64,
    owner: Owner,
    callback: Callback,
}

#[derive(Default)]
struct Registry {
    next_seq: u64,
    actions: HashMap<Box<str>, Vec<Entry>>,
    filters: HashMap<Box<str>, Vec<Entry>>,
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
        list: fn(&mut Registry) -> &mut HashMap<Box<str>, Vec<Entry>>,
        reg: &mut Registry,
        name: &str,
        priority: i32,
        owner: Owner,
        callback: Callback,
    ) {
        let seq = reg.next_seq;
        reg.next_seq += 1;
        let entries = list(reg).entry(name.into()).or_default();
        entries.push(Entry {
            priority,
            seq,
            owner,
            callback,
        });
        entries.sort_by_key(|e| (e.priority, e.seq));
    }

    fn snapshot(map: &HashMap<Box<str>, Vec<Entry>>, name: &str) -> Vec<Callback> {
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
            Callback::Typed(Arc::new(cb)),
        );
    }

    /// Registers `f` on the action named `name`, with the payload as JSON text: how a WASM
    /// plugin subscribes, by a name it gives at run time. Priorities are as for
    /// [`add_action`](Self::add_action), and typed and JSON callbacks run in one order.
    pub fn add_json_action(
        &self,
        name: &str,
        priority: i32,
        owner: Owner,
        f: impl Fn(&str) + Send + Sync + 'static,
    ) {
        Self::insert(
            |r| &mut r.actions,
            &mut self.write(),
            name,
            priority,
            owner,
            Callback::JsonAction(Arc::new(f)),
        );
    }

    /// Runs every callback on the action, in priority order. The payload is turned into JSON
    /// once, when the first JSON callback runs; a payload that cannot be (a map with keys that
    /// are not strings) skips the JSON callbacks, with a warning.
    pub fn do_action<A: Payload>(&self, hook: Action<A>, payload: &A) {
        let callbacks = Self::snapshot(&self.read().actions, hook.0);
        let mut json: Option<Option<String>> = None;
        for cb in callbacks {
            match cb {
                Callback::Typed(cb) => match cb.downcast_ref::<ActionFn<A>>() {
                    Some(f) => f(payload),
                    None => type_clash(hook.0, type_name::<A>()),
                },
                Callback::JsonAction(f) => {
                    if let Some(json) = json.get_or_insert_with(|| to_json(hook.0, payload)) {
                        f(json);
                    }
                }
                Callback::JsonFilter(_) => {}
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
            Callback::Typed(Arc::new(cb)),
        );
    }

    /// Registers `f` on the filter named `name`, with the value as JSON text: `f` gives the
    /// new value as JSON text, or `None` to keep the value. Priorities are as for
    /// [`add_filter`](Self::add_filter), and typed and JSON callbacks run in one order.
    pub fn add_json_filter(
        &self,
        name: &str,
        priority: i32,
        owner: Owner,
        f: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) {
        Self::insert(
            |r| &mut r.filters,
            &mut self.write(),
            name,
            priority,
            owner,
            Callback::JsonFilter(Arc::new(f)),
        );
    }

    /// Passes `value` through every callback on the filter, in priority order. A JSON callback
    /// gets the value as it is when its turn comes; a new value that does not read back as `V`
    /// is refused with a warning, and the value is kept.
    pub fn apply_filters<V: Payload>(&self, hook: Filter<V>, value: V) -> V {
        let callbacks = Self::snapshot(&self.read().filters, hook.0);
        let mut value = value;
        for cb in callbacks {
            match cb {
                Callback::Typed(cb) => match cb.downcast_ref::<FilterFn<V>>() {
                    Some(f) => value = f(value),
                    None => type_clash(hook.0, type_name::<V>()),
                },
                Callback::JsonFilter(f) => {
                    let Some(json) = to_json(hook.0, &value) else {
                        continue;
                    };
                    let Some(filtered) = f(&json) else {
                        continue;
                    };
                    match serde_json::from_str(&filtered) {
                        Ok(filtered) => value = filtered,
                        Err(e) => log::warn!(
                            "a callback on the filter `{}` gave a value that is not a `{}` ({e}); \
                             the value is kept",
                            hook.0,
                            type_name::<V>()
                        ),
                    }
                }
                Callback::JsonAction(_) => {}
            }
        }
        value
    }

    /// Removes every action and filter callback registered by `owner`.
    pub fn remove_owner(&self, owner: &Owner) {
        self.remove_where(|o| o == owner);
    }

    /// Removes every action and filter callback of the plugin `slug`: those owned by
    /// [`Owner::Plugin`] with the slug and by every [`Owner::PluginLoad`] of it.
    pub fn remove_plugin(&self, slug: &str) {
        self.remove_where(|o| o.plugin_slug() == Some(slug));
    }

    fn remove_where(&self, owned: impl Fn(&Owner) -> bool) {
        let mut guard = self.write();
        let reg = &mut *guard;
        for map in [&mut reg.actions, &mut reg.filters] {
            for entries in map.values_mut() {
                entries.retain(|e| !owned(&e.owner));
            }
            map.retain(|_, entries| !entries.is_empty());
        }
    }
}

/// `payload` as JSON text, or `None` with a warning when it cannot be written as JSON.
fn to_json<T: Serialize>(hook: &str, payload: &T) -> Option<String> {
    match serde_json::to_string(payload) {
        Ok(json) => Some(json),
        Err(e) => {
            log::warn!("the payload of the hook `{hook}` cannot be written as JSON ({e})");
            None
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
