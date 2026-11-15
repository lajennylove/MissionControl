//! Taxonomies and their terms, after WordPress's `wp_terms`, `wp_term_taxonomy` and
//! `wp_term_relationships`: a [`Term`] is a name and slug in `terms` plus its place in one
//! taxonomy in `term_taxonomy`, and a post's terms are rows of `term_relationships`.
//!
//! A parent is a term id (`term_id`), as in WordPress; post relationships refer to the
//! `term_taxonomy_id`, as `wp_term_relationships` does.

use std::collections::{HashMap, HashSet};
use std::fmt;

use orbit_orm::{Database, Error, Executor, Model, OrbitModel, Transaction, Value, raw};

use super::post::{Post, PostStatus};
use super::slug::{make_slug, sanitize_title};

/// A taxonomy: its name and whether its terms nest. A minimal stand-in for the taxonomy
/// definitions that custom taxonomies bring later; for now the built-ins are
/// [`CATEGORY`](Self::CATEGORY), [`POST_TAG`](Self::POST_TAG) and [`NAV_MENU`](Self::NAV_MENU).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taxonomy {
    /// The name stored in `term_taxonomy.taxonomy`, at most 32 characters.
    pub name: &'static str,
    /// Whether a term may have a parent, as categories do and tags do not.
    pub hierarchical: bool,
    /// The post types whose published posts a term's `count` counts.
    pub object_types: &'static [&'static str],
}

impl Taxonomy {
    /// Categories: hierarchical, for posts.
    pub const CATEGORY: Self = Self {
        name: "category",
        hierarchical: true,
        object_types: &["post"],
    };

    /// Tags: flat, for posts.
    pub const POST_TAG: Self = Self {
        name: "post_tag",
        hierarchical: false,
        object_types: &["post"],
    };

    /// Navigation menus: each menu is a term, flat, and its items (`nav_menu_item` posts) are
    /// its objects, as WordPress registers `nav_menu`. Not public: it has no archive.
    pub const NAV_MENU: Self = Self {
        name: "nav_menu",
        hierarchical: false,
        object_types: &["nav_menu_item"],
    };

    /// The built-in taxonomy named `name`, if there is one.
    pub fn builtin(name: &str) -> Option<Self> {
        [Self::CATEGORY, Self::POST_TAG, Self::NAV_MENU]
            .into_iter()
            .find(|t| t.name == name)
    }
}

/// A term in one taxonomy, as WordPress's `WP_Term`: its `terms` row and its `term_taxonomy` row
/// together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The `terms` id (`term_id`).
    pub term_id: i64,
    /// The `term_taxonomy` id, which post relationships refer to.
    pub term_taxonomy_id: i64,
    /// The name.
    pub name: String,
    /// The slug, unique within the taxonomy.
    pub slug: String,
    /// WordPress's `term_group`, for term aliases; 0.
    pub term_group: i64,
    /// The taxonomy's name, such as `category`.
    pub taxonomy: String,
    /// The description.
    pub description: String,
    /// The parent's `term_id`, or `None` for a top-level term.
    pub parent_id: Option<i64>,
    /// How many published posts of the taxonomy's post types have this term. Kept in step, as
    /// WordPress does, when a post's terms change, when [`Post::save`] or [`Post::trash`] writes
    /// a post, and when a scheduled post is published; see [`update_term_counts`] for other
    /// changes and for deleting a post.
    pub count: i64,
}

/// A term to create. Start from [`NewTerm::new`] and set the rest with struct update syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTerm {
    /// The name; surrounding whitespace is trimmed.
    pub name: String,
    /// The slug. Empty means one is made from the name.
    pub slug: String,
    /// The description.
    pub description: String,
    /// The parent's `term_id`, for a hierarchical taxonomy.
    pub parent_id: Option<i64>,
}

