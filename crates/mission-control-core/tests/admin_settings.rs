//! The admin's settings screens and Menus screen (Task 6.5, Rulings R132 and R133): each screen's
//! data layer round-trips its options through the shared `sanitize_option`, Ignite's checks and
//! `sanitize_option` refuse what WordPress refuses, the Permalinks screen stores a structure the
//! front end reads, and the Menus screen saves menus, locations and items through the menus API.
//!
//! They run on the database in `MC_TEST_DATABASE_URL`, or on a fresh SQLite file when it is unset.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::{Arc, Mutex};

use ignite_core::{FieldErrors, FieldKindDef, FormInput, Row, RuntimeResource};
use mission_control_core::admin::menus::{MenuObjectType, default_menu_object_types};
use mission_control_core::admin::{
    NAV_MENUS, PostScreen, Refusal, SettingsScreen, SettingsServices, authorize_menus_screen,
    authorize_settings_screen, nav_menus_resource, read_settings, save_settings, settings_resource,
};
use mission_control_core::content::{
    NewPost, NewTerm, PermalinkStructure, Post, PostStatus, Taxonomy, Term, get_option,
    get_post_meta, set_option,
};
use mission_control_core::hooks::Owner;
use mission_control_core::media::ImageSizes;
use mission_control_core::migrations::migrator;
use mission_control_core::rest::KsesLimiter;
use mission_control_core::theme::{
    NAV_MENU_ITEM, NewMenuItem, Theme, add_menu_item, get_nav_menu, load_theme, menu_items,
    menu_locations,
};
use mission_control_core::users::{ADMINISTRATOR, EDITOR, UserCaps};
use mission_control_testing::{create_users_table, database, unique_email};
use orbit_orm::{Database, Error, Model, raw};
use serde_json::{Value, json};

/// Ignite's runtime registry is process-wide: one test at a time registers in it.
static IGNITE: Mutex<()> = Mutex::new(());

async fn setup(db: &Database) -> i64 {
    create_users_table(db).await;
    migrator()
        .unwrap()
        .run(db)
        .await
        .expect("runs the migrations");
    raw::execute(
        db,
        "INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)",
        &["Ann".into(), unique_email("ann").into(), "hash".into()],
    )
    .await
    .expect("inserts a user");
    raw::fetch_scalar(db, "SELECT MAX(id) FROM users", &[])
        .await
        .expect("reads the user id")
}

/// What the panel's update does with `form` on `resource`: Ignite's checks (`validate_update`,
/// and each lookup value against the loaded choices), or the field errors it shows.
async fn validated(
    db: &Database,
    resource: RuntimeResource,
    form: &[(&str, &str)],
) -> Result<Row, FieldErrors> {
    let slug = resource.slug.clone();
    let data = resource.data.clone();
    let fields = resource.fields.clone();
    let values: HashMap<String, String> = form
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let row = {
        let _ignite = IGNITE.lock().unwrap_or_else(|e| e.into_inner());
        ignite_core::register_runtime(resource).expect("registers the resource");
        let row = ignite_core::runtime_resource(&slug)
            .unwrap()
            .validate_update(&FormInput::new(values.clone()));
        ignite_core::unregister_runtime(&slug).expect("unregisters the resource");
        row
    };
    let mut errors = row.as_ref().err().cloned().unwrap_or_default();
    for field in fields.iter().filter(|f| f.kind == FieldKindDef::Lookup) {
        if let Some(value) = values.get(&field.name).filter(|v| !v.trim().is_empty()) {
            let choices = data.options(db, &field.name).await.expect("loads choices");
            if !choices.iter().any(|(v, _)| v == value.trim()) {
                errors
                    .entry(field.name.clone())
                    .or_insert_with(|| "Choose one of the options.".to_owned());
            }
        }
    }
    if errors.is_empty() { row } else { Err(errors) }
}

