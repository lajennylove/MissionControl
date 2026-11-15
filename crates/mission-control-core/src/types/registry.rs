//! The registry of custom post types, taxonomies and field groups (design 5.3): every
//! definition the site uses, wherever it was made, and the REST registries they feed.
//!
//! Definitions come three ways, and the [`Origin`] says which:
//!
//! - **Code and plugins** ([`Origin::Code`], [`Origin::Plugin`]) call
//!   [`register`](Registry::register) at every start, as a theme calls `register_post_type` on
//!   `init`. They are held in memory only, never written to the database, and are locked in the
//!   admin. [`remove_plugin`](Registry::remove_plugin) drops every definition of a plugin, as on
//!   its deactivation.
//! - **The admin** ([`Origin::Admin`]) saves through
//!   [`save_admin_definition`](Registry::save_admin_definition) and
//!   [`delete_admin_definition`](Registry::delete_admin_definition), which write the
//!   `post_types`, `taxonomies` and `field_groups` tables (each row the definition's JSON), and
//!   keep `field_group_locations` (one row per location rule of a stored group) in step with the
//!   group's JSON. [`load_from_db`](Registry::load_from_db) reads them back at start.
//!
//! # A key defined twice
//!
//! ACF's local JSON rule: a definition made in code or by a plugin (a "local" one) replaces an
//! admin definition with the same slug or key in use. The admin copy stays stored, and
//! [`is_overridden`](Registry::is_overridden) reports it, so the admin can show it as overridden
//! and delete it; saving it again is refused ([`DefinitionError::Locked`], naming the local
//! origin), since the change would not take effect. When the local definition goes (its plugin
//! is deactivated, or the code no longer registers it at the next start), the stored one is used
//! again. Two local definitions with the same key: the last registered wins, as
//! `register_post_type` called twice keeps the second.
//!
//! # What the registry feeds
//!
//! After every change, the definitions in use are registered on the REST registries the app
//! serves: each post type on [`PostTypes`] (with its capability names), each taxonomy on
//! [`Taxonomies`]; a removed one is unregistered. A taxonomy whose `object_type` names a post
//! type is listed on that type, `post` and `page` included, as `register_taxonomy` attaches it.
//! Types and taxonomies whose `show_in_rest` is false are registered too, and not served. Call
//! `register_routes` on the REST route registry afterwards so discovery lists new routes.
//!
//! Known limits, deferred (ruling R90):
//!
//! - The REST route registry has no unregister, so a removed type's or taxonomy's routes stay
//!   listed in `/wp-json/` discovery until the next start; requests to them answer
//!   `rest_no_route`.
//! - A built-in `post` or `page` that a registered taxonomy is attached to is registered again
//!   from WordPress's own descriptor ([`PostType::post`], [`PostType::page`]), so it replaces a
//!   descriptor other code registered for it on [`PostTypes`].
//!
//! A post type's [`field_groups`](PostTypeDef::field_groups) are a shorthand for location rules
//! (Ruling R81: the rules are the source of truth): [`all_field_groups`](Registry::all_field_groups)
//! gives each group named there an or-group `post_type == {slug}`. That rule lives in the
//! registry's view only; the stored group's JSON and its `field_group_locations` rows hold the
//! group's own rules.
//!
//! ```ignore
//! let registry = Registry::new(post_types, taxonomies);
//! registry.load_from_db(&db).await?;
//! registry.register(PostTypeDef { origin: Origin::Code, ..PostTypeDef::new("book", "Books", "Book") })?;
//! ```

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{NaiveDateTime, SubsecRound, Utc};
use orbit_orm::{Database, Error, Transaction, Value as Sql, raw};
use tokio::sync::Mutex;

use super::definition::{
    DefinitionError, FieldGroupDef, LocationOperator, LocationParam, LocationRule, Origin,
    PostTypeDef, TaxonomyDef,
};
use crate::rest::posts::{PostType, PostTypes, RestTaxonomy};
use crate::rest::terms::Taxonomies;

