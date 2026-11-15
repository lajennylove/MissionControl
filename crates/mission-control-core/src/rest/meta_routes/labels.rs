//! The `labels` of post types and taxonomies, as `get_post_type_labels` and `get_taxonomy_labels`
//! fill them in: WordPress's defaults for a flat type (the post wording) or a hierarchical one
//! (the page wording), over which a type's own labels are set.

use serde_json::{Map, Value};

use crate::rest::posts::PostType;
use crate::rest::terms::TaxonomySpec;

/// A label: the flat (post, tag) value and the hierarchical (page, category) value. `None` is
/// WordPress's `null`.
type Pair = (Option<&'static str>, Option<&'static str>);

/// `get_post_type_labels`'s defaults.
const POST_TYPE_DEFAULTS: &[(&str, Pair)] = &[
    ("name", (Some("Posts"), Some("Pages"))),
    ("singular_name", (Some("Post"), Some("Page"))),
    ("add_new", (Some("Add New Post"), Some("Add New Page"))),
    ("add_new_item", (Some("Add New Post"), Some("Add New Page"))),
    ("edit_item", (Some("Edit Post"), Some("Edit Page"))),
    ("new_item", (Some("New Post"), Some("New Page"))),
    ("view_item", (Some("View Post"), Some("View Page"))),
    ("view_items", (Some("View Posts"), Some("View Pages"))),
    ("search_items", (Some("Search Posts"), Some("Search Pages"))),
    (
        "not_found",
        (Some("No posts found."), Some("No pages found.")),
    ),
    (
        "not_found_in_trash",
        (
            Some("No posts found in Trash."),
            Some("No pages found in Trash."),
        ),
    ),
    ("parent_item_colon", (None, Some("Parent Page:"))),
    ("all_items", (Some("All Posts"), Some("All Pages"))),
    ("archives", (Some("Post Archives"), Some("Page Archives"))),
    (
        "attributes",
        (Some("Post Attributes"), Some("Page Attributes")),
    ),
    (
        "insert_into_item",
        (Some("Insert into post"), Some("Insert into page")),
    ),
    (
        "uploaded_to_this_item",
        (Some("Uploaded to this post"), Some("Uploaded to this page")),
    ),
    (
        "featured_image",
        (Some("Featured image"), Some("Featured image")),
    ),
    (
        "set_featured_image",
        (Some("Set featured image"), Some("Set featured image")),
    ),
    (
        "remove_featured_image",
        (Some("Remove featured image"), Some("Remove featured image")),
    ),
    (
        "use_featured_image",
        (Some("Use as featured image"), Some("Use as featured image")),
    ),
    (
        "filter_items_list",
        (Some("Filter posts list"), Some("Filter pages list")),
    ),
    (
        "filter_by_date",
        (Some("Filter by date"), Some("Filter by date")),
    ),
    (
        "items_list_navigation",
        (Some("Posts list navigation"), Some("Pages list navigation")),
    ),
    ("items_list", (Some("Posts list"), Some("Pages list"))),
    (
        "item_published",
        (Some("Post published."), Some("Page published.")),
    ),
    (
        "item_published_privately",
        (
            Some("Post published privately."),
            Some("Page published privately."),
        ),
    ),
    (
        "item_reverted_to_draft",
        (
            Some("Post reverted to draft."),
            Some("Page reverted to draft."),
        ),
    ),
    (
        "item_trashed",
        (Some("Post trashed."), Some("Page trashed.")),
    ),
    (
        "item_scheduled",
        (Some("Post scheduled."), Some("Page scheduled.")),
    ),
    (
        "item_updated",
        (Some("Post updated."), Some("Page updated.")),
    ),
    ("item_link", (Some("Post Link"), Some("Page Link"))),
    (
        "item_link_description",
        (Some("A link to a post."), Some("A link to a page.")),
    ),
    ("menu_name", (Some("Posts"), Some("Pages"))),
    ("name_admin_bar", (Some("Post"), Some("Page"))),
];

/// The field descriptions of the term screens, the same for every taxonomy.
const NAME_FIELD: &str = "The name is how it appears on your site.";
const SLUG_FIELD: &str = "The &#8220;slug&#8221; is the URL-friendly version of the name. It is usually all lowercase and contains only letters, numbers, and hyphens.";
const DESC_FIELD: &str =
    "The description is not prominent by default; however, some themes may show it.";

