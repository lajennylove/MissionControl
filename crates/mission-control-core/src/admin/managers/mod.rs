//! The admin's managers of custom post types, taxonomies and field groups (Task 5.7, design
//! `cpt-field-manager-design.md`): the screens ACF 6 has under ACF > Post Types, Taxonomies and
//! Field Groups, in the sidebar's Site group above Settings.
//!
//! - **Post Types** at `/mc-admin/post-types` ([`post_types`]): the list (labels, key,
//!   taxonomies, posts, origin) and the editor of `register_post_type`'s options.
//! - **Taxonomies** at `/mc-admin/taxonomies` ([`taxonomies`]): the list and the editor of
//!   `register_taxonomy`'s options. Each stored taxonomy gets its term screen under Posts, as
//!   Categories and Tags have ([`TaxonomyScreens`](super::terms::TaxonomyScreens)).
//! - **Field Groups** at `/mc-admin/field-groups` ([`field_groups`]): the list and ACF's
//!   one-page editor: the title, the fields as a repeater of field rows, the location rules
//!   (or-groups of and-rules) and the presentation.
//!
//! Each is an Ignite runtime resource whose records are addressed by their key (`book`,
//! `genre`, `group_65a1…`), as Filament's `$recordRouteKeyName`. A save goes through the
//! [`Registry`] (`save_admin_definition`, `delete_admin_definition`), which stores the
//! definition and updates the REST registries, then registers the admin screens again
//! ([`Managers::sync`]): from the next request on, a new post type is in the sidebar under
//! Content with its New screen, as design 5.3 asks, without a restart.
//!
//! Definitions made in code or by a plugin, and WordPress's own `post`, `page`, `category` and
//! `post_tag`, are listed as locked: their edit form shows the values read-only with a notice
//! that says why, has no Save, and their save and Delete are refused (Ignite's read-only records
//! and `hide_default_delete_when`), as ACF shows its local JSON and PHP definitions.
//!
//! Who may use them: the three managers need `manage_options`, as ACF's admin needs its
//! `capability` setting (default `manage_options`). A post type saved here has its capabilities
//! (`capability_type`, `edit_books` and the others) granted to the Administrator role
//! ([`CapabilityGrants`]), the owner's rule for Task 5.7: WordPress itself grants them to no
//! role.
//!
//! Known limits of phase 1, each waiting on Ignite or on a later phase (see the design's gaps):
//! the New screens start blank (Ignite 0.6.8's create form has no default values, Filament's
//! `->default()`), so Public, Show in REST API and the supports are not pre-ticked as ACF does;
//! a field row offers the settings every type shares and the choices, not each type's own
//! settings, since Ignite cannot yet show a row's sub-field by the row's `type` (Filament's
//! `$get('type')` in a repeater); and the Menus screen and REST discovery see a new type at the
//! next start (gap M8).

use std::sync::Arc;

use ignite_core::{RuntimeError, RuntimeFuture};
use orbit_orm::{Database, Error};
use serde_json::Value;

use super::terms::TaxonomyScreens;
use crate::types::{DefinitionError, Origin, PostTypeResources, Registry, RegistryError};

pub mod field_groups;
pub mod post_types;
pub mod taxonomies;

pub use field_groups::{FIELD_GROUPS, FieldGroupsData, field_groups_resource};
pub use post_types::{POST_TYPES, PostTypesData, post_types_resource};
pub use taxonomies::{TAXONOMIES, TaxonomiesData, taxonomies_resource};

/// The slugs of the three managers, in the sidebar's order.
pub const MANAGERS: [&str; 3] = [POST_TYPES, TAXONOMIES, FIELD_GROUPS];

/// The capability the managers need, as ACF's admin and the settings screens.
pub const MANAGE_OPTIONS: &str = "manage_options";

/// The WordPress role a new post type's capabilities are granted to.
pub const ADMINISTRATOR: &str = "administrator";