/// One definition of any kind, as [`Registry::register`] and
/// [`Registry::save_admin_definition`] take it.
// A post type is the largest variant; a definition is moved once per registration, so the
// variants are not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Definition {
    /// A custom post type.
    PostType(PostTypeDef),
    /// A custom taxonomy.
    Taxonomy(TaxonomyDef),
    /// A field group.
    FieldGroup(FieldGroupDef),
}

impl Definition {
    /// The definition's kind.
    pub fn kind(&self) -> DefinitionKind {
        match self {
            Self::PostType(_) => DefinitionKind::PostType,
            Self::Taxonomy(_) => DefinitionKind::Taxonomy,
            Self::FieldGroup(_) => DefinitionKind::FieldGroup,
        }
    }

    /// The slug of a post type or taxonomy, or the key of a field group.
    pub fn key(&self) -> &str {
        match self {
            Self::PostType(def) => def.key(),
            Self::Taxonomy(def) => def.key(),
            Self::FieldGroup(def) => def.key(),
        }
    }

    /// Who made the definition.
    pub fn origin(&self) -> &Origin {
        match self {
            Self::PostType(def) => &def.origin,
            Self::Taxonomy(def) => &def.origin,
            Self::FieldGroup(def) => &def.origin,
        }
    }

    /// Checks the definition, as its own `validate`.
    fn validate(&self) -> Result<(), DefinitionError> {
        match self {
            Self::PostType(def) => def.validate(),
            Self::Taxonomy(def) => def.validate(),
            Self::FieldGroup(def) => def.validate(),
        }
    }

    /// The definition as the JSON stored in its table.
    fn to_json(&self) -> String {
        let json = match self {
            Self::PostType(def) => serde_json::to_string(def),
            Self::Taxonomy(def) => serde_json::to_string(def),
            Self::FieldGroup(def) => serde_json::to_string(def),
        };
        // The definitions hold only strings, numbers, booleans and JSON values, whose
        // serialization cannot fail.
        json.unwrap_or_else(|e| unreachable!("a definition serializes: {e}"))
    }
}

impl From<PostTypeDef> for Definition {
    fn from(def: PostTypeDef) -> Self {
        Self::PostType(def)
    }
}

impl From<TaxonomyDef> for Definition {
    fn from(def: TaxonomyDef) -> Self {
        Self::Taxonomy(def)
    }
}

impl From<FieldGroupDef> for Definition {
    fn from(def: FieldGroupDef) -> Self {
        Self::FieldGroup(def)
    }
}

/// The kinds of definition, each with its own table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefinitionKind {
    /// Post types, in `post_types`, by slug.
    PostType,
    /// Taxonomies, in `taxonomies`, by slug.
    Taxonomy,
    /// Field groups, in `field_groups`, by key.
    FieldGroup,
}

impl DefinitionKind {
    /// The table the admin's definitions of this kind are stored in.
    pub fn table(self) -> &'static str {
        match self {
            Self::PostType => "post_types",
            Self::Taxonomy => "taxonomies",
            Self::FieldGroup => "field_groups",
        }
    }

    /// The column of the table that holds the slug or key.
    fn key_column(self) -> &'static str {
        match self {
            Self::PostType | Self::Taxonomy => "slug",
            Self::FieldGroup => "group_key",
        }
    }

    /// The kind's name, for messages.
    fn name(self) -> &'static str {
        match self {
            Self::PostType => "post type",
            Self::Taxonomy => "taxonomy",
            Self::FieldGroup => "field group",
        }
    }
}