/// `get_taxonomy_labels`'s defaults.
const TAXONOMY_DEFAULTS: &[(&str, Pair)] = &[
    ("name", (Some("Tags"), Some("Categories"))),
    ("singular_name", (Some("Tag"), Some("Category"))),
    (
        "search_items",
        (Some("Search Tags"), Some("Search Categories")),
    ),
    ("popular_items", (Some("Popular Tags"), None)),
    ("all_items", (Some("All Tags"), Some("All Categories"))),
    ("parent_item", (None, Some("Parent Category"))),
    ("parent_item_colon", (None, Some("Parent Category:"))),
    (
        "name_field_description",
        (Some(NAME_FIELD), Some(NAME_FIELD)),
    ),
    (
        "slug_field_description",
        (Some(SLUG_FIELD), Some(SLUG_FIELD)),
    ),
    (
        "parent_field_description",
        (
            None,
            Some(
                "Assign a parent term to create a hierarchy. The term Jazz, for example, would be the parent of Bebop and Big Band.",
            ),
        ),
    ),
    (
        "desc_field_description",
        (Some(DESC_FIELD), Some(DESC_FIELD)),
    ),
    ("edit_item", (Some("Edit Tag"), Some("Edit Category"))),
    ("view_item", (Some("View Tag"), Some("View Category"))),
    ("update_item", (Some("Update Tag"), Some("Update Category"))),
    (
        "add_new_item",
        (Some("Add New Tag"), Some("Add New Category")),
    ),
    (
        "new_item_name",
        (Some("New Tag Name"), Some("New Category Name")),
    ),
    (
        "separate_items_with_commas",
        (Some("Separate tags with commas"), None),
    ),
    ("add_or_remove_items", (Some("Add or remove tags"), None)),
    (
        "choose_from_most_used",
        (Some("Choose from the most used tags"), None),
    ),
    (
        "not_found",
        (Some("No tags found."), Some("No categories found.")),
    ),
    ("no_terms", (Some("No tags"), Some("No categories"))),
    ("filter_by_item", (None, Some("Filter by category"))),
    (
        "items_list_navigation",
        (
            Some("Tags list navigation"),
            Some("Categories list navigation"),
        ),
    ),
    ("items_list", (Some("Tags list"), Some("Categories list"))),
    ("most_used", (Some("Most Used"), Some("Most Used"))),
    (
        "back_to_items",
        (Some("&larr; Go to Tags"), Some("&larr; Go to Categories")),
    ),
    ("item_link", (Some("Tag Link"), Some("Category Link"))),
    (
        "item_link_description",
        (Some("A link to a tag."), Some("A link to a category.")),
    ),
    ("menu_name", (Some("Tags"), Some("Categories"))),
    ("name_admin_bar", (Some("Tag"), Some("Category"))),
    ("archives", (Some("All Tags"), Some("All Categories"))),
];

/// The defaults of `table` for a flat or hierarchical object.
fn defaults(table: &[(&str, Pair)], hierarchical: bool) -> Map<String, Value> {
    table
        .iter()
        .map(|(key, (flat, hier))| {
            let value = if hierarchical { hier } else { flat };
            (
                (*key).to_owned(),
                value.map_or(Value::Null, |v| Value::String(v.to_owned())),
            )
        })
        .collect()
}

/// `labels` with `name` as the label of an object registered without labels, as
/// `_get_custom_object_labels` fills them: the name, singular name, menu and admin bar name, and
/// the "all items" and archives labels.
fn named(mut labels: Map<String, Value>, name: &str) -> Map<String, Value> {
    for key in [
        "name",
        "singular_name",
        "menu_name",
        "name_admin_bar",
        "all_items",
        "archives",
    ] {
        labels.insert(key.to_owned(), Value::String(name.to_owned()));
    }
    labels
}

/// The labels of `post_type`. `post` and `page` are the defaults; `attachment` is "Media", with
/// the labels WordPress registers it with; any other type is named after itself.
pub(super) fn post_type(post_type: &PostType) -> Value {
    let labels = defaults(POST_TYPE_DEFAULTS, post_type.hierarchical);
    let labels = match post_type.name.as_str() {
        "post" | "page" => labels,
        "attachment" => {
            let mut labels = named(labels, "Media");
            for (key, value) in [
                ("add_new", "Add Media File"),
                ("edit_item", "Edit Media"),
                ("view_item", "View Media File"),
                ("attributes", "Attachment Attributes"),
            ] {
                labels.insert(key.to_owned(), Value::String(value.to_owned()));
            }
            labels
        }
        other => named(labels, other),
    };
    Value::Object(labels)
}

/// The labels of `taxonomy`: `category` and `post_tag` are the defaults; any other taxonomy is
/// named after itself.
pub(super) fn taxonomy(taxonomy: &TaxonomySpec) -> Value {
    let tax = &taxonomy.taxonomy;
    let labels = defaults(TAXONOMY_DEFAULTS, tax.hierarchical);
    Value::Object(match &*tax.name {
        "category" | "post_tag" => labels,
        other => named(labels, other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordpresss_own_objects_keep_their_labels() {
        assert_eq!(post_type(&PostType::post())["name"], "Posts");
        assert_eq!(
            post_type(&PostType::page())["parent_item_colon"],
            "Parent Page:"
        );
        assert_eq!(
            post_type(&PostType::post())["parent_item_colon"],
            Value::Null
        );
        let media = post_type(&PostType::attachment());
        assert_eq!(
            (media["name"].as_str(), media["singular_name"].as_str()),
            (Some("Media"), Some("Media"))
        );
        assert_eq!(taxonomy(&TaxonomySpec::category())["name"], "Categories");
        assert_eq!(
            taxonomy(&TaxonomySpec::post_tag())["popular_items"],
            "Popular Tags"
        );
    }
}
