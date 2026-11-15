//! Custom post types, taxonomies and field groups.
//!
//! [`definition`] holds the one definition format the admin's manager screens, Rust code and
//! plugins all produce (design 5.3). [`registry`] holds the definitions in use: those made in
//! code and by plugins in memory, the admin's in the database.

pub mod definition;
pub mod registry;

pub use definition::{
    CapabilityType, DefinitionError, FieldDef, FieldGroupDef, FieldKindDef, HasArchive,
    HideOnScreen, LayoutDef, LocationOperator, LocationParam, LocationRule, MAX_CHOICES,
    MAX_FIELD_DEPTH, MAX_FIELDS, MAX_ICON_LEN, MAX_JSON_BYTES, MAX_KEY_LEN, MAX_LAYOUTS,
    MAX_LIST_ITEMS, MAX_LOCATION_RULES, MAX_TEXT_LEN, Origin, Position, PostTypeCapabilities,
    PostTypeDef, PostTypeLabels, Rewrite, RewriteRules, ShowInMenu, Support, TaxonomyCapabilities,
    TaxonomyDef, TaxonomyLabels,
};
pub use registry::{Definition, DefinitionKind, Registry, RegistryError};