impl NewTerm {
    /// A top-level term named `name`, with no slug yet and no description.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            slug: String::new(),
            description: String::new(),
            parent_id: None,
        }
    }
}

/// The most characters a term's name or slug may have: the size of the `terms.name` and
/// `terms.slug` columns, as in WordPress.
pub const MAX_TERM_LENGTH: usize = 200;

/// Why a term was not created. The messages follow WordPress's `wp_insert_term` errors.
#[derive(Debug)]
pub enum TermError {
    /// The name is empty or only whitespace (`empty_term_name`).
    EmptyName,
    /// The name is longer than [`MAX_TERM_LENGTH`] characters, the size of the `name` column, as
    /// WordPress refuses it (`invalid_term_name`).
    NameTooLong,
    /// The slug, as given or once made unique, is longer than [`MAX_TERM_LENGTH`] characters, the size
    /// of the `slug` column. Refused so that every database behaves alike; MySQL and PostgreSQL
    /// would reject it and SQLite would store it.
    SlugTooLong,
    /// A parent was given for a flat taxonomy, such as tags.
    NotHierarchical {
        /// The taxonomy's name.
        taxonomy: &'static str,
    },
    /// The parent is not a term of this taxonomy (`missing_parent`).
    MissingParent,
    /// A term with this name already exists with this parent, or in this flat taxonomy
    /// (`term_exists`).
    Exists {
        /// The existing term.
        term_taxonomy_id: i64,
    },
    /// The database refused a query.
    Database(Error),
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("a term name must not be empty"),
            Self::NameTooLong => write!(
                f,
                "a term name must be at most {MAX_TERM_LENGTH} characters"
            ),
            Self::SlugTooLong => write!(
                f,
                "a term slug must be at most {MAX_TERM_LENGTH} characters"
            ),
            Self::NotHierarchical { taxonomy } => write!(
                f,
                "the {taxonomy} taxonomy is not hierarchical, so a term cannot have a parent"
            ),
            Self::MissingParent => f.write_str("the parent term does not exist in this taxonomy"),
            Self::Exists { term_taxonomy_id } => write!(
                f,
                "a term with this name already exists here (term {term_taxonomy_id})"
            ),
            Self::Database(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TermError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<Error> for TermError {
    fn from(e: Error) -> Self {
        Self::Database(e)
    }
}

/// A row of `terms`.
#[derive(OrbitModel)]
#[orbit(table = "terms")]
struct TermRow {
    id: i64,
    #[orbit(fillable)]
    name: String,
    #[orbit(fillable)]
    slug: String,
    #[orbit(fillable)]
    term_group: i64,
}

/// A row of `term_taxonomy`.
#[derive(OrbitModel)]
#[orbit(table = "term_taxonomy")]
struct TermTaxonomyRow {
    id: i64,
    #[orbit(fillable)]
    term_id: i64,
    #[orbit(fillable)]
    taxonomy: String,
    #[orbit(fillable)]
    description: String,
    #[orbit(fillable)]
    parent_id: Option<i64>,
    #[orbit(fillable)]
    count: i64,
}

/// A connection that queries can run on more than once: the pool, or an open transaction. Lets
/// one helper serve both reads on the pool and the steps of a write.
pub(crate) trait Conn: Send {
    /// The executor for the next query.
    fn exec(&mut self) -> impl Executor<'_> + Send;
}

impl Conn for &Database {
    fn exec(&mut self) -> impl Executor<'_> + Send {
        &**self
    }
}

impl Conn for Transaction {
    fn exec(&mut self) -> impl Executor<'_> + Send {
        self
    }
}

