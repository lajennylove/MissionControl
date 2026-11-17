//! Custom post types, taxonomies and field groups.
//!
//! [`definition`] holds the one definition format the admin's manager screens, Rust code and
//! plugins all produce (design 5.3). [`registry`] holds the definitions in use: those made in
//! code and by plugins in memory, the admin's in the database. [`code`] builds definitions in
//! Rust, as Poet and ACF Composer do, and [`get_field`] reads a field's value, typed.
//! [`resource`] makes each post type's admin screens: an Ignite runtime resource.

pub mod code;
pub mod definition;
pub mod field_value;
pub mod location;
pub mod registry;
pub mod resource;

pub use definition::{
    CapabilityType, DefinitionError, FieldDef, FieldGroupDef, FieldKindDef, HasArchive,
    HideOnScreen, LayoutDef, LocationOperator, LocationParam, LocationRule, MAX_CHOICES,
    MAX_FIELD_DEPTH, MAX_FIELDS, MAX_ICON_LEN, MAX_JSON_BYTES, MAX_KEY_LEN, MAX_LAYOUTS,
    MAX_LIST_ITEMS, MAX_LOCATION_RULES, MAX_TEXT_LEN, Origin, Position, PostTypeCapabilities,
    PostTypeDef, PostTypeLabels, Rewrite, RewriteRules, ShowInMenu, Support, TaxonomyCapabilities,
    TaxonomyDef, TaxonomyLabels,
};
pub use field_value::{FieldError, FieldOwner, get_field};
pub use location::{Rule, ScreenContext, matches};
pub use registry::{Definition, DefinitionKind, Registry, RegistryError};
pub use resource::{
    Access, MAX_OPTIONS, MAX_PER_PAGE, PostTypeData, PostTypeResources, RefusedResource,
    builtin_post_types, create_auto_draft, custom_field_name, post_type_resource,
    post_type_resource_with_access, post_type_resource_with_templates,
};
