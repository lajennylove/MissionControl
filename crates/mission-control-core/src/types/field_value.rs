//! Reading a field's value in code, typed: ACF's `get_field` (design 5.3).
//!
//! [`get_field`] does what ACF's `get_field` does through `acf_get_field` and
//! `acf_get_value`, and decodes the value into the type the caller asks for, so a theme writes
//! `get_field::<String>(db, &registry, &post, "venue")` where ACF writes
//! `get_field('venue', $post)`:
//!
//! 1. It reads the post meta stored under the name (its first value, if the key repeats).
//! 2. When the post has no value, it resolves the field by name across **all** the field
//!    groups in use ([`Registry::all_field_groups`]), as `acf_get_field($name)` does: no
//!    location check, so the group need not apply to the post, and an inactive group's field
//!    counts too. The first top-level field with the name wins, the groups taken in ACF's
//!    order of groups: `menu_order`, then title, then key (the key makes the order stable).
//! 3. When that field has a `default_value`, it gives the default, as `acf_get_value` does,
//!    first made the type the field's kind stores ([`FieldDef::typed_default`]): a
//!    `true_false` default of `0` or `"1"` is a boolean, a number's `"100"` is a number, a
//!    select's `false` is none. A stored value, `null` included, wins over the default.
//! 4. When no group defines the name, or its field has no default, it gives `None`.
//!
//! A value that does not decode into `T` is a [`FieldError::Type`], the default included.
//!
//! The registry is an argument the plan's sketch (`get_field::<T>(post, "name")`) does not
//! have: ACF finds the groups through its global store, which Mission Control passes
//! explicitly (Ruling R93).
//!
//! Not done, as ACF does: the field reference meta (`_venue` holding the field's key), which
//! ACF checks before searching by name; and formatting by the field's settings (`format_value`:
//! an image's `return_format`, a relationship's posts). The value is decoded as stored. An
//! empty-string `default_value`, which ACF writes for a field without one, counts as none
//! (Ruling R96), where ACF would return `""`.

use std::any::type_name;
use std::fmt;

use orbit_orm::{Database, Error};
use serde::de::DeserializeOwned;

use super::definition::FieldDef;
use super::registry::Registry;
use crate::content::{MetaError, Post, get_post_meta};

/// Why a field's value was not read.
#[derive(Debug)]
#[non_exhaustive]
pub enum FieldError {
    /// The value is there, but is not the type asked for.
    Type {
        /// The field's name.
        field: String,
        /// The type asked for, as Rust names it (`i64`, `alloc::string::String`, ...).
        expected: &'static str,
        /// Why the value does not decode.
        source: serde_json::Error,
    },
    /// The stored text is not JSON, so the value was not written by Mission Control.
    InvalidJson(serde_json::Error),
    /// The database refused the query.
    Database(Error),
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type {
                field,
                expected,
                source,
            } => write!(f, "the field `{field}` is not a `{expected}`: {source}"),
            Self::InvalidJson(e) => write!(f, "the field's stored value is not valid JSON: {e}"),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for FieldError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Type { source, .. } | Self::InvalidJson(source) => Some(source),
            Self::Database(e) => Some(e),
        }
    }
}

impl From<Error> for FieldError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

impl From<MetaError> for FieldError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::InvalidJson(e) => Self::InvalidJson(e),
            MetaError::Database(e) => Self::Database(e),
        }
    }
}

/// The post whose field is read: its id, or the post. ACF's `$post_id` argument, for posts.
pub trait FieldOwner {
    /// The post's id.
    fn post_id(&self) -> i64;
}

impl FieldOwner for i64 {
    fn post_id(&self) -> i64 {
        *self
    }
}

impl FieldOwner for Post {
    fn post_id(&self) -> i64 {
        self.id
    }
}

impl<T: FieldOwner + ?Sized> FieldOwner for &T {
    fn post_id(&self) -> i64 {
        (**self).post_id()
    }
}

/// The value of the field `name` of `post`, decoded into `T`, as ACF's `get_field`: the stored
/// value, or the default of the field the groups define under that name, or `None`
/// (ACF's `null`). See the [module](self) for how the field is resolved.
///
/// ```ignore
/// use mission_control_core::types::get_field;
///
/// let venue: Option<String> = get_field::<String>(&db, &registry, &post, "venue").await?;
/// let seats = get_field::<i64>(&db, &registry, post.id, "seats").await?.unwrap_or(0);
/// ```
///
/// # Errors
///
/// [`FieldError::Type`] when the value (or the default) is not a `T`, naming the field and the
/// type; [`FieldError::InvalidJson`] when the stored text is not JSON; the database's error.
pub async fn get_field<T: DeserializeOwned>(
    db: &Database,
    registry: &Registry,
    post: impl FieldOwner,
    name: &str,
) -> Result<Option<T>, FieldError> {
    let value = match get_post_meta(db, post.post_id(), name).await? {
        Some(value) => value,
        None => match resolve_field(registry, name).and_then(|f| f.typed_default()) {
            Some(default) => default,
            None => return Ok(None),
        },
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|source| FieldError::Type {
            field: name.to_owned(),
            expected: type_name::<T>(),
            source,
        })
}

/// The top-level field named `name` of the first group, in ACF's order of groups
/// (`menu_order`, title, key), that has one: any group in use, as `acf_get_field($name)`
/// searches by name.
fn resolve_field(registry: &Registry, name: &str) -> Option<FieldDef> {
    let mut groups = registry.all_field_groups();
    groups.sort_by(|a, b| (a.menu_order, &a.title, &a.key).cmp(&(b.menu_order, &b.title, &b.key)));
    groups
        .into_iter()
        .flat_map(|group| group.fields)
        .find(|field| field.name == name)
}