impl Term {
    /// Creates a term in `taxonomy`, as WordPress's `wp_insert_term` does, and returns it.
    ///
    /// The name is trimmed and must not be empty; it and the slug may have at most
    /// [`MAX_TERM_LENGTH`] characters. A given slug is sanitized with
    /// [`make_slug`](super::make_slug)'s rules (`Hello World` is stored as `hello-world`); an
    /// empty one, or one that sanitizes to nothing, is made from the name. A parent must be a
    /// term of the same taxonomy, and only a hierarchical taxonomy takes one.
    ///
    /// A name that already exists is refused, compared without regard to case: under the same
    /// parent in a hierarchical taxonomy, anywhere in a flat one. A distinct slug of the caller's
    /// own lets the name repeat, as in WordPress. A taken slug is made unique, as
    /// `wp_unique_term_slug` does: a child first takes its ancestors' slugs as suffixes
    /// (`apple-tech`), then a number is added (`rust-2`). Slugs are unique per taxonomy.
    ///
    /// # Errors
    ///
    /// Fails with [`TermError`] for the cases above, or with the database's error.
    pub async fn create(
        db: &Database,
        taxonomy: &Taxonomy,
        new: NewTerm,
    ) -> Result<Self, TermError> {
        let name = new.name.trim().to_owned();
        if name.is_empty() {
            return Err(TermError::EmptyName);
        }
        if name.chars().count() > MAX_TERM_LENGTH {
            return Err(TermError::NameTooLong);
        }
        if new.parent_id.is_some() && !taxonomy.hierarchical {
            return Err(TermError::NotHierarchical {
                taxonomy: taxonomy.name,
            });
        }
        let taxonomy = *taxonomy;
        db.transaction(async |tx| {
            if let Some(parent) = new.parent_id {
                let found = TermTaxonomyRow::query()
                    .where_eq("term_id", parent)
                    .where_eq("taxonomy", taxonomy.name)
                    .exists(tx.exec())
                    .await?;
                if !found {
                    return Err(TermError::MissingParent);
                }
            }
            // A given slug is sanitized too, as WordPress's `pre_term_slug` filter runs
            // `sanitize_title` on it, and counts as given only when something is left. It is not
            // cut to the column: one that is too long is refused below.
            let given = sanitize_title(&new.slug);
            let slug_given = !given.is_empty();
            let slug = if slug_given { given } else { make_slug(&name) };
            if let Some(term_taxonomy_id) =
                existing(tx, &taxonomy, &name, &slug, slug_given, new.parent_id).await?
            {
                return Err(TermError::Exists { term_taxonomy_id });
            }
            let slug = unique_slug(tx, &taxonomy, slug, new.parent_id).await?;
            if slug.chars().count() > MAX_TERM_LENGTH {
                return Err(TermError::SlugTooLong);
            }

            let term_id = TermRow::query()
                .insert(
                    tx.exec(),
                    &[
                        ("name", name.clone().into()),
                        ("slug", slug.clone().into()),
                        ("term_group", 0_i64.into()),
                    ],
                )
                .await?;
            let term_id = i64::try_from(term_id).expect("an id from a BIGINT column fits in i64");
            let term_taxonomy_id = TermTaxonomyRow::query()
                .insert(
                    tx.exec(),
                    &[
                        ("term_id", term_id.into()),
                        ("taxonomy", taxonomy.name.into()),
                        ("description", new.description.clone().into()),
                        ("parent_id", new.parent_id.into()),
                        ("count", 0_i64.into()),
                    ],
                )
                .await?;
            Ok(Self {
                term_id,
                term_taxonomy_id: i64::try_from(term_taxonomy_id)
                    .expect("an id from a BIGINT column fits in i64"),
                name,
                slug,
                term_group: 0,
                taxonomy: taxonomy.name.to_owned(),
                description: new.description,
                parent_id: new.parent_id,
                count: 0,
            })
        })
        .await
    }

