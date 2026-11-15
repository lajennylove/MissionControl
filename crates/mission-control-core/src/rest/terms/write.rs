//! Writing terms: `create_item`, `update_item` and `delete_item` of `WP_REST_Terms_Controller`,
//! with their permission checks and `prepare_item_for_database`.

use serde_json::{Map, Value, json};

use super::prepare::{self, ItemOptions};
use super::{TaxonomySpec, TermsController};
use crate::content::{
    FieldTooLarge, MAX_FILTERED_BYTES, MAX_TERM_LENGTH, NewTerm, Term, TermChanges, TermError,
    kses_data, sanitize_text_field,
};
use crate::rest::params::{Params, Reader, require};
use crate::rest::shared::{Context, Fields, Site, database_error, too_large};
use crate::rest::{RestError, RestResponse};

/// The fields a request writes, read and checked from its parameters. `None` leaves a field as
/// it is.
#[derive(Debug, Default)]
struct Changes {
    name: Option<String>,
    description: Option<String>,
    slug: Option<String>,
    /// The `parent` parameter, when given: for a flat taxonomy any value, which is refused.
    parent: Option<i64>,
    parent_given: bool,
}

impl Changes {
    /// Reads the writable fields from `params`, as the schema validates them.
    fn read(taxonomy: &TaxonomySpec, params: &Params) -> Result<Self, RestError> {
        let mut r = Reader::new(params);
        let mut changes = Self {
            name: r.string("name"),
            description: r.string("description"),
            slug: r.string("slug"),
            parent_given: r.has("parent"),
            ..Self::default()
        };
        // A flat taxonomy has no `parent` argument to validate; giving one is refused later.
        if taxonomy.taxonomy.hierarchical {
            changes.parent = r.integer("parent", None, None);
        }
        if let Some(meta) = r.raw("meta")
            && !meta.is_object()
            && !meta.as_array().is_some_and(Vec::is_empty)
        {
            r.invalid.wrong_type("meta", "object");
        }
        r.invalid.finish()?;
        Ok(changes)
    }

    /// Whether the request changes anything.
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.slug.is_none()
            && !self.parent_given
    }
}

/// The parent to store, from a given `parent`: `None` when none was given, `Some(None)` for 0
/// (top-level). As `create_item` and `update_item`, a flat taxonomy refuses any parent with
/// `rest_taxonomy_not_hierarchical`, and a parent that is not a term of the taxonomy is
/// `rest_term_invalid` (400).
async fn checked_parent(
    ctl: &TermsController<'_>,
    changes: &Changes,
) -> Result<Option<Option<i64>>, RestError> {
    if !changes.parent_given {
        return Ok(None);
    }
    if !ctl.taxonomy.taxonomy.hierarchical {
        return Err(RestError::new(
            "rest_taxonomy_not_hierarchical",
            "Cannot set parent term, taxonomy is not hierarchical.",
            400,
        ));
    }
    match changes.parent {
        None | Some(0) => Ok(Some(None)),
        Some(parent) => match ctl.find_in(parent).await? {
            Some(term) => Ok(Some(Some(term.term_id))),
            None => Err(RestError::new(
                "rest_term_invalid",
                "Parent term does not exist.",
                400,
            )),
        },
    }
}

/// The name and description as they are stored, as `sanitize_term` filters them in its `db`
/// context for every user: the name through `sanitize_text_field`, the description through
/// `wp_filter_kses`. Run on a blocking thread behind the kses limiter.
async fn sanitize(
    ctl: &TermsController<'_>,
    name: Option<String>,
    description: Option<String>,
) -> Result<(Option<String>, Option<String>), RestError> {
    for (field, value) in [("name", &name), ("description", &description)] {
        if let Some(value) = value
            && value.len() > MAX_FILTERED_BYTES
        {
            return Err(too_large(&FieldTooLarge {
                field,
                size: value.len(),
            }));
        }
    }
    let (name, description) = ctl
        .kses
        .run(move || {
            (
                name.map(|n| sanitize_text_field(&n)),
                description.map(|d| kses_data(&d)),
            )
        })
        .await?;
    Ok((name, description))
}