/// Grants capabilities to a role when a post type is saved. The app implements it with Ignite's
/// permissions (`ignite_panel::authz`); the core crate has no Ignite panel.
pub trait CapabilityGrants: Send + Sync + 'static {
    /// Registers each of `caps` as a permission when it is not one yet, and grants it to the
    /// role `role`. Granting a capability the role holds changes nothing.
    fn grant<'a>(
        &'a self,
        db: &'a Database,
        role: &'a str,
        caps: Vec<String>,
    ) -> RuntimeFuture<'a, ()>;
}

/// Grants nothing: for a site without the admin's roles, and for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoGrants;

impl CapabilityGrants for NoGrants {
    fn grant<'a>(&'a self, _: &'a Database, _: &'a str, _: Vec<String>) -> RuntimeFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// What the three managers share: the registry they save to, and the admin screens they
/// register again after a save.
#[derive(Clone)]
pub struct Managers {
    /// The custom post types, taxonomies and field groups.
    pub registry: Arc<Registry>,
    /// The post types' screens, registered again after each save.
    pub resources: Arc<PostTypeResources>,
    /// The stored taxonomies' term screens, registered again after each save.
    pub term_screens: Arc<TaxonomyScreens>,
    /// Who grants a new post type's capabilities.
    pub grants: Arc<dyn CapabilityGrants>,
}

impl std::fmt::Debug for Managers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Managers").finish_non_exhaustive()
    }
}

impl Managers {
    /// Registers the post types' screens and the taxonomies' term screens again, so the next
    /// request serves the definitions in use. Logs what Ignite refuses.
    pub fn sync(&self) {
        for refused in self.resources.sync() {
            log::warn!("{refused}");
        }
        for refused in self.term_screens.sync() {
            log::warn!("A taxonomy has no term screen: {refused}");
        }
    }

    /// Registers the three managers with Ignite. Returns the slugs registered, and Ignite's
    /// refusal of each other one.
    pub fn register(&self) -> (Vec<&'static str>, Vec<RuntimeError>) {
        let mut registered = Vec::new();
        let mut refused = Vec::new();
        for (slug, resource) in [
            (POST_TYPES, post_types_resource(self.clone())),
            (TAXONOMIES, taxonomies_resource(self.clone())),
            (FIELD_GROUPS, field_groups_resource(self.clone())),
        ] {
            match ignite_core::register_runtime(resource) {
                Ok(()) => registered.push(slug),
                Err(e) => refused.push(e),
            }
        }
        (registered, refused)
    }
}

// ---------------------------------------------------------------------------------------------
// What the three share

/// The row key of a record's key, which addresses it in the URLs.
pub(crate) const KEY: &str = "key";
/// The row key that marks a locked record: no Delete, a read-only form.
pub(crate) const LOCKED: &str = "locked";
/// The row key of the notice a locked record's form shows.
pub(crate) const LOCKED_NOTICE: &str = "locked_notice";
/// The column of the record's origin, a badge.
pub(crate) const COL_ORIGIN: &str = "col_origin";

/// The badge text of `origin`: empty for the admin's.
pub(crate) fn origin_label(origin: &Origin) -> String {
    match origin {
        Origin::Admin => String::new(),
        Origin::Code => "Defined in code".to_owned(),
        Origin::Plugin(name) => format!("Plugin: {name}"),
    }
}

/// The badge text of WordPress's own definitions.
pub(crate) const BUILT_IN: &str = "Built in";

/// Why the record of a `noun` ("post type") made by `origin` cannot be changed here, as ACF
/// says it of a local definition; `None` for the admin's.
pub(crate) fn locked_notice(noun: &str, origin: &Origin) -> Option<String> {
    match origin {
        Origin::Admin => None,
        Origin::Code => Some(format!(
            "This {noun} is defined in code, so it cannot be changed or deleted here. Change it \
             in the code that registers it."
        )),
        Origin::Plugin(name) => Some(format!(
            "This {noun} is defined by the plugin \u{201c}{name}\u{201d}, so it cannot be \
             changed or deleted here."
        )),
    }
}

/// The notice of WordPress's own definitions.
pub(crate) fn built_in_notice(noun: &str) -> String {
    format!("This {noun} is built into WordPress, so it cannot be changed or deleted here.")
}