    /// The term with this `term_taxonomy_id`, if there is one.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn find(db: &Database, term_taxonomy_id: i64) -> Result<Option<Self>, Error> {
        find(&mut { db }, term_taxonomy_id).await
    }

    /// The term of `taxonomy` with this slug, if there is one.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn find_by_slug(
        db: &Database,
        taxonomy: &Taxonomy,
        slug: &str,
    ) -> Result<Option<Self>, Error> {
        find_by_slug(&mut { db }, taxonomy, slug).await
    }

    /// Every term of `taxonomy`, ordered by name.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn all(db: &Database, taxonomy: &Taxonomy) -> Result<Vec<Self>, Error> {
        let mut c = db;
        let rows = TermTaxonomyRow::query()
            .where_eq("taxonomy", taxonomy.name)
            .get(c.exec())
            .await?;
        Ok(by_name(with_terms(&mut c, rows).await?))
    }

    /// The terms whose parent is this term, ordered by name.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn children(&self, db: &Database) -> Result<Vec<Self>, Error> {
        let mut c = db;
        let rows = TermTaxonomyRow::query()
            .where_eq("parent_id", self.term_id)
            .where_eq("taxonomy", self.taxonomy.as_str())
            .get(c.exec())
            .await?;
        Ok(by_name(with_terms(&mut c, rows).await?))
    }

    /// Deletes this term, as WordPress's `wp_delete_term` does: its children move to its parent
    /// (or to the top level), its post relationships go, and its `terms` row goes when no other
    /// taxonomy uses it. Returns `false` when the term was already gone.
    ///
    /// WordPress also refuses to delete the default category and gives posts left without a
    /// category the default one; that arrives with the `default_category` option.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn delete(&self, db: &Database) -> Result<bool, Error> {
        db.transaction(async |tx| {
            let Some(row) = TermTaxonomyRow::query()
                .find(tx.exec(), self.term_taxonomy_id)
                .await?
            else {
                return Ok(false);
            };
            // Before the term goes: the foreign key on `parent_id` refuses to delete a term that
            // still has children.
            TermTaxonomyRow::query()
                .where_eq("parent_id", row.term_id)
                .where_eq("taxonomy", row.taxonomy.as_str())
                .update(tx.exec(), &[("parent_id", row.parent_id.into())])
                .await?;
            // The foreign key deletes the term's `term_relationships` rows.
            TermTaxonomyRow::query()
                .where_eq("id", row.id)
                .delete(tx.exec())
                .await?;
            let shared = TermTaxonomyRow::query()
                .where_eq("term_id", row.term_id)
                .exists(tx.exec())
                .await?;
            if !shared {
                TermRow::query()
                    .where_eq("id", row.term_id)
                    .delete(tx.exec())
                    .await?;
            }
            Ok(true)
        })
        .await
    }

    /// Joins a `terms` row and a `term_taxonomy` row.
    fn from_rows(term: &TermRow, row: TermTaxonomyRow) -> Self {
        Self {
            term_id: term.id,
            term_taxonomy_id: row.id,
            name: term.name.clone(),
            slug: term.slug.clone(),
            term_group: term.term_group,
            taxonomy: row.taxonomy,
            description: row.description,
            parent_id: row.parent_id,
            count: row.count,
        }
    }
}

