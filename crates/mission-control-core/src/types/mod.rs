//! Custom post types, taxonomies and field groups.
//!
//! [`definition`] holds the one definition format the admin's manager screens, Rust code and
//! plugins all produce (design 5.3). The registry that loads and serves them is Task 5.2.

pub mod definition;

pub use definition::{
    CapabilityType, DefinitionError, FieldDef, FieldGroupDef, FieldKindDef, HasArchive,
    HideOnScreen, LayoutDef, LocationOperator, LocationParam, LocationRule, MAX_CHOICES,
    MAX_FIELD_DEPTH, MAX_FIELDS, MAX_ICON_LEN, MAX_JSON_BYTES, MAX_KEY_LEN, MAX_LAYOUTS,
    MAX_LIST_ITEMS, MAX_LOCATION_RULES, MAX_TEXT_LEN, Origin, Position, PostTypeCapabilities,
    PostTypeDef, PostTypeLabels, Rewrite, RewriteRules, ShowInMenu, Support, TaxonomyCapabilities,
    TaxonomyDef, TaxonomyLabels,
};