/// The form `screen`'s edit page shows, as the browser submits it: each field's stored value
/// (a ticked checkbox as `on`, an unticked one left out), with `changes` over it.
async fn form_of(
    db: &Database,
    screen: SettingsScreen,
    changes: &[(&str, &str)],
) -> Vec<(String, String)> {
    let record = read_settings(db, screen).await.expect("reads the screen");
    let mut form: Vec<(String, String)> = Vec::new();
    for field in settings_resource(screen, SettingsServices::default()).fields {
        let value = match record.get(&field.name) {
            Some(Value::Bool(true)) => "on".to_owned(),
            Some(Value::Bool(false)) | Some(Value::Null) | None => continue,
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        form.push((field.name.clone(), value));
    }
    for (name, value) in changes {
        form.retain(|(n, _)| n != name);
        form.push(((*name).to_owned(), (*value).to_owned()));
    }
    form
}

/// Submits `changes` on `screen`'s edit form: Ignite's checks, then the data layer's save.
/// Returns the `sanitize_option` errors.
async fn submit(
    db: &Database,
    screen: SettingsScreen,
    changes: &[(&str, &str)],
    sizes: Option<Arc<ImageSizes>>,
) -> BTreeMap<String, String> {
    let form = form_of(db, screen, changes).await;
    let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let row = validated(
        db,
        settings_resource(screen, SettingsServices::default()),
        &form,
    )
    .await
    .unwrap_or_else(|e| panic!("Ignite accepts the form of {screen:?}: {e:?}"));
    let services = SettingsServices {
        image_sizes: sizes,
        kses: Some(KsesLimiter::default()),
    };
    save_settings(db, screen, &row, &services)
        .await
        .expect("saves the settings")
}

/// The field errors Ignite shows for `changes` on `screen`'s edit form.
async fn refused(db: &Database, screen: SettingsScreen, changes: &[(&str, &str)]) -> FieldErrors {
    let form = form_of(db, screen, changes).await;
    let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    validated(
        db,
        settings_resource(screen, SettingsServices::default()),
        &form,
    )
    .await
    .expect_err("Ignite refuses the form")
}

async fn option(db: &Database, name: &str) -> Option<Value> {
    get_option(db, name).await.expect("reads the option")
}

async fn page(db: &Database, author: i64, title: &str, status: PostStatus) -> Post {
    Post::create(
        db,
        NewPost {
            status,
            title: title.to_owned(),
            ..NewPost::new(author, "page")
        },
    )
    .await
    .expect("creates a page")
}

#[tokio::test]
async fn each_settings_screen_round_trips_its_options() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;

    // General: the title through `esc_html`, the addresses, the email, a UTC offset, a custom
    // date format and the week's first day.
    let errors = submit(
        db,
        SettingsScreen::General,
        &[
            ("blogname", "Tom & Jerry"),
            ("blogdescription", "Cartoons <daily>"),
            ("siteurl", "https://example.com"),
            ("home", "https://example.com/"),
            ("admin_email", " owner@example.com "),
            ("timezone_string", "UTC+5.5"),
            ("date_format", "custom"),
            ("date_format_custom", "<b>j</b> F Y"),
            ("time_format", "H:i"),
            ("time_format_custom", "g:i a"),
            ("start_of_week", "0"),
        ],
        None,
    )
    .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(option(db, "blogname").await, Some(json!("Tom &amp; Jerry")));
    assert_eq!(
        option(db, "blogdescription").await,
        Some(json!("Cartoons &lt;daily&gt;"))
    );
    assert_eq!(
        option(db, "siteurl").await,
        Some(json!("https://example.com"))
    );
    assert_eq!(
        option(db, "home").await,
        Some(json!("https://example.com/"))
    );
    assert_eq!(
        option(db, "admin_email").await,
        Some(json!("owner@example.com"))
    );
    assert_eq!(option(db, "gmt_offset").await, Some(json!("5.5")));
    assert_eq!(option(db, "timezone_string").await, Some(json!("")));
    assert_eq!(option(db, "date_format").await, Some(json!("j F Y")));
    assert_eq!(option(db, "time_format").await, Some(json!("H:i")));
    assert_eq!(option(db, "start_of_week").await, Some(json!(0)));
    let general = read_settings(db, SettingsScreen::General).await.unwrap();
    assert_eq!(general["timezone_string"], "UTC+5.5");
    assert_eq!(general["date_format"], "custom");
    assert_eq!(general["date_format_custom"], "j F Y");
    assert_eq!(general["time_format"], "H:i");
    assert_eq!(general["start_of_week"], "0");
    // The form shows the title once escaped (decoded here, escaped by the form), as
    // WordPress's `esc_attr` does not encode `&amp;` again.
    assert_eq!(general["blogname"], "Tom & Jerry");
    assert_eq!(general["blogdescription"], "Cartoons <daily>");
    // Saved back unchanged, the stored title does not change.
    submit(db, SettingsScreen::General, &[], None).await;
    assert_eq!(option(db, "blogname").await, Some(json!("Tom &amp; Jerry")));
    submit(
        db,
        SettingsScreen::General,
        &[("blogname", "Jenny's Blog")],
        None,
    )
    .await;
    assert_eq!(
        option(db, "blogname").await,
        Some(json!("Jenny&#039;s Blog"))
    );
    let general = read_settings(db, SettingsScreen::General).await.unwrap();
    assert_eq!(general["blogname"], "Jenny's Blog");
    // A named time zone: stored, with its offset now in `gmt_offset` (dates read it).
    submit(
        db,
        SettingsScreen::General,
        &[("timezone_string", "Asia/Kolkata")],
        None,
    )
    .await;
    assert_eq!(
        option(db, "timezone_string").await,
        Some(json!("Asia/Kolkata"))
    );
    assert_eq!(option(db, "gmt_offset").await, Some(json!("5.5")));
    let general = read_settings(db, SettingsScreen::General).await.unwrap();
    assert_eq!(general["timezone_string"], "Asia/Kolkata");
    // A legacy name stored by another way is offered, so the form saves without changing it.
    set_option(db, "timezone_string", &json!("US/Eastern"))
        .await
        .unwrap();
    submit(
        db,
        SettingsScreen::General,
        &[("timezone_string", "US/Eastern")],
        None,
    )
    .await;
    assert_eq!(
        option(db, "timezone_string").await,
        Some(json!("US/Eastern"))
    );

    // Writing: the default category, from the categories.
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    submit(
        db,
        SettingsScreen::Writing,
        &[("default_category", &news.term_id.to_string())],
        None,
    )
    .await;
    assert_eq!(
        option(db, "default_category").await,
        Some(json!(news.term_id))
    );

    // Reading: a static front page and a posts page, the counts, excerpts in feeds, and
    // "discourage search engines" stored as `blog_public` 0.
    let home = page(db, author, "Home", PostStatus::Publish).await;
    let blog = page(db, author, "Blog", PostStatus::Publish).await;
    let draft = page(db, author, "Draft", PostStatus::Draft).await;
    let errors = submit(
        db,
        SettingsScreen::Reading,
        &[
            ("show_on_front", "page"),
            ("page_on_front", &home.id.to_string()),
            ("page_for_posts", &blog.id.to_string()),
            ("posts_per_page", "0"),
            ("posts_per_rss", "5000"),
            ("rss_use_excerpt", "1"),
            ("blog_public", "on"),
        ],
        None,
    )
    .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(option(db, "show_on_front").await, Some(json!("page")));
    assert_eq!(option(db, "page_on_front").await, Some(json!(home.id)));
    assert_eq!(option(db, "page_for_posts").await, Some(json!(blog.id)));
    assert_eq!(option(db, "posts_per_page").await, Some(json!(1)));
    // Ruling R136: at most 1000.
    assert_eq!(option(db, "posts_per_rss").await, Some(json!(1000)));
    assert_eq!(option(db, "rss_use_excerpt").await, Some(json!(1)));
    assert_eq!(option(db, "blog_public").await, Some(json!(0)));
    let reading = read_settings(db, SettingsScreen::Reading).await.unwrap();
    assert_eq!(reading["page_on_front"], home.id.to_string());
    assert_eq!(reading["blog_public"], true);
    // A draft is not offered as the homepage (WordPress lists published pages).
    let errors = refused(
        db,
        SettingsScreen::Reading,
        &[("page_on_front", &draft.id.to_string())],
    )
    .await;
    assert!(errors.contains_key("page_on_front"));
    // The placeholder clears the posts page, and an unticked checkbox makes the site public.
    submit(
        db,
        SettingsScreen::Reading,
        &[
            ("page_for_posts", ""),
            ("posts_per_page", "12"),
            ("blog_public", ""),
        ],
        None,
    )
    .await;
    assert_eq!(option(db, "page_for_posts").await, Some(json!(0)));
    assert_eq!(option(db, "blog_public").await, Some(json!(1)));
    assert_eq!(option(db, "posts_per_page").await, Some(json!(12)));

    // Discussion: checkboxes as 1 and 0, the statuses as `open` and `closed`, numbers, choices,
    // and the key lists cleaned.
    let defaults = read_settings(db, SettingsScreen::Discussion).await.unwrap();
    assert_eq!(defaults["default_comment_status"], true);
    assert_eq!(defaults["thread_comments_depth"], "5");
    assert_eq!(defaults["comments_per_page"], 50);
    submit(
        db,
        SettingsScreen::Discussion,
        &[
            ("default_ping_status", "on"),
            ("default_comment_status", ""),
            ("require_name_email", "on"),
            ("close_comments_days_old", "30"),
            ("thread_comments_depth", "7"),
            ("comments_per_page", "20"),
            ("default_comments_page", "oldest"),
            ("comment_order", "desc"),
            ("comment_max_links", "-4"),
            ("moderation_keys", " spam \r\n\r\nspam\nviagra "),
            ("disallowed_keys", ""),
            ("avatar_rating", "PG"),
            ("avatar_default", "identicon"),
        ],
        None,
    )
    .await;
    assert_eq!(option(db, "default_ping_status").await, Some(json!("open")));
    assert_eq!(
        option(db, "default_comment_status").await,
        Some(json!("closed"))
    );
    assert_eq!(option(db, "require_name_email").await, Some(json!(1)));
    assert_eq!(option(db, "comment_registration").await, Some(json!(0)));
    assert_eq!(option(db, "close_comments_days_old").await, Some(json!(30)));
    assert_eq!(option(db, "thread_comments_depth").await, Some(json!(7)));
    assert_eq!(option(db, "comments_per_page").await, Some(json!(20)));
    assert_eq!(
        option(db, "default_comments_page").await,
        Some(json!("oldest"))
    );
    assert_eq!(option(db, "comment_order").await, Some(json!("desc")));
    assert_eq!(option(db, "comment_max_links").await, Some(json!(4)));
    assert_eq!(
        option(db, "moderation_keys").await,
        Some(json!("spam\nviagra"))
    );
    assert_eq!(option(db, "disallowed_keys").await, Some(json!("")));
    assert_eq!(option(db, "avatar_rating").await, Some(json!("PG")));
    assert_eq!(option(db, "avatar_default").await, Some(json!("identicon")));
    let discussion = read_settings(db, SettingsScreen::Discussion).await.unwrap();
    assert_eq!(discussion["default_comment_status"], false);
    assert_eq!(discussion["default_ping_status"], true);
    assert_eq!(discussion["thread_comments_depth"], "7");

    // Media: the sizes are stored and the size registry takes them.
    let sizes = Arc::new(ImageSizes::new());
    submit(
        db,
        SettingsScreen::Media,
        &[
            ("thumbnail_size_w", "120"),
            ("thumbnail_size_h", "90"),
            ("medium_size_w", "400"),
            ("medium_size_h", "400"),
            ("large_size_w", "1600"),
            ("large_size_h", "1200"),
            ("thumbnail_crop", ""),
        ],
        Some(Arc::clone(&sizes)),
    )
    .await;
    assert_eq!(option(db, "thumbnail_size_w").await, Some(json!(120)));
    assert_eq!(option(db, "thumbnail_crop").await, Some(json!(0)));
    let thumbnail = sizes.get("thumbnail").unwrap();
    assert_eq!(
        (thumbnail.width, thumbnail.height, thumbnail.crop),
        (120, 90, false)
    );
    assert_eq!(sizes.get("large").unwrap().width, 1600);
    // A theme's size is left alone.
    sizes.add_image_size("hero", 1200, 400, true, Owner::Theme("t".into()));
    mission_control_core::admin::apply_media_sizes(db, &sizes)
        .await
        .unwrap();
    assert!(sizes.has_image_size("hero"));

    // Privacy: draft pages are offered too.
    submit(
        db,
        SettingsScreen::Privacy,
        &[("wp_page_for_privacy_policy", &draft.id.to_string())],
        None,
    )
    .await;
    assert_eq!(
        option(db, "wp_page_for_privacy_policy").await,
        Some(json!(draft.id))
    );
}