impl Post {
    /// This post's terms in `taxonomy`, ordered by name, as WordPress's `wp_get_object_terms`.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn terms(&self, db: &Database, taxonomy: &Taxonomy) -> Result<Vec<Term>, Error> {
        let mut c = db;
        let ids = related(&mut c, self.id, taxonomy).await?;
        let rows = TermTaxonomyRow::query()
            .where_in("id", ids)
            .get(c.exec())
            .await?;
        Ok(by_name(with_terms(&mut c, rows).await?))
    }

    /// Makes `term_taxonomy_ids` this post's terms in `taxonomy`: adds the missing ones and
    /// removes the others, as Laravel's `sync` and WordPress's `wp_set_object_terms` do. Terms of
    /// other taxonomies stay. Ids that are not terms of `taxonomy` are skipped, as WordPress skips
    /// unknown term ids. The counts of the terms added or removed are updated.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn sync_terms(
        &self,
        db: &Database,
        taxonomy: &Taxonomy,
        term_taxonomy_ids: &[i64],
    ) -> Result<(), Error> {
        set_terms(db, self.id, taxonomy, term_taxonomy_ids, false).await
    }

    /// Adds `term_taxonomy_ids` to this post's terms in `taxonomy` and keeps the rest, as
    /// Laravel's `attach` and `wp_set_object_terms` with `$append` do. Terms the post has already
    /// and ids that are not terms of `taxonomy` are skipped. The counts of the terms added are
    /// updated.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn attach_terms(
        &self,
        db: &Database,
        taxonomy: &Taxonomy,
        term_taxonomy_ids: &[i64],
    ) -> Result<(), Error> {
        set_terms(db, self.id, taxonomy, term_taxonomy_ids, true).await
    }

    /// Removes `term_taxonomy_ids` from this post's terms in `taxonomy`, as Laravel's `detach`
    /// and WordPress's `wp_remove_object_terms` do, and returns how many were removed. Their
    /// counts are updated.
    ///
    /// # Errors
    ///
    /// Fails with the database's error.
    pub async fn detach_terms(
        &self,
        db: &Database,
        taxonomy: &Taxonomy,
        term_taxonomy_ids: &[i64],
    ) -> Result<u64, Error> {
        let object_id = self.id;
        let taxonomy = *taxonomy;
        db.transaction(async |tx| {
            let current = related(tx, object_id, &taxonomy).await?;
            let removed: Vec<i64> = current
                .into_iter()
                .filter(|id| term_taxonomy_ids.contains(id))
                .collect();
            let count = unrelate(tx, object_id, &removed).await?;
            recount(tx, &taxonomy, &removed).await?;
            Ok(count)
        })
        .await
    }
}

/// Sets `count` on the terms `term_taxonomy_ids` of `taxonomy` to the number of their posts that
/// are published and of the taxonomy's post types, as WordPress's `wp_update_term_count` does.
/// Adding and removing a post's terms, [`Post::save`], [`Post::trash`] and publishing a scheduled
/// post call it already; call it after changing a post's status any other way.
///
/// Deleting a post deletes its `term_relationships` rows through the foreign key, without a
/// recount; [`Post::delete`] reads the post's terms first and recounts them once the post is
/// gone, as WordPress's `wp_delete_post` does.
///
/// # Errors
///
/// Fails with the database's error.
pub async fn update_term_counts(
    db: &Database,
    taxonomy: &Taxonomy,
    term_taxonomy_ids: &[i64],
) -> Result<(), Error> {
    recount(&mut { db }, taxonomy, term_taxonomy_ids).await
}

/// Recounts every term post `post_id` has, in each of its taxonomies, as WordPress's
/// `_update_term_count_on_transition_post_status` does when a post's status changes. Only the
/// built-in taxonomies are known yet; terms of any other taxonomy are left as they are.
pub(crate) async fn recount_post_terms<C: Conn>(c: &mut C, post_id: i64) -> Result<(), Error> {
    let rows: Vec<(i64, String)> = raw::fetch_all(
        c.exec(),
        "SELECT tt.id, tt.taxonomy FROM term_relationships r \
         INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
         WHERE r.object_id = ?",
        &[post_id.into()],
    )
    .await?;
    let mut by_taxonomy: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, taxonomy) in rows {
        by_taxonomy.entry(taxonomy).or_default().push(id);
    }
    for (name, ids) in by_taxonomy {
        if let Some(taxonomy) = Taxonomy::builtin(&name) {
            recount(c, &taxonomy, &ids).await?;
        }
    }
    Ok(())
}

