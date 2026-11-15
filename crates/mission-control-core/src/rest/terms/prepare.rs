//! A term as the REST API shows it: `prepare_item_for_response` and `prepare_links` of
//! `WP_REST_Terms_Controller`, the context filter, `_fields` and `_embed` (the parent, `up`).
//!
//! What every term of a page needs beyond its row (links, parents) is read in one query per kind
//! for the whole page.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::TermsController;
use crate::content::{Term, terms_by_term_id};
use crate::front::term_links;
use crate::rest::RestError;
use crate::rest::meta::{MetaObject, MetaValues};
use crate::rest::shared::{Context, Fields, Site, curies, database_error};

/// How the terms of a request are shown.
#[derive(Debug, Clone)]
pub(super) struct ItemOptions {
    /// The request's context.
    pub(super) context: Context,
    /// `_fields`.
    pub(super) fields: Fields,
    /// `_embed`: `None` for no embedding, an empty list for every relation.
    pub(super) embed: Option<Vec<String>>,
}

/// The JSON of `terms` for `options`, in order.
pub(super) async fn items(
    ctl: &TermsController<'_>,
    site: &Site,
    terms: &[Term],
    options: &ItemOptions,
) -> Result<Vec<Value>, RestError> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let with_links = options.fields.includes("_links") || options.fields.includes("_embedded");
    let mut parent_ids: Vec<i64> = terms.iter().filter_map(|t| t.parent_id).collect();
    parent_ids.sort_unstable();
    parent_ids.dedup();
    let parents: HashMap<i64, Term> = if with_links && !parent_ids.is_empty() {
        terms_by_term_id(ctl.db, &ctl.taxonomy.taxonomy, &parent_ids)
            .await
            .map_err(|e| database_error(&e))?
            .into_iter()
            .map(|t| (t.term_id, t))
            .collect()
    } else {
        HashMap::new()
    };
    let embed_up = with_links
        && options
            .embed
            .as_ref()
            .is_some_and(|e| e.is_empty() || e.iter().any(|rel| rel == "up"));
    let embedded: HashMap<i64, Value> = if embed_up && !parents.is_empty() {
        let list: Vec<Term> = parents.values().cloned().collect();
        let embed_options = ItemOptions {
            context: Context::Embed,
            fields: Fields(None),
            embed: None,
        };
        let rendered = render(ctl, site, &list, &embed_options, &HashMap::new()).await?;
        list.iter()
            .map(|t| t.term_id)
            .zip(rendered.into_iter().map(Value::Object))
            .collect()
    } else {
        HashMap::new()
    };
    let rendered = render(ctl, site, terms, options, &parents).await?;
    Ok(terms
        .iter()
        .zip(rendered)
        .map(|(term, mut item)| {
            if let Some(parent) = term.parent_id.and_then(|id| embedded.get(&id))
                && item.contains_key("_links")
            {
                item.insert("_embedded".to_owned(), json!({"up": [parent]}));
            }
            Value::Object(item)
        })
        .collect())
}

/// The JSON of `terms` without anything embedded. `parents` are the terms' parents that exist,
/// for the `up` links.
async fn render(
    ctl: &TermsController<'_>,
    site: &Site,
    terms: &[Term],
    options: &ItemOptions,
    parents: &HashMap<i64, Term>,
) -> Result<Vec<Map<String, Value>>, RestError> {
    let paths = term_links(ctl.db, &site.structure, terms)
        .await
        .map_err(|e| database_error(&e))?;
    let with_links = options.fields.includes("_links") || options.fields.includes("_embedded");
    let defaults = if with_links {
        ctl.default_terms().await?
    } else {
        Vec::new()
    };
    let meta = if options.context != Context::Embed && options.fields.includes("meta") {
        let ids: Vec<i64> = terms.iter().map(|t| t.term_id).collect();
        MetaValues::load(
            ctl.db,
            ctl.meta,
            MetaObject::Term,
            ctl.taxonomy.taxonomy.name,
            &ids,
        )
        .await?
    } else {
        MetaValues::default()
    };
    Ok(terms
        .iter()
        .zip(paths)
        .map(|(term, path)| {
            let link = match path {
                Some(path) => format!("{}{path}", site.home),
                None => format!(
                    "{}/?taxonomy={}&term={}",
                    site.home, term.taxonomy, term.slug
                ),
            };
            let item = full_item(ctl, term, link, meta.json(term.term_id));
            let mut item = options.fields.filter(by_context(item, options.context));
            if with_links {
                item.insert(
                    "_links".to_owned(),
                    Value::Object(links(ctl, site, term, parents, &defaults)),
                );
            }
            item
        })
        .collect())
}