#[tokio::test]
async fn invalid_values_are_refused_with_field_errors() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    set_option(db, "admin_email", &json!("owner@example.com"))
        .await
        .unwrap();
    set_option(db, "home", &json!("https://example.com"))
        .await
        .unwrap();
    set_option(db, "siteurl", &json!("https://example.com"))
        .await
        .unwrap();
    set_option(db, "permalink_structure", &json!("/%postname%/"))
        .await
        .unwrap();

    // Ignite's own checks, shown on the form.
    let errors = refused(
        db,
        SettingsScreen::General,
        &[
            ("admin_email", "not an email"),
            ("timezone_string", "Mars/Olympus"),
        ],
    )
    .await;
    assert_eq!(
        errors.get("admin_email").map(String::as_str),
        Some("Administration Email Address must be a valid email address.")
    );
    assert_eq!(
        errors.get("timezone_string").map(String::as_str),
        Some("Choose one of the options.")
    );
    let errors = refused(db, SettingsScreen::General, &[("siteurl", "")]).await;
    assert!(errors["siteurl"].contains("is required"), "{errors:?}");
    let errors = refused(db, SettingsScreen::Reading, &[("posts_per_page", "ten")]).await;
    assert_eq!(
        errors["posts_per_page"],
        "Blog pages show at most must be a whole number."
    );
    let errors = refused(
        db,
        SettingsScreen::Discussion,
        &[("thread_comments_depth", "40"), ("avatar_rating", "NC-17")],
    )
    .await;
    assert!(errors.contains_key("thread_comments_depth"));
    assert!(errors.contains_key("avatar_rating"));

    // `sanitize_option`'s refusals (Ruling R135): nothing of the form is saved, the error is
    // WordPress's, and the data layer refuses the save (Ignite shows its error page).
    let form = form_of(
        db,
        SettingsScreen::General,
        &[("siteurl", "ftp://example.com"), ("blogname", "Kept")],
    )
    .await;
    let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let resource = settings_resource(SettingsScreen::General, SettingsServices::default());
    let data = resource.data.clone();
    let row = validated(db, resource, &form).await.unwrap();
    assert!(data.update(db, 1, row).await.is_err());
    assert_eq!(option(db, "blogname").await, None);
    let errors = submit(
        db,
        SettingsScreen::General,
        &[("siteurl", "ftp://example.com"), ("blogname", "Kept")],
        None,
    )
    .await;
    assert!(
        errors["siteurl"].contains("did not appear to be a valid URL"),
        "{errors:?}"
    );
    assert_eq!(
        option(db, "siteurl").await,
        Some(json!("https://example.com"))
    );
    assert_eq!(option(db, "blogname").await, None);
    // The address is cleaned with `esc_url_raw` before it is checked, as WordPress does.
    let errors = submit(
        db,
        SettingsScreen::General,
        &[("siteurl", "https://example.com/a b")],
        None,
    )
    .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        option(db, "siteurl").await,
        Some(json!("https://example.com/a%20b"))
    );
    // The default category is required (WordPress's dropdown has no empty choice).
    let errors = refused(db, SettingsScreen::Writing, &[("default_category", "")]).await;
    assert!(errors.contains_key("default_category"), "{errors:?}");

    // Permalinks: a custom structure needs a tag, and one the front end reads.
    for (custom, message) in [
        (
            "/archives/",
            "A structure tag is required when using custom permalinks.",
        ),
        (
            "/%postid%/",
            "The permalink structure is not valid: %postid% is not a permalink structure tag.",
        ),
        (
            "/%postname/",
            "A structure tag is required when using custom permalinks.",
        ),
    ] {
        let errors = submit(
            db,
            SettingsScreen::Permalink,
            &[
                ("permalink_selection", "custom"),
                ("permalink_structure", custom),
            ],
            None,
        )
        .await;
        assert_eq!(
            errors.get("permalink_structure").map(String::as_str),
            Some(message),
            "{custom}"
        );
        assert_eq!(
            option(db, "permalink_structure").await,
            Some(json!("/%postname%/"))
        );
    }
}