/// WordPress's error for a [`TermError`] from `wp_insert_term` or `wp_update_term`, as 400 (see
/// the module's notes on statuses). `term_exists` names the existing term's id.
async fn term_error(ctl: &TermsController<'_>, error: TermError) -> RestError {
    match error {
        TermError::EmptyName => {
            RestError::new("empty_term_name", "A name is required for this term.", 400)
        }
        TermError::NameTooLong => RestError::new(
            "invalid_term_name",
            format!("Invalid term name: a term name must be at most {MAX_TERM_LENGTH} characters."),
            400,
        ),
        TermError::SlugTooLong => RestError::new(
            "invalid_term_slug",
            format!("Invalid term slug: a term slug must be at most {MAX_TERM_LENGTH} characters."),
            400,
        ),
        TermError::NotHierarchical { .. } => RestError::new(
            "rest_taxonomy_not_hierarchical",
            "Cannot set parent term, taxonomy is not hierarchical.",
            400,
        ),
        TermError::MissingParent => {
            RestError::new("rest_term_invalid", "Parent term does not exist.", 400)
        }
        TermError::DuplicateSlug { slug } => RestError::new(
            "duplicate_term_slug",
            format!("The slug &#8220;{slug}&#8221; is already in use by another term."),
            400,
        ),
        TermError::Exists { term_taxonomy_id } => {
            let message = if ctl.taxonomy.taxonomy.hierarchical {
                "A term with the name provided already exists with this parent."
            } else {
                "A term with the name provided already exists in this taxonomy."
            };
            let mut error = RestError::new("term_exists", message, 400);
            match Term::find(ctl.db, term_taxonomy_id).await {
                Ok(Some(term)) => error = error.with_data("term_id", json!(term.term_id)),
                Ok(None) => {}
                Err(e) => return database_error(&e),
            }
            error
        }
        TermError::DefaultTerm => {
            RestError::new("rest_cannot_delete", "The term cannot be deleted.", 500)
        }
        TermError::Database(e) => database_error(&e),
    }
}

/// Term `id`, read again, as `context` shows it.
async fn respond(
    ctl: &TermsController<'_>,
    site: &Site,
    id: i64,
    status: u16,
    context: Context,
) -> Result<RestResponse, RestError> {
    let term = ctl.find(id).await?;
    let options = ItemOptions {
        context,
        fields: Fields(None),
        embed: None,
    };
    let mut items = prepare::items(ctl, site, std::slice::from_ref(&term), &options).await?;
    Ok(RestResponse {
        status,
        body: items.pop().unwrap_or(Value::Null),
        headers: Vec::new(),
    })
}

/// `create_item`.
pub(super) async fn create(
    ctl: &TermsController<'_>,
    params: &Params,
) -> Result<RestResponse, RestError> {
    require(params, &["name"])?;
    let changes = Changes::read(ctl.taxonomy, params)?;
    // `create_item_permissions_check`: a hierarchical taxonomy's `edit_terms`, a flat one's
    // `assign_terms`, so whoever may tag a post may add a tag.
    let caps = ctl.taxonomy.caps();
    let cap = if ctl.taxonomy.taxonomy.hierarchical {
        &caps.edit_terms
    } else {
        &caps.assign_terms
    };
    if !ctl.has(cap) {
        return Err(ctl.forbidden(
            "rest_cannot_create",
            "Sorry, you are not allowed to create terms in this taxonomy.",
        ));
    }
    let parent = checked_parent(ctl, &changes).await?.flatten();
    let raw_name = changes.name.clone().unwrap_or_default();
    if raw_name.trim().is_empty() {
        return Err(term_error(ctl, TermError::EmptyName).await);
    }
    let (name, description) = sanitize(ctl, Some(raw_name), changes.description.clone()).await?;
    let name = name.unwrap_or_default();
    if name.is_empty() {
        return Err(RestError::new(
            "invalid_term_name",
            "Invalid term name.",
            400,
        ));
    }
    let new = NewTerm {
        name,
        slug: changes.slug.clone().unwrap_or_default(),
        description: description.unwrap_or_default(),
        parent_id: parent,
    };
    let term = match Term::create(ctl.db, &ctl.taxonomy.taxonomy, new).await {
        Ok(term) => term,
        Err(e) => return Err(term_error(ctl, e).await),
    };
    let site = Site::load(ctl.db).await?;
    let response = respond(ctl, &site, term.term_id, 201, Context::Edit).await?;
    let location = site.rest_url(&format!(
        "wp/v2/{}/{}",
        ctl.taxonomy.rest_base, term.term_id
    ));
    Ok(response.with_header("Location", location))
}