/// Why the registry refused a change.
#[derive(Debug)]
pub enum RegistryError {
    /// The definition is invalid, or locked: made in code or by a plugin, or overridden by one.
    Definition(DefinitionError),
    /// [`register`](Registry::register) was given an admin definition: those are saved with
    /// [`save_admin_definition`](Registry::save_admin_definition).
    AdminOrigin {
        /// The definition's kind.
        kind: DefinitionKind,
        /// Its slug or key.
        key: String,
    },
    /// [`register_exclusive`](Registry::register_exclusive) was given a definition whose slug
    /// or key a definition made in code or by another plugin holds.
    Taken {
        /// The definition's kind.
        kind: DefinitionKind,
        /// Its slug or key.
        key: String,
        /// Who made the definition that holds it.
        holder: Origin,
    },
    /// The database refused a query.
    Database(Error),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Definition(e) => e.fmt(f),
            Self::AdminOrigin { kind, key } => write!(
                f,
                "the {} `{key}` is an admin definition: save it, do not register it",
                kind.name()
            ),
            Self::Taken { kind, key, holder } => {
                let holder = match holder {
                    Origin::Plugin(name) => format!("the plugin `{name}`"),
                    Origin::Code => "the site's code".to_owned(),
                    Origin::Admin => "the admin".to_owned(),
                };
                write!(
                    f,
                    "the {} `{key}` is already registered by {holder}",
                    kind.name()
                )
            }
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Definition(e) => Some(e),
            Self::AdminOrigin { .. } | Self::Taken { .. } => None,
            Self::Database(e) => Some(e),
        }
    }
}

impl From<DefinitionError> for RegistryError {
    fn from(e: DefinitionError) -> Self {
        Self::Definition(e)
    }
}

impl From<Error> for RegistryError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

/// What the registry reads from each kind of definition.
trait Keyed: Clone {
    /// The slug or key.
    fn key(&self) -> &str;
    /// Who made it.
    fn origin(&self) -> &Origin;
}

impl Keyed for PostTypeDef {
    fn key(&self) -> &str {
        &self.slug
    }
    fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl Keyed for TaxonomyDef {
    fn key(&self) -> &str {
        &self.slug
    }
    fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl Keyed for FieldGroupDef {
    fn key(&self) -> &str {
        &self.key
    }
    fn origin(&self) -> &Origin {
        &self.origin
    }
}

/// Definitions of each kind, in the order they were registered or stored.
#[derive(Debug, Default)]
struct Set {
    post_types: Vec<PostTypeDef>,
    taxonomies: Vec<TaxonomyDef>,
    field_groups: Vec<FieldGroupDef>,
}

impl Set {
    /// Adds `def`, replacing the one with the same key in its place.
    fn put(&mut self, def: Definition) {
        match def {
            Definition::PostType(def) => upsert(&mut self.post_types, def),
            Definition::Taxonomy(def) => upsert(&mut self.taxonomies, def),
            Definition::FieldGroup(def) => upsert(&mut self.field_groups, def),
        }
    }

    /// Removes the definition of `kind` with `key`. True when there was one.
    fn remove(&mut self, kind: DefinitionKind, key: &str) -> bool {
        match kind {
            DefinitionKind::PostType => remove(&mut self.post_types, key),
            DefinitionKind::Taxonomy => remove(&mut self.taxonomies, key),
            DefinitionKind::FieldGroup => remove(&mut self.field_groups, key),
        }
    }

    /// The origin of the definition of `kind` with `key`, if there is one.
    fn origin(&self, kind: DefinitionKind, key: &str) -> Option<&Origin> {
        match kind {
            DefinitionKind::PostType => find(&self.post_types, key),
            DefinitionKind::Taxonomy => find(&self.taxonomies, key),
            DefinitionKind::FieldGroup => find(&self.field_groups, key),
        }
    }
}

fn upsert<T: Keyed>(defs: &mut Vec<T>, def: T) {
    match defs.iter_mut().find(|d| d.key() == def.key()) {
        Some(slot) => *slot = def,
        None => defs.push(def),
    }
}

fn remove<T: Keyed>(defs: &mut Vec<T>, key: &str) -> bool {
    let before = defs.len();
    defs.retain(|d| d.key() != key);
    defs.len() != before
}

fn find<'a, T: Keyed>(defs: &'a [T], key: &str) -> Option<&'a Origin> {
    defs.iter().find(|d| d.key() == key).map(Keyed::origin)
}