#[tokio::test]
async fn the_permalinks_screen_stores_wordpresss_structures() {
    let test = database().await;
    let db = test.db();
    setup(db).await;
    let defaults = read_settings(db, SettingsScreen::Permalink).await.unwrap();
    assert_eq!(defaults["permalink_selection"], "plain");

    for (choice, structure) in [
        ("day", "/%year%/%monthnum%/%day%/%postname%/"),
        ("month", "/%year%/%monthnum%/%postname%/"),
        ("numeric", "/archives/%post_id%"),
        ("postname", "/%postname%/"),
        ("plain", ""),
    ] {
        // The custom text is ignored when a common setting is chosen.
        submit(
            db,
            SettingsScreen::Permalink,
            &[
                ("permalink_selection", choice),
                ("permalink_structure", "/%nope%/"),
            ],
            None,
        )
        .await;
        assert_eq!(
            option(db, "permalink_structure").await,
            Some(json!(structure)),
            "{choice}"
        );
        let read = read_settings(db, SettingsScreen::Permalink).await.unwrap();
        assert_eq!(read["permalink_selection"], choice);
        assert!(structure.parse::<PermalinkStructure>().is_ok() || structure.is_empty());
    }

    // A custom structure gets its leading slash, loses `#` and repeated slashes; the bases too.
    let errors = submit(
        db,
        SettingsScreen::Permalink,
        &[
            ("permalink_selection", "custom"),
            ("permalink_structure", "blog//%year%/#%postname%/"),
            ("category_base", "topics#"),
            ("tag_base", "//labels"),
        ],
        None,
    )
    .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        option(db, "permalink_structure").await,
        Some(json!("/blog/%year%/%postname%/"))
    );
    assert_eq!(option(db, "category_base").await, Some(json!("/topics")));
    assert_eq!(option(db, "tag_base").await, Some(json!("/labels")));
    let read = read_settings(db, SettingsScreen::Permalink).await.unwrap();
    assert_eq!(read["permalink_selection"], "custom");
    assert_eq!(read["permalink_structure"], "/blog/%year%/%postname%/");
    // Emptied bases go back to WordPress's `category` and `tag`.
    submit(
        db,
        SettingsScreen::Permalink,
        &[("category_base", ""), ("tag_base", "")],
        None,
    )
    .await;
    assert_eq!(option(db, "category_base").await, Some(json!("")));
}