/// The `term_taxonomy` ids of post `post_id`'s terms, by taxonomy, for the built-in taxonomies:
/// what a post delete recounts once the post's relationships are gone.
pub(crate) async fn post_term_ids_in<C: Conn>(
    c: &mut C,
    post_id: i64,
) -> Result<Vec<(Taxonomy, Vec<i64>)>, Error> {
    let rows: Vec<(i64, String)> = raw::fetch_all(
        c.exec(),
        "SELECT tt.id, tt.taxonomy FROM term_relationships r \
         INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
         WHERE r.object_id = ?",
        &[post_id.into()],
    )
    .await?;
    let mut by_taxonomy: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, taxonomy) in rows {
        by_taxonomy.entry(taxonomy).or_default().push(id);
    }
    Ok(by_taxonomy
        .into_iter()
        .filter_map(|(name, ids)| Taxonomy::builtin(&name).map(|taxonomy| (taxonomy, ids)))
        .collect())
}

/// [`update_term_counts`] on a connection that is already in use.
pub(crate) async fn recount_in<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    term_taxonomy_ids: &[i64],
) -> Result<(), Error> {
    recount(c, taxonomy, term_taxonomy_ids).await
}

/// The terms of `taxonomy` with these `term_id`s, in no order, read in one query per 500 ids
/// (and one for their names), without loading the rest of the taxonomy.
pub(crate) async fn terms_by_term_id(
    db: &Database,
    taxonomy: &Taxonomy,
    term_ids: &[i64],
) -> Result<Vec<Term>, Error> {
    let mut c = db;
    let mut rows = Vec::new();
    for chunk in term_ids.chunks(500) {
        rows.extend(
            TermTaxonomyRow::query()
                .where_in("term_id", chunk.iter().copied())
                .where_eq("taxonomy", taxonomy.name)
                .get(c.exec())
                .await?,
        );
    }
    // Exact comparison: MySQL compares text without case.
    rows.retain(|row| row.taxonomy == taxonomy.name);
    with_terms(&mut c, rows).await
}

/// The term with this `term_taxonomy_id`, if there is one.
async fn find<C: Conn>(c: &mut C, term_taxonomy_id: i64) -> Result<Option<Term>, Error> {
    let Some(row) = TermTaxonomyRow::query()
        .find(c.exec(), term_taxonomy_id)
        .await?
    else {
        return Ok(None);
    };
    Ok(with_terms(c, vec![row]).await?.pop())
}

/// The term of `taxonomy` with this `term_id`, if there is one.
async fn find_by_term_id<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    term_id: i64,
) -> Result<Option<Term>, Error> {
    let rows = TermTaxonomyRow::query()
        .where_eq("term_id", term_id)
        .where_eq("taxonomy", taxonomy.name)
        .get(c.exec())
        .await?;
    Ok(with_terms(c, rows).await?.pop())
}

/// The term of `taxonomy` with this slug, if there is one.
async fn find_by_slug<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    slug: &str,
) -> Result<Option<Term>, Error> {
    let term_ids: Vec<i64> = TermRow::query()
        .where_eq("slug", slug)
        .get(c.exec())
        .await?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let rows = TermTaxonomyRow::query()
        .where_in("term_id", term_ids)
        .where_eq("taxonomy", taxonomy.name)
        .limit(1)
        .get(c.exec())
        .await?;
    Ok(with_terms(c, rows).await?.pop())
}

/// The terms of `term_taxonomy` rows, in the same order.
async fn with_terms<C: Conn>(c: &mut C, rows: Vec<TermTaxonomyRow>) -> Result<Vec<Term>, Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let terms: HashMap<i64, TermRow> = TermRow::query()
        .where_in("id", rows.iter().map(|r| r.term_id))
        .get(c.exec())
        .await?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    // The foreign key keeps every `term_id` in `terms`.
    Ok(rows
        .into_iter()
        .filter_map(|row| Some(Term::from_rows(terms.get(&row.term_id)?, row)))
        .collect())
}

/// `terms` ordered by name, ignoring case, as WordPress orders them by default.
fn by_name(mut terms: Vec<Term>) -> Vec<Term> {
    terms.sort_by_cached_key(|t| (t.name.to_lowercase(), t.term_taxonomy_id));
    terms
}