/// The definitions in use: every local one, then each admin one no local one overrides.
fn in_use<T: Keyed>(local: &[T], admin: &[T]) -> Vec<T> {
    local
        .iter()
        .chain(
            admin
                .iter()
                .filter(|a| !local.iter().any(|l| l.key() == a.key())),
        )
        .cloned()
        .collect()
}

/// The registry's state: the definitions, and what it last registered on the REST registries.
#[derive(Debug, Default)]
struct State {
    /// Definitions made in code or by plugins.
    local: Set,
    /// Definitions made in the admin, as stored.
    admin: Set,
    /// The post types last registered on [`PostTypes`], to unregister when they go.
    synced_post_types: Vec<String>,
    /// The taxonomies last registered on [`Taxonomies`].
    synced_taxonomies: Vec<String>,
    /// The built-in post types last registered with a registered taxonomy attached.
    synced_builtins: Vec<&'static str>,
}

/// A built-in post type's name, and the function that makes its descriptor.
type Builtin = (&'static str, fn() -> PostType);

/// WordPress's own post types that a custom taxonomy may be attached to, with the descriptor
/// each starts from.
const BUILTIN_POST_TYPES: [Builtin; 2] = [("post", PostType::post), ("page", PostType::page)];

/// The custom post types, taxonomies and field groups, from code, plugins and the admin. Shared,
/// like [`Hooks`](crate::hooks::Hooks): every method takes `&self`.
#[derive(Debug)]
pub struct Registry {
    post_types: Arc<PostTypes>,
    taxonomies: Arc<Taxonomies>,
    state: RwLock<State>,
    /// Held by each admin write and by [`load_from_db`](Self::load_from_db) from before its
    /// first query until memory is updated, so the stored and the held admin definitions
    /// cannot disagree. The state lock is never held across an `.await`.
    admin_writes: Mutex<()>,
}

impl Default for Registry {
    /// A registry feeding new REST registries of its own.
    fn default() -> Self {
        Self::new(Arc::new(PostTypes::new()), Arc::new(Taxonomies::new()))
    }
}

impl Registry {
    /// A registry with no definitions, feeding `post_types` and `taxonomies`: the REST
    /// registries the app serves.
    pub fn new(post_types: Arc<PostTypes>, taxonomies: Arc<Taxonomies>) -> Self {
        Self {
            post_types,
            taxonomies,
            state: RwLock::new(State::default()),
            admin_writes: Mutex::new(()),
        }
    }

    /// The REST post type registry this registry feeds.
    pub fn post_types(&self) -> &Arc<PostTypes> {
        &self.post_types
    }

    /// The REST taxonomy registry this registry feeds.
    pub fn taxonomies(&self) -> &Arc<Taxonomies> {
        &self.taxonomies
    }

    /// Registers a definition made in code or by a plugin, held in memory only. It replaces a
    /// local definition with the same key, and overrides an admin one (see the module
    /// documentation).
    ///
    /// # Errors
    ///
    /// [`RegistryError::Definition`] when it is invalid, and [`RegistryError::AdminOrigin`] for
    /// an admin definition, which [`save_admin_definition`](Self::save_admin_definition) takes.
    pub fn register(&self, def: impl Into<Definition>) -> Result<(), RegistryError> {
        let def = def.into();
        if !def.origin().is_locked() {
            return Err(RegistryError::AdminOrigin {
                kind: def.kind(),
                key: def.key().to_owned(),
            });
        }
        def.validate()?;
        let mut state = self.write();
        state.local.put(def);
        self.sync(&mut state);
        Ok(())
    }

    /// Registers a definition as [`register`](Self::register) does, but refuses one whose slug
    /// or key a definition made in code or with another origin holds: a plugin may replace its
    /// own definitions and override the admin's, not another plugin's or the site's code's.
    /// The check and the registration are one step.
    ///
    /// # Errors
    ///
    /// As [`register`](Self::register), and [`RegistryError::Taken`] with the holder's origin.
    pub fn register_exclusive(&self, def: impl Into<Definition>) -> Result<(), RegistryError> {
        let def = def.into();
        if !def.origin().is_locked() {
            return Err(RegistryError::AdminOrigin {
                kind: def.kind(),
                key: def.key().to_owned(),
            });
        }
        def.validate()?;
        let mut state = self.write();
        if let Some(holder) = state.local.origin(def.kind(), def.key())
            && holder != def.origin()
        {
            return Err(RegistryError::Taken {
                kind: def.kind(),
                key: def.key().to_owned(),
                holder: holder.clone(),
            });
        }
        state.local.put(def);
        self.sync(&mut state);
        Ok(())
    }

    /// Drops every definition the plugin `name` registered, as on its deactivation; an admin
    /// definition one of them overrode is used again. Returns how many were dropped.
    pub fn remove_plugin(&self, name: &str) -> usize {
        let mut state = self.write();
        let local = &mut state.local;
        let of_plugin = |origin: &Origin| matches!(origin, Origin::Plugin(p) if p == name);
        let before = local.post_types.len() + local.taxonomies.len() + local.field_groups.len();
        local.post_types.retain(|d| !of_plugin(&d.origin));
        local.taxonomies.retain(|d| !of_plugin(&d.origin));
        local.field_groups.retain(|d| !of_plugin(&d.origin));
        let after = local.post_types.len() + local.taxonomies.len() + local.field_groups.len();
        self.sync(&mut state);
        before - after
    }

    /// Saves an admin definition: stores it (replacing the stored one with the same key) and
    /// uses it. A field group's location rules are written to `field_group_locations` in the
    /// same transaction.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Definition`] when it is invalid, when its origin is not
    /// [`Origin::Admin`] (a definition made in code or by a plugin is never stored), or when a
    /// local definition with the same key overrides it ([`DefinitionError::Locked`] with that
    /// definition's origin); [`RegistryError::Database`] when the database refuses it.
    pub async fn save_admin_definition(
        &self,
        db: &Database,
        def: impl Into<Definition>,
    ) -> Result<(), RegistryError> {
        let def = def.into();
        def.origin().ensure_editable()?;
        def.validate()?;
        let _writing = self.admin_writes.lock().await;
        let local = self.read().local.origin(def.kind(), def.key()).cloned();
        if let Some(origin) = local {
            return Err(DefinitionError::Locked(origin).into());
        }
        let json = def.to_json();
        db.transaction(async |tx| store(tx, &def, &json).await)
            .await?;
        let mut state = self.write();
        state.admin.put(def);
        self.sync(&mut state);
        Ok(())
    }

    /// Deletes the admin definition of `kind` with `key` from the database and stops using it.
    /// An overridden one may be deleted. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Definition`] ([`DefinitionError::Locked`]) when the only definition
    /// with that key was made in code or by a plugin (a stored row counts even before
    /// [`load_from_db`](Self::load_from_db) ran), and [`RegistryError::Database`].
    pub async fn delete_admin_definition(
        &self,
        db: &Database,
        kind: DefinitionKind,
        key: &str,
    ) -> Result<bool, RegistryError> {
        let _writing = self.admin_writes.lock().await;
        let (held, local) = {
            let state = self.read();
            (
                state.admin.origin(kind, key).is_some(),
                state.local.origin(kind, key).cloned(),
            )
        };
        let deleted = db
            .transaction::<_, Error, _>(async |tx| {
                // A stored row counts even before `load_from_db` read it into memory.
                if let Some(origin) = local.filter(|_| !held)
                    && !is_stored(tx, kind, key).await?
                {
                    return Ok(Err(DefinitionError::Locked(origin)));
                }
                unstore(tx, kind, key).await.map(Ok)
            })
            .await??;
        let mut state = self.write();
        let removed = state.admin.remove(kind, key);
        self.sync(&mut state);
        Ok(deleted || removed)
    }

    /// Reads the admin definitions from the database, replacing those held, and uses them.
    /// Each is read with `from_json(json, Origin::Admin)`; a stored definition that no longer
    /// reads (written by a later version, or edited by hand), or whose JSON names another slug
    /// or key than its row, is logged and left out, as ACF skips a group it cannot read.
    /// Returns how many were loaded.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Database`] when a table cannot be read, such as before the migrations
    /// ran. Nothing changes then.
    pub async fn load_from_db(&self, db: &Database) -> Result<usize, RegistryError> {
        let _writing = self.admin_writes.lock().await;
        let mut admin = Set::default();
        for (key, json) in rows(db, DefinitionKind::PostType).await? {
            if let Some(def) = read_row(
                DefinitionKind::PostType,
                &key,
                &json,
                PostTypeDef::from_json,
            ) {
                upsert(&mut admin.post_types, def);
            }
        }
        for (key, json) in rows(db, DefinitionKind::Taxonomy).await? {
            if let Some(def) = read_row(
                DefinitionKind::Taxonomy,
                &key,
                &json,
                TaxonomyDef::from_json,
            ) {
                upsert(&mut admin.taxonomies, def);
            }
        }
        for (key, json) in rows(db, DefinitionKind::FieldGroup).await? {
            if let Some(def) = read_row(
                DefinitionKind::FieldGroup,
                &key,
                &json,
                FieldGroupDef::from_json,
            ) {
                upsert(&mut admin.field_groups, def);
            }
        }
        let loaded = admin.post_types.len() + admin.taxonomies.len() + admin.field_groups.len();
        let mut state = self.write();
        state.admin = admin;
        self.sync(&mut state);
        Ok(loaded)
    }

    /// The post types in use: those made in code and by plugins, in the order registered, then
    /// the admin's not overridden, in the order stored.
    pub fn all_post_types(&self) -> Vec<PostTypeDef> {
        let state = self.read();
        in_use(&state.local.post_types, &state.admin.post_types)
    }

    /// The taxonomies in use, in the order of [`all_post_types`](Self::all_post_types).
    pub fn all_taxonomies(&self) -> Vec<TaxonomyDef> {
        let state = self.read();
        in_use(&state.local.taxonomies, &state.admin.taxonomies)
    }

    /// The field groups in use, in the order of [`all_post_types`](Self::all_post_types). Each
    /// group a post type in use names in its `field_groups` has the location or-group
    /// `post_type == {slug}`, unless its rules have that or-group already.
    pub fn all_field_groups(&self) -> Vec<FieldGroupDef> {
        let state = self.read();
        let mut groups = in_use(&state.local.field_groups, &state.admin.field_groups);
        for post_type in in_use(&state.local.post_types, &state.admin.post_types) {
            for key in &post_type.field_groups {
                if let Some(group) = groups.iter_mut().find(|g| &g.key == key) {
                    let rule = vec![LocationRule {
                        param: LocationParam::PostType,
                        operator: LocationOperator::Is,
                        value: post_type.slug.clone(),
                    }];
                    if !group.location.contains(&rule) {
                        group.location.push(rule);
                    }
                }
            }
        }
        groups
    }

    /// The post type in use named `slug`.
    pub fn post_type(&self, slug: &str) -> Option<PostTypeDef> {
        self.all_post_types().into_iter().find(|d| d.slug == slug)
    }

    /// The taxonomy in use named `slug`.
    pub fn taxonomy(&self, slug: &str) -> Option<TaxonomyDef> {
        self.all_taxonomies().into_iter().find(|d| d.slug == slug)
    }

    /// The field group in use with `key`, with the rules
    /// [`all_field_groups`](Self::all_field_groups) adds.
    pub fn field_group(&self, key: &str) -> Option<FieldGroupDef> {
        self.all_field_groups().into_iter().find(|d| d.key == key)
    }

    /// Whether the admin definition of `kind` with `key` is stored but not used, because a
    /// definition made in code or by a plugin has the same key.
    pub fn is_overridden(&self, kind: DefinitionKind, key: &str) -> bool {
        let state = self.read();
        state.admin.origin(kind, key).is_some() && state.local.origin(kind, key).is_some()
    }

    /// Registers the definitions in use on the REST registries, and unregisters those that went.
    /// A descriptor that has not changed is left in its place, so the registries keep their
    /// order.
    fn sync(&self, state: &mut State) {
        let taxonomies = in_use(&state.local.taxonomies, &state.admin.taxonomies);
        for name in &state.synced_taxonomies {
            if !taxonomies.iter().any(|t| &t.slug == name) {
                self.taxonomies.unregister(name);
            }
        }
        for def in &taxonomies {
            let spec = def.to_taxonomy_spec();
            if self.taxonomies.registered(&def.slug).as_ref() != Some(&spec) {
                self.taxonomies.register(spec);
            }
        }
        state.synced_taxonomies = taxonomies.iter().map(|t| t.slug.clone()).collect();

        let known: Vec<RestTaxonomy> = self
            .taxonomies
            .shown_in_rest()
            .into_iter()
            .map(|spec| RestTaxonomy {
                taxonomy: spec.taxonomy,
                rest_base: Cow::Owned(spec.rest_base),
            })
            .collect();
        // `register_taxonomy`'s `object_type` attaches the taxonomy to those post types.
        let attach = |post_type: &mut PostType| {
            for def in taxonomies.iter().filter(|t| t.show_in_rest) {
                let named = post_type
                    .taxonomies
                    .iter()
                    .any(|t| t.taxonomy.name == def.slug);
                if !named && def.object_type.contains(&post_type.name) {
                    post_type.taxonomies.push(def.to_rest_taxonomy());
                }
            }
        };

        let post_types = in_use(&state.local.post_types, &state.admin.post_types);
        for name in &state.synced_post_types {
            if !post_types.iter().any(|t| &t.slug == name) {
                self.post_types.unregister(name);
            }
        }
        for def in &post_types {
            let mut post_type = def.to_post_type(&known);
            attach(&mut post_type);
            self.put_post_type(post_type);
        }
        state.synced_post_types = post_types.iter().map(|t| t.slug.clone()).collect();

        // A built-in type is touched only when a registered taxonomy is attached to it, or was.
        // It starts from WordPress's descriptor, not the one registered: another registration
        // of `post` or `page` is replaced (deferred, see the module documentation).
        let mut extended = Vec::new();
        for (name, builtin) in BUILTIN_POST_TYPES {
            let base = builtin();
            let mut post_type = base.clone();
            attach(&mut post_type);
            let changed = post_type.taxonomies != base.taxonomies;
            if changed || state.synced_builtins.contains(&name) {
                self.put_post_type(post_type);
            }
            if changed {
                extended.push(name);
            }
        }
        state.synced_builtins = extended;
    }

    /// Registers `post_type` on [`PostTypes`] unless it is registered as it is.
    fn put_post_type(&self, post_type: PostType) {
        if self.post_types.registered(&post_type.name).as_ref() != Some(&post_type) {
            self.post_types.register(post_type);
        }
    }

    // A poisoned lock only means a registering thread panicked; the definitions are still whole.
    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// The definition stored as `json` in the row of `kind` with `key`, read with `from_json`, or
/// `None` (logged) when it does not read or its JSON names another slug or key than the row.
fn read_row<T: Keyed>(
    kind: DefinitionKind,
    key: &str,
    json: &str,
    from_json: fn(&str, Origin) -> Result<T, DefinitionError>,
) -> Option<T> {
    match from_json(json, Origin::Admin) {
        Ok(def) if def.key() == key => Some(def),
        Ok(def) => {
            log::warn!(
                "The stored {} `{key}` was not loaded: its definition names `{}`",
                kind.name(),
                def.key()
            );
            None
        }
        Err(e) => {
            log::warn!("The stored {} `{key}` was not loaded: {e}", kind.name());
            None
        }
    }
}

/// Whether a definition of `kind` with `key` is stored.
async fn is_stored(tx: &mut Transaction, kind: DefinitionKind, key: &str) -> Result<bool, Error> {
    let found: i64 = raw::fetch_scalar(
        &mut *tx,
        &format!(
            "SELECT COUNT(*) FROM {} WHERE {} = ?",
            kind.table(),
            kind.key_column()
        ),
        &[key.into()],
    )
    .await?;
    Ok(found > 0)
}

/// The current time in UTC, to the second, as the database stores it.
fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

/// Every stored definition of `kind`: its key and its JSON, in the order stored.
async fn rows(db: &Database, kind: DefinitionKind) -> Result<Vec<(String, String)>, Error> {
    let sql = format!(
        "SELECT {}, definition FROM {} ORDER BY id",
        kind.key_column(),
        kind.table()
    );
    raw::fetch_all(db, &sql, &[]).await
}

/// Stores `def` as `json`, replacing the row with its key, and a field group's location rules.
async fn store(tx: &mut Transaction, def: &Definition, json: &str) -> Result<(), Error> {
    let kind = def.kind();
    let (table, column) = (kind.table(), kind.key_column());
    let key = def.key();
    let now = now();
    // Counted, not read from the rows an `UPDATE` changed: MySQL counts no change when the
    // values are the same.
    let found: i64 = raw::fetch_scalar(
        &mut *tx,
        &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?"),
        &[key.into()],
    )
    .await?;
    if found > 0 {
        raw::execute(
            &mut *tx,
            &format!("UPDATE {table} SET definition = ?, updated_at = ? WHERE {column} = ?"),
            &[json.into(), now.into(), key.into()],
        )
        .await?;
    } else {
        raw::execute(
            &mut *tx,
            &format!(
                "INSERT INTO {table} ({column}, definition, created_at, updated_at) \
                 VALUES (?, ?, ?, ?)"
            ),
            &[key.into(), json.into(), now.into(), now.into()],
        )
        .await?;
    }
    if let Definition::FieldGroup(group) = def {
        store_locations(tx, group).await?;
    }
    Ok(())
}

/// Replaces the `field_group_locations` rows of `group` with its location rules.
async fn store_locations(tx: &mut Transaction, group: &FieldGroupDef) -> Result<(), Error> {
    raw::execute(
        &mut *tx,
        "DELETE FROM field_group_locations WHERE group_key = ?",
        &[group.key.as_str().into()],
    )
    .await?;
    for (or_group, rules) in (0_i64..).zip(&group.location) {
        for (position, rule) in (0_i64..).zip(rules) {
            let binds: [Sql; 6] = [
                group.key.as_str().into(),
                or_group.into(),
                position.into(),
                rule.param.as_str().into(),
                rule.operator.as_str().into(),
                rule.value.as_str().into(),
            ];
            raw::execute(
                &mut *tx,
                "INSERT INTO field_group_locations \
                 (group_key, or_group, position, param, operator, value) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                &binds,
            )
            .await?;
        }
    }
    Ok(())
}

/// Deletes the stored definition of `kind` with `key`, and a field group's location rows
/// (by hand: SQLite enforces the foreign key's cascade only with `foreign_keys` on). True when
/// there was one.
async fn unstore(tx: &mut Transaction, kind: DefinitionKind, key: &str) -> Result<bool, Error> {
    if kind == DefinitionKind::FieldGroup {
        raw::execute(
            &mut *tx,
            "DELETE FROM field_group_locations WHERE group_key = ?",
            &[key.into()],
        )
        .await?;
    }
    let deleted = raw::execute(
        &mut *tx,
        &format!(
            "DELETE FROM {} WHERE {} = ?",
            kind.table(),
            kind.key_column()
        ),
        &[key.into()],
    )
    .await?;
    Ok(deleted > 0)
}