#[test]
fn settings_need_manage_options_and_menus_edit_theme_options() {
    let caps = |role| UserCaps::with_default_roles(1, &[role], &BTreeMap::new());
    for screen in SettingsScreen::ALL {
        assert_eq!(
            authorize_settings_screen(&caps(ADMINISTRATOR), screen, PostScreen::Edit(1)),
            None
        );
        let refusal = authorize_settings_screen(&caps(EDITOR), screen, PostScreen::List);
        let expected = if screen == SettingsScreen::Privacy {
            Refusal::ManagePrivacyOptions
        } else {
            Refusal::ManageOptions
        };
        assert_eq!(refusal, Some(expected), "{screen:?}");
        for request in [PostScreen::New, PostScreen::Create, PostScreen::Delete(1)] {
            assert_eq!(
                authorize_settings_screen(&caps(ADMINISTRATOR), screen, request),
                Some(Refusal::NoSuchPage)
            );
        }
    }
    assert_eq!(
        authorize_menus_screen(&caps(ADMINISTRATOR), PostScreen::New),
        None
    );
    assert_eq!(
        authorize_menus_screen(&caps(EDITOR), PostScreen::List),
        Some(Refusal::EditThemeOptions)
    );
    assert_eq!(
        Refusal::EditThemeOptions.message(),
        "Sorry, you are not allowed to edit theme options on this site."
    );
}