/// The term that `wp_insert_term` would refuse a new term for, as `term_exists`: one with the
/// same name, ignoring case, under the same parent (the whole taxonomy when it is flat), unless
/// the caller gave a slug of its own that no term has.
async fn existing<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    name: &str,
    slug: &str,
    slug_given: bool,
    parent_id: Option<i64>,
) -> Result<Option<i64>, Error> {
    let rows = TermTaxonomyRow::query()
        .where_eq("taxonomy", taxonomy.name)
        .where_eq("parent_id", parent_id)
        .get(c.exec())
        .await?;
    let siblings = with_terms(c, rows).await?;
    let lower = name.to_lowercase();
    let Some(name_match) = siblings.iter().find(|t| t.name.to_lowercase() == lower) else {
        return Ok(None);
    };
    let slug_match = find_by_slug(c, taxonomy, slug).await?;
    if slug_given && name_match.slug != slug && slug_match.is_none() {
        return Ok(None);
    }
    if !taxonomy.hierarchical {
        return Ok(Some(name_match.term_taxonomy_id));
    }
    if (!slug_given || name_match.slug == slug) && siblings.iter().any(|t| t.name == name) {
        return Ok(Some(name_match.term_taxonomy_id));
    }
    Ok(slug_match
        .filter(|_| siblings.iter().any(|t| t.slug == slug))
        .map(|t| t.term_taxonomy_id))
}

/// `slug`, or a free one made from it, as `wp_unique_term_slug` does: a child of a hierarchical
/// taxonomy first appends its ancestors' slugs, nearest first, until one is free; then `-2`,
/// `-3`, ... is appended. WordPress checks the numbered slugs against every taxonomy; here a slug
/// only needs to be free in its own taxonomy.
async fn unique_slug<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    slug: String,
    parent_id: Option<i64>,
) -> Result<String, Error> {
    if find_by_slug(c, taxonomy, &slug).await?.is_none() {
        return Ok(slug);
    }
    let mut slug = slug;
    if taxonomy.hierarchical {
        let mut suffix = String::new();
        let mut next = parent_id;
        while let Some(id) = next {
            let Some(parent) = find_by_term_id(c, taxonomy, id).await? else {
                break;
            };
            suffix.push('-');
            suffix.push_str(&parent.slug);
            if find_by_slug(c, taxonomy, &format!("{slug}{suffix}"))
                .await?
                .is_none()
            {
                break;
            }
            next = parent.parent_id;
        }
        slug.push_str(&suffix);
        if find_by_slug(c, taxonomy, &slug).await?.is_none() {
            return Ok(slug);
        }
    }
    let mut n = 2;
    loop {
        let numbered = format!("{slug}-{n}");
        if find_by_slug(c, taxonomy, &numbered).await?.is_none() {
            return Ok(numbered);
        }
        n += 1;
    }
}

/// Adds the terms `term_taxonomy_ids` of `taxonomy` to post `object_id` and, unless `append`,
/// removes its other terms of `taxonomy`; then updates the counts of the terms that changed.
async fn set_terms(
    db: &Database,
    object_id: i64,
    taxonomy: &Taxonomy,
    term_taxonomy_ids: &[i64],
    append: bool,
) -> Result<(), Error> {
    let taxonomy = *taxonomy;
    db.transaction(async |tx| {
        set_terms_in(tx, object_id, &taxonomy, term_taxonomy_ids, append).await
    })
    .await
}