/// `update_item`.
pub(super) async fn update(
    ctl: &TermsController<'_>,
    id: i64,
    params: &Params,
) -> Result<RestResponse, RestError> {
    let changes = Changes::read(ctl.taxonomy, params)?;
    let term = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    let defaults = ctl.default_terms().await?;
    if !ctl.can_on("edit_term", &term, &defaults) {
        return Err(ctl.forbidden(
            "rest_cannot_update",
            "Sorry, you are not allowed to edit this term.",
        ));
    }
    let parent = checked_parent(ctl, &changes).await?;
    // As `update_item`: only a request with something to change updates the term.
    if !changes.is_empty() {
        let (name, description) =
            sanitize(ctl, changes.name.clone(), changes.description.clone()).await?;
        let update = TermChanges {
            name,
            slug: changes.slug.clone(),
            description,
            parent_id: parent,
        };
        if let Err(e) = term.update(ctl.db, &ctl.taxonomy.taxonomy, update).await {
            return Err(term_error(ctl, e).await);
        }
    }
    respond(ctl, &site, term.term_id, 200, Context::Edit).await
}

/// `delete_item`.
pub(super) async fn delete(
    ctl: &TermsController<'_>,
    id: i64,
    params: &Params,
) -> Result<RestResponse, RestError> {
    let mut r = Reader::new(params);
    let force = r.boolean("force").unwrap_or(false);
    r.invalid.finish()?;
    let term = ctl.find(id).await?;
    let site = Site::load(ctl.db).await?;
    let defaults = ctl.default_terms().await?;
    if !ctl.can_on("delete_term", &term, &defaults) {
        return Err(ctl.forbidden(
            "rest_cannot_delete",
            "Sorry, you are not allowed to delete this term.",
        ));
    }
    if !force {
        return Err(RestError::new(
            "rest_trash_not_supported",
            "Terms do not support trashing. Set 'force=true' to delete.",
            501,
        ));
    }
    let previous = respond(ctl, &site, term.term_id, 200, Context::View)
        .await?
        .body;
    let deleted = match term.delete(ctl.db).await {
        Ok(deleted) => deleted,
        // The default term, made so since the permission check.
        Err(TermError::DefaultTerm) => false,
        Err(e) => return Err(database_error(&e)),
    };
    if !deleted {
        return Err(RestError::new(
            "rest_cannot_delete",
            "The term cannot be deleted.",
            500,
        ));
    }
    Ok(RestResponse::ok(
        json!({"deleted": true, "previous": previous}),
    ))
}

/// The arguments of a create (`creating`) or update, as WordPress's discovery lists the item
/// schema's writable fields: the name is required to create.
pub(super) fn schema_args(taxonomy: &TaxonomySpec, creating: bool) -> Map<String, Value> {
    let mut args = Map::new();
    let mut put = |name: &str, value: Value| {
        args.insert(name.to_owned(), value);
    };
    put(
        "description",
        json!({"description": "HTML description of the term.", "type": "string", "required": false}),
    );
    put(
        "name",
        json!({"description": "HTML title for the term.", "type": "string", "required": creating}),
    );
    put(
        "slug",
        json!({"description": "An alphanumeric identifier for the term unique to its type.", "type": "string", "required": false}),
    );
    if taxonomy.taxonomy.hierarchical {
        put(
            "parent",
            json!({"description": "The parent term ID.", "type": "integer", "required": false}),
        );
    }
    put(
        "meta",
        json!({"description": "Meta fields.", "type": "object", "properties": [], "required": false}),
    );
    args
}