/// A theme with a `primary` and a `footer` menu location, in a temporary folder.
/// The seven settings screens are listed under the Settings entry, a label the panel declares
/// (`SETTINGS_PARENT`), in Site, by their menu titles and in WordPress's submenu order (80 to
/// 86), so the entry opens General, the first.
#[test]
fn settings_screens_are_listed_under_settings_in_wordpresss_order() {
    use mission_control_core::admin::navigation::{SETTINGS, SETTINGS_PARENT, SITE};
    let mut sorts = Vec::new();
    for screen in SettingsScreen::ALL {
        let resource = settings_resource(screen, SettingsServices::default());
        assert_eq!(resource.navigation_group.as_deref(), Some(SITE));
        assert_eq!(
            resource.navigation_parent_item.as_deref(),
            Some(SETTINGS_PARENT),
            "{}",
            screen.slug()
        );
        assert_eq!(
            resource.navigation_label.as_deref(),
            Some(screen.menu_title())
        );
        sorts.push(resource.navigation_sort.unwrap());
    }
    assert_eq!(sorts, (SETTINGS..SETTINGS + 7).collect::<Vec<_>>());
    assert_eq!(SettingsScreen::ALL[0], SettingsScreen::General);
    assert_eq!(SETTINGS_PARENT, "Settings");
}

fn theme() -> (tempfile::TempDir, Theme) {
    let dir = tempfile::tempdir().unwrap();
    let theme_dir = dir.path().join("menus-theme");
    fs::create_dir_all(&theme_dir).unwrap();
    fs::write(
        theme_dir.join("theme.toml"),
        "name = \"Menus\"\n[menus]\nprimary = \"Primary\"\nfooter = \"Footer\"\n",
    )
    .unwrap();
    fs::write(theme_dir.join("index.propel"), "").unwrap();
    let theme = load_theme(&theme_dir).unwrap();
    (dir, theme)
}

/// The repeater rows as the form submits them.
fn items(rows: Value) -> String {
    rows.to_string()
}