/// Whether the record being edited is locked, for `read_only_when`.
pub(crate) fn is_locked_record(form: &ignite_core::FormContext<'_>) -> bool {
    form.record().and_then(|r| r.get(LOCKED)) == Some(&Value::Bool(true))
}

/// The locked record's notice, for `read_only_notice`.
pub(crate) fn locked_record_notice(form: &ignite_core::FormContext<'_>) -> Option<String> {
    form.record()
        .and_then(|r| r.get(LOCKED_NOTICE))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A text value of the form's row: `None` when the key is missing, `Some("")` when it is null.
pub(crate) fn text(input: &ignite_core::Row, key: &str) -> Option<String> {
    match input.get(key)? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Null => Some(String::new()),
        other => Some(other.to_string()),
    }
}

/// A checkbox's value of the form's row: `None` when the form did not send it.
pub(crate) fn flag(input: &ignite_core::Row, key: &str) -> Option<bool> {
    match input.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => Some(matches!(s.trim(), "1" | "on" | "true")),
        _ => Some(false),
    }
}

/// A list of the form's row (a checkbox list or a repeater): its JSON items, `None` when the
/// form did not send it, empty when it was cleared.
pub(crate) fn list(input: &ignite_core::Row, key: &str) -> Option<Vec<Value>> {
    let value = input.get(key)?;
    let value = match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    Some(match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    })
}

/// The strings of a checkbox list's items.
pub(crate) fn strings(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

/// A whole number of the form's row: `None` when blank or missing.
pub(crate) fn number(input: &ignite_core::Row, key: &str) -> Option<i64> {
    match input.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The error a save is refused with when the database or the registry fails for a reason the
/// form cannot show.
pub(crate) fn registry_error(e: RegistryError) -> Error {
    match e {
        RegistryError::Database(e) => e,
        other => super::settings::refused_save(&other.to_string()),
    }
}

/// The message a refusal of the definition shows on the form.
pub(crate) fn definition_message(e: &DefinitionError) -> String {
    let text = e.to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut message: String = first.to_uppercase().chain(chars).collect();
            if !message.ends_with('.') {
                message.push('.');
            }
            message
        }
        None => text,
    }
}

/// A new key's random part: 13 hexadecimal digits, as ACF's `uniqid()` gives `group_` and
/// `field_` keys.
pub(crate) fn unique_id() -> String {
    let mut bytes = [0_u8; 7];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..7]);
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex[..13].to_owned()
}

/// Sorts `rows` by the text of `column`, and keeps those whose `searchable` columns contain
/// `search` (without case), then cuts the page out: the managers' lists are short and built in
/// memory, as ACF lists its definitions.
pub(crate) fn page_of(
    mut rows: Vec<ignite_core::Row>,
    query: &ignite_core::ListQuery,
    searchable: &[&str],
    default_sort: &str,
) -> ignite_core::Page<ignite_core::Row> {
    let cell = |row: &ignite_core::Row, key: &str| match row.get(key) {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(Value::Number(n)) => format!("{:020}", n.as_i64().unwrap_or_default()),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let search = search.to_lowercase();
        rows.retain(|row| {
            searchable
                .iter()
                .any(|key| cell(row, key).contains(&search))
        });
    }
    rows.retain(|row| query.filter.matches(row));
    let (key, descending) = match &query.sort {
        Some((key, direction)) => (key.as_str(), *direction == ignite_core::Direction::Desc),
        None => (default_sort, false),
    };
    rows.sort_by_cached_key(|row| cell(row, key));
    if descending {
        rows.reverse();
    }
    let total = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let per_page = query.per_page.clamp(0, 100);
    if per_page == 0 {
        return ignite_core::Page {
            rows: Vec::new(),
            total,
        };
    }
    let offset = usize::try_from(query.page.max(1).saturating_sub(1).saturating_mul(per_page))
        .unwrap_or(usize::MAX);
    let rows = rows
        .into_iter()
        .skip(offset)
        .take(usize::try_from(per_page).unwrap_or(0))
        .collect();
    ignite_core::Page { rows, total }
}

/// The error of a record that is not there (a key that names nothing).
pub(crate) fn not_found() -> Error {
    Error::NotFound { table: "records" }
}