/// Every field of `term`, as the view and edit contexts show it.
fn full_item(
    ctl: &TermsController<'_>,
    term: &Term,
    link: String,
    meta: Value,
) -> Map<String, Value> {
    let mut item = Map::new();
    let mut put = |key: &str, value: Value| {
        item.insert(key.to_owned(), value);
    };
    put("id", json!(term.term_id));
    put("count", json!(term.count));
    put("description", json!(term.description));
    put("link", json!(link));
    put("name", json!(term.name));
    put("slug", json!(term.slug));
    put("taxonomy", json!(term.taxonomy));
    if ctl.taxonomy.taxonomy.hierarchical {
        put("parent", json!(term.parent_id.unwrap_or(0)));
    }
    put("meta", meta);
    item
}

/// The fields `context` shows: every field in `view` and `edit`; in `embed` only those
/// WordPress's schema marks for it.
fn by_context(mut item: Map<String, Value>, context: Context) -> Map<String, Value> {
    const EMBED: &[&str] = &["id", "link", "name", "slug", "taxonomy"];
    if context == Context::Embed {
        item.retain(|key, _| EMBED.contains(&key.as_str()));
    }
    item
}

/// The `_links` of `term`, as `prepare_links` gives them, with the `self` link's `targetHints`.
fn links(
    ctl: &TermsController<'_>,
    site: &Site,
    term: &Term,
    parents: &HashMap<i64, Term>,
    defaults: &[i64],
) -> Map<String, Value> {
    let base = format!("wp/v2/{}", ctl.taxonomy.rest_base);
    let mut allow = vec!["GET"];
    if ctl.can_on("edit_term", term, defaults) {
        allow.extend(["POST", "PUT", "PATCH"]);
    }
    if ctl.can_on("delete_term", term, defaults) {
        allow.push("DELETE");
    }
    let mut links = Map::new();
    let mut put = |rel: &str, value: Value| {
        links.insert(rel.to_owned(), value);
    };
    put(
        "self",
        json!([{"href": site.rest_url(&format!("{base}/{}", term.term_id)), "targetHints": {"allow": allow}}]),
    );
    put("collection", json!([{"href": site.rest_url(&base)}]));
    put(
        "about",
        json!([{"href": site.rest_url(&format!("wp/v2/taxonomies/{}", term.taxonomy))}]),
    );
    if let Some(parent) = term.parent_id.and_then(|id| parents.get(&id)) {
        put(
            "up",
            json!([{"embeddable": true, "href": site.rest_url(&format!("{base}/{}", parent.term_id))}]),
        );
    }
    let post_types: Vec<Value> = ctl
        .taxonomy
        .taxonomy
        .object_types
        .iter()
        .filter_map(|name| ctl.post_types.by_name(name))
        .map(|post_type| {
            json!({"href": format!(
                "{}?{}={}",
                site.rest_url(&format!("wp/v2/{}", post_type.rest_base)),
                ctl.taxonomy.rest_base,
                term.term_id
            )})
        })
        .collect();
    if !post_types.is_empty() {
        put("wp:post_type", Value::Array(post_types));
    }
    put("curies", curies());
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embed_context_keeps_wordpresss_embed_fields() {
        let Value::Object(item) = json!({
            "id": 1, "count": 2, "description": "d", "link": "l", "name": "n", "slug": "s",
            "taxonomy": "category", "parent": 0, "meta": {},
        }) else {
            unreachable!()
        };
        assert_eq!(
            Value::Object(by_context(item.clone(), Context::Embed)),
            json!({"id": 1, "link": "l", "name": "n", "slug": "s", "taxonomy": "category"})
        );
        assert_eq!(by_context(item.clone(), Context::View), item);
    }
}