/// [`set_terms`] on the connection of a transaction the caller holds, so the relationships
/// commit or roll back with the caller's other writes (a menu item's post and meta).
pub(crate) async fn set_terms_in(
    tx: &mut Transaction,
    object_id: i64,
    taxonomy: &Taxonomy,
    term_taxonomy_ids: &[i64],
    append: bool,
) -> Result<(), Error> {
    let taxonomy = *taxonomy;
    let wanted: HashSet<i64> = TermTaxonomyRow::query()
        .where_in("id", term_taxonomy_ids.iter().copied())
        .where_eq("taxonomy", taxonomy.name)
        .get(tx.exec())
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect();
    let current: HashSet<i64> = related(tx, object_id, &taxonomy)
        .await?
        .into_iter()
        .collect();
    let mut changed: Vec<i64> = wanted.difference(&current).copied().collect();
    for &id in &changed {
        raw::execute(
            tx.exec(),
            "INSERT INTO term_relationships (object_id, term_taxonomy_id, term_order) \
             VALUES (?, ?, 0)",
            &[object_id.into(), id.into()],
        )
        .await?;
    }
    if !append {
        let removed: Vec<i64> = current.difference(&wanted).copied().collect();
        unrelate(tx, object_id, &removed).await?;
        changed.extend(removed);
    }
    recount(tx, &taxonomy, &changed).await
}

/// The `term_taxonomy_id`s of post `object_id`'s terms in `taxonomy`.
async fn related<C: Conn>(
    c: &mut C,
    object_id: i64,
    taxonomy: &Taxonomy,
) -> Result<Vec<i64>, Error> {
    let rows: Vec<(i64,)> = raw::fetch_all(
        c.exec(),
        "SELECT r.term_taxonomy_id FROM term_relationships r \
         INNER JOIN term_taxonomy tt ON tt.id = r.term_taxonomy_id \
         WHERE r.object_id = ? AND tt.taxonomy = ?",
        &[object_id.into(), taxonomy.name.into()],
    )
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Removes the terms `term_taxonomy_ids` from post `object_id` and returns how many it had.
async fn unrelate<C: Conn>(
    c: &mut C,
    object_id: i64,
    term_taxonomy_ids: &[i64],
) -> Result<u64, Error> {
    if term_taxonomy_ids.is_empty() {
        return Ok(0);
    }
    let mut binds = vec![Value::from(object_id)];
    binds.extend(term_taxonomy_ids.iter().map(|&id| Value::from(id)));
    raw::execute(
        c.exec(),
        &format!(
            "DELETE FROM term_relationships WHERE object_id = ? AND term_taxonomy_id IN ({})",
            marks(term_taxonomy_ids.len())
        ),
        &binds,
    )
    .await
}

/// Recounts the published posts of the terms `term_taxonomy_ids`; see [`update_term_counts`].
async fn recount<C: Conn>(
    c: &mut C,
    taxonomy: &Taxonomy,
    term_taxonomy_ids: &[i64],
) -> Result<(), Error> {
    if term_taxonomy_ids.is_empty() {
        return Ok(());
    }
    if taxonomy.object_types.is_empty() {
        TermTaxonomyRow::query()
            .where_in("id", term_taxonomy_ids.iter().copied())
            .update(c.exec(), &[("count", 0_i64.into())])
            .await?;
        return Ok(());
    }
    let mut binds = vec![Value::from(PostStatus::Publish)];
    binds.extend(taxonomy.object_types.iter().map(|&t| Value::from(t)));
    binds.extend(term_taxonomy_ids.iter().map(|&id| Value::from(id)));
    raw::execute(
        c.exec(),
        &format!(
            "UPDATE term_taxonomy SET count = (\
                 SELECT COUNT(*) FROM term_relationships r \
                 INNER JOIN posts p ON p.id = r.object_id \
                 WHERE r.term_taxonomy_id = term_taxonomy.id \
                 AND p.status = ? AND p.post_type IN ({})\
             ) WHERE id IN ({})",
            marks(taxonomy.object_types.len()),
            marks(term_taxonomy_ids.len())
        ),
        &binds,
    )
    .await?;
    Ok(())
}

/// `n` comma-separated `?` placeholders.
fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}