#[tokio::test]
async fn the_menus_screen_saves_menus_locations_and_items() {
    let test = database().await;
    let db = test.db();
    let author = setup(db).await;
    let (_dir, theme) = theme();
    let about = page(db, author, "About", PostStatus::Publish).await;
    let trashed = page(db, author, "Old", PostStatus::Trash).await;
    let news = Term::create(db, &Taxonomy::CATEGORY, NewTerm::new("News"))
        .await
        .unwrap();
    let mut types = default_menu_object_types();
    types.push((MenuObjectType::PostType("book".into()), "Book".into()));
    let resource = || nav_menus_resource(&theme, types.clone());
    let names: Vec<String> = resource().fields.iter().map(|f| f.name.clone()).collect();
    assert_eq!(
        names,
        ["name", "auto_add", "loc_primary", "loc_footer", "items"]
    );
    let data = resource().data;

    // New: a name, a location and three items, the third under the first.
    let rows = json!([
        {"title": "", "type": "page", "object_id": about.id, "classes": "a b!", "new_tab": false},
        {"title": "Docs", "type": "custom", "url": "https://rocketboost.rs/docs/", "new_tab": true},
        {"title": "", "type": "category", "object_id": news.term_id, "parent": 1},
    ]);
    let input = validated(
        db,
        resource(),
        &[
            ("name", "Main menu"),
            ("loc_primary", "on"),
            ("items", &items(rows)),
        ],
    )
    .await
    .expect("Ignite accepts the form");
    let created = data.create(db, input).await.expect("creates the menu");
    let menu_id = created["id"].as_i64().unwrap();
    assert_eq!(created["name"], "Main menu");
    assert_eq!(created["loc_primary"], true);
    assert_eq!(created["loc_footer"], false);
    assert_eq!(created["col_items"], "3");
    assert_eq!(created["col_locations"], "Primary");
    let menu = get_nav_menu(db, menu_id).await.unwrap().unwrap();
    assert_eq!(
        menu_locations(db, theme.slug()).await.unwrap()["primary"],
        menu_id
    );
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[0].classes, ["a", "b"]);
    assert_eq!(stored[1].target, "_blank");
    assert_eq!(stored[2].parent, Some(stored[0].id));
    assert_eq!(
        get_post_meta(db, stored[2].id, "_menu_item_object")
            .await
            .unwrap(),
        Some(json!("category"))
    );
    // The data layer does not know who is signed in (owner item O8): a menu's first items are
    // by the site's first user.
    let first_user: i64 = raw::fetch_scalar(db, "SELECT MIN(id) FROM users", &[])
        .await
        .unwrap();
    let item = Post::query().find(db, stored[0].id).await.unwrap().unwrap();
    assert_eq!(
        (item.post_type.as_str(), item.author_id),
        (NAV_MENU_ITEM, first_user)
    );
    // The form reads the items back as rows, the parent as its item's id (WordPress's
    // `menu-item-parent-id`).
    let row: Value = created["items"].clone();
    assert_eq!(row[2]["parent"], stored[0].id);
    assert_eq!(row[0]["parent"], 0);
    assert_eq!(row[0]["type"], "page");
    assert_eq!(row[1]["url"], "https://rocketboost.rs/docs/");

    let ids: Vec<i64> = stored.iter().map(|s| s.id).collect();
    // Ruling R135: a row that links to nothing (a trashed page, an unknown page) refuses the
    // whole save; nothing changes, not the name or the locations either.
    for bad in [
        json!({"id": ids[0], "title": "About us", "type": "page", "object_id": trashed.id}),
        json!({"title": "Gone", "type": "page", "object_id": 999_999}),
        json!({"title": "No term", "type": "category", "object_id": 999_999}),
    ] {
        let rows = json!([
            {"id": ids[1], "title": "Docs!", "type": "custom", "url": "/docs/"},
            bad,
        ]);
        let input = validated(
            db,
            resource(),
            &[
                ("name", "Renamed"),
                ("loc_footer", "on"),
                ("items", &items(rows)),
            ],
        )
        .await
        .unwrap();
        // Refused on the items field: the form shows the reason under it.
        let user = ignite_core::ActingUser::new(1, "admin", "admin@example.com");
        let refused = data
            .try_update_for(db, &user, menu_id, input)
            .await
            .expect("no database error")
            .expect_err("refuses the items");
        assert!(
            refused.errors()["items"].contains("to nothing"),
            "{refused:?}"
        );
        let menu = get_nav_menu(db, menu_id).await.unwrap().unwrap();
        assert_eq!(menu.name, "Main menu");
        assert_eq!(menu_items(db, &menu).await.unwrap().len(), 3);
        assert_eq!(
            menu_locations(db, theme.slug())
                .await
                .unwrap()
                .get("footer"),
            None
        );
    }

    // Edit: reorder (Docs first), rename, keep the page, drop the category, add a child of Docs,
    // and move the menu from `primary` to `footer`.
    let rows = json!([
        {"id": ids[1], "title": "Docs!", "type": "custom", "url": "/docs/", "new_tab": false},
        {"id": ids[0], "title": "About us", "type": "page", "object_id": about.id},
        {"title": "Guide", "type": "custom", "url": "/docs/guide/", "parent": 1},
    ]);
    let input = validated(
        db,
        resource(),
        &[
            ("name", "Main"),
            ("loc_primary", ""),
            ("loc_footer", "on"),
            ("items", &items(rows)),
        ],
    )
    .await
    .unwrap();
    let updated = data
        .update(db, menu_id, input)
        .await
        .expect("saves the menu");
    assert_eq!(updated["name"], "Main");
    assert_eq!(updated["loc_primary"], false);
    assert_eq!(updated["loc_footer"], true);
    let stored = menu_items(db, &menu).await.unwrap();
    let titles: Vec<&str> = stored.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Docs!", "About us", "Guide"]);
    assert_eq!(stored[0].id, ids[1]);
    assert_eq!(stored[1].id, ids[0]);
    assert_eq!(stored[2].parent, Some(ids[1]));
    assert!(Post::query().find(db, ids[2]).await.unwrap().is_none());
    assert!(
        get_post_meta(db, ids[2], "_menu_item_type")
            .await
            .unwrap()
            .is_none()
    );
    let locations = menu_locations(db, theme.slug()).await.unwrap();
    assert_eq!(locations.get("primary"), None);
    assert_eq!(locations.get("footer"), Some(&menu_id));

    // Parents: one naming its own row, two naming each other (a loop) and one past the last row
    // all go to the top level; a `javascript:` link is stored empty, as `esc_url_raw` empties it.
    let rows = json!([
        {"id": ids[1], "title": "Self", "type": "custom", "url": "/a/", "parent": 1},
        {"title": "Loop A", "type": "custom", "url": "/b/", "parent": 3},
        {"title": "Loop B", "type": "custom", "url": "/c/", "parent": 2},
        {"title": "Far", "type": "custom", "url": "/d/", "parent": 99},
        {"title": "Script", "type": "custom", "url": "javascript:alert(1)"},
    ]);
    let input = validated(
        db,
        resource(),
        &[
            ("name", "Main"),
            ("loc_footer", "on"),
            ("items", &items(rows)),
        ],
    )
    .await
    .unwrap();
    data.update(db, menu_id, input).await.expect("saves");
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored.len(), 5);
    assert_eq!(stored[0].parent, None, "self");
    assert_eq!(stored[3].parent, None, "out of range");
    // A loop is broken: at least one of the two is at the top level, and no item is its own
    // ancestor.
    assert!(
        stored[1].parent.is_none() || stored[2].parent.is_none(),
        "loop"
    );
    assert_eq!(
        stored[4].object,
        mission_control_core::theme::MenuItemObject::Custom { url: String::new() }
    );
    assert_eq!(
        get_post_meta(db, stored[4].id, "_menu_item_url")
            .await
            .unwrap(),
        Some(json!(""))
    );

    // WordPress's "Invalid" items: an item whose page was trashed later, and an item of a type
    // the screen does not offer (here `book`), are kept when their rows come back unchanged, and
    // the menu still saves (renamed, moved to `primary`).
    let mut gone = page(db, author, "Gone later", PostStatus::Publish).await;
    let book = Post::create(
        db,
        NewPost {
            status: PostStatus::Publish,
            title: "Dune".to_owned(),
            ..NewPost::new(author, "book")
        },
    )
    .await
    .unwrap();
    let gone_item = add_menu_item(db, &menu, author, NewMenuItem::post(&gone))
        .await
        .unwrap();
    let book_item = add_menu_item(db, &menu, author, NewMenuItem::post(&book))
        .await
        .unwrap();
    gone.trash(db).await.unwrap();
    let no_book = || nav_menus_resource(&theme, default_menu_object_types());
    let no_book_data = no_book().data;
    let shown = no_book_data.find(db, menu_id).await.unwrap().unwrap();
    let rows = shown["items"].to_string();
    let input = validated(
        db,
        no_book(),
        &[
            ("name", "Main again"),
            ("loc_primary", "on"),
            ("loc_footer", ""),
            ("items", &rows),
        ],
    )
    .await
    .expect("Ignite accepts the stored rows, the book's type included");
    let saved = no_book_data
        .update(db, menu_id, input)
        .await
        .expect("saves a menu with an invalid item");
    assert_eq!(saved["name"], "Main again");
    assert_eq!(saved["loc_primary"], true);
    let stored = menu_items(db, &menu).await.unwrap();
    assert_eq!(stored.len(), 7);
    let kept = |id: i64| stored.iter().find(|s| s.id == id).cloned().expect("kept");
    assert_eq!(kept(gone_item.id).object, NewMenuItem::post(&gone).object);
    assert_eq!(kept(book_item.id).object, NewMenuItem::post(&book).object);
    // A changed row is checked: pointing the kept item at another trashed page, or a new row
    // of the type the screen does not offer, refuses the save.
    for bad in [
        json!({"id": gone_item.id, "type": "page", "object_id": trashed.id}),
        json!({"type": "book", "object_id": book.id}),
    ] {
        let mut rows: Vec<Value> = serde_json::from_str(&rows).unwrap();
        rows.push(bad);
        let input = validated(
            db,
            no_book(),
            &[
                ("name", "Main again"),
                ("items", &Value::Array(rows).to_string()),
            ],
        )
        .await
        .unwrap();
        assert!(no_book_data.update(db, menu_id, input).await.is_err());
        assert_eq!(menu_items(db, &menu).await.unwrap().len(), 7);
    }
    let input = validated(db, resource(), &[("name", "Main"), ("loc_footer", "on")])
        .await
        .unwrap();
    data.update(db, menu_id, input).await.unwrap();
    let ids: Vec<i64> = menu_items(db, &menu)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();

    // A second menu cannot take the name, in any case.
    let input = validated(db, resource(), &[("name", "MAIN")])
        .await
        .unwrap();
    match data.create(db, input).await {
        Err(Error::UniqueViolation { index: Some(index) }) => {
            assert_eq!(index, format!("{NAV_MENUS}_name_unique"));
        }
        other => panic!("refuses the name: {other:?}"),
    }
    let other = data
        .create(
            db,
            validated(db, resource(), &[("name", "Footer links")])
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let other_id = other["id"].as_i64().unwrap();
    assert!(matches!(
        data.update(
            db,
            other_id,
            validated(db, resource(), &[("name", "main")])
                .await
                .unwrap()
        )
        .await,
        Err(Error::UniqueViolation { .. })
    ));

    // The list: names, locations and counts, searched and sorted.
    let page = data
        .list(
            db,
            &ignite_core::ListQuery {
                page: 1,
                per_page: 500,
                search: Some("mai".into()),
                sort: None,
                filter: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0]["col_name"], "Main");
    assert_eq!(page.rows[0]["col_locations"], "Footer");
    assert_eq!(page.rows[0]["col_items"], "7");

    // Delete: the items, the term and the location go.
    assert_eq!(data.delete(db, menu_id).await.unwrap(), 1);
    assert!(get_nav_menu(db, menu_id).await.unwrap().is_none());
    for id in ids {
        assert!(Post::query().find(db, id).await.unwrap().is_none());
    }
    assert_eq!(
        menu_locations(db, theme.slug())
            .await
            .unwrap()
            .get("footer"),
        None
    );
    assert_eq!(data.delete(db, menu_id).await.unwrap(), 0);
}
