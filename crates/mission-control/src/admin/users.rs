//! The Users screens, as WordPress's `users.php`, `user-new.php` and `user-edit.php`, built on
//! Ignite's Users screen (`ignite_panel::screens::users_resource`, a Filament `UserResource`)
//! with Mission Control's fields and WordPress's labels, and Ignite's Roles screen for the roles
//! and their capabilities.
//!
//! - **Users** in the Site group at WordPress's position (70), with its submenu as WordPress's:
//!   All Users, Add New User, Your Profile (a link to the panel's profile page), then Roles.
//! - **The list** (`users.php`): the columns Username, Name (the first and last name), Email,
//!   Role and Posts (the published posts, `count_many_users_posts`); a search on the username
//!   and the email; one view per role with its count, after All ("Administrator (1)"), as
//!   `WP_Users_List_Table::get_views`; and the row actions "Change role to ..." (WordPress's
//!   bulk `promote` action, one per role) and Delete.
//! - **Add New User** (`user-new.php`): Username, Email, First Name, Last Name, Website,
//!   Password (with WordPress's "Generate password" button) and Role, the new user's role
//!   chosen in a select (`wp_dropdown_roles`, Subscriber first chosen as `default_role`). A
//!   username already taken is refused with WordPress's "Sorry, that username already exists!".
//! - **Edit User** (`user-edit.php`): the same fields; the username is shown, not changed
//!   ("Usernames cannot be changed."), and a blank password keeps the stored one.
//!
//! The capabilities are WordPress's: the list needs `list_users`, Add New User `create_users`,
//! the edit form `edit_users`, Delete `delete_users`, and changing a role `promote_users`
//! (`users.php`'s `promote` action); the Roles screen needs `promote_users` too. Ignite checks
//! them as the screens' abilities, so a user without one sees neither the entry nor the screen
//! (403).
//!
//! The first and last names are the `first_name` and `last_name` user meta, as WordPress keeps
//! them; the website is the `user_url` user meta (WordPress keeps it in `wp_users`, whose
//! columns are Ignite's here).

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use ignite_panel::authz;
use ignite_panel::ignite_core::{
    ActingUser, BadgeColor, ColumnDef, FieldDef, FieldErrors, FieldKindDef, FormInput, ListQuery,
    Operation, Page, Row, RowAction, RuntimeData, RuntimeFuture, RuntimeResource, Tab,
    ValidationError,
};
use ignite_panel::role_label;
use ignite_panel::screens::{USERS_SLUG, UsersData, roles_resource, users_resource};
use mission_control_core::admin::ScreenCaps;
use mission_control_core::admin::navigation;
use mission_control_core::content::set_user_meta;
use mission_control_core::users::{IGNITE_SUPER_ADMIN, WORDPRESS_ROLES, default_role};
use orbit_orm::{Database, Error, Value as Sql, raw};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::{ContentType, Method, Status};
use rocket::response::Redirect;
use rocket::route::Route;
use rocket::{Request, Response};
use serde_json::{Value, json};

use super::ADMIN_PATH;

/// The slug of the "Your Profile" entry, which opens the panel's profile page.
pub(crate) const PROFILE_SLUG: &str = "your-profile";

/// The label of the Users screen's create link, as WordPress's Users > Add New User.
const ADD_NEW_USER: &str = "Add New User";

/// The user meta of the first name.
const FIRST_NAME: &str = "first_name";
/// The user meta of the last name.
const LAST_NAME: &str = "last_name";
/// The website's form field.
const URL: &str = "url";
/// The user meta the website is kept in.
const USER_URL: &str = "user_url";
/// The role's form field.
const ROLE: &str = "role";
/// The list's Name column: the first and last name.
const DISPLAY_NAME: &str = "display_name";
/// The list's Posts column.
const POSTS: &str = "posts";

/// WordPress's refusal of a username another user has (`register_new_user`, `edit_user`).
pub(crate) const USERNAME_TAKEN: &str = "Sorry, that username already exists!";

/// WordPress's refusal of a role change the user may not make (`users.php`).
const NOT_ALLOWED_ROLE: &str = "Sorry, you are not allowed to give users that role.";

/// WordPress's refusal of a role change that would take the current user's own user editing
/// away (`users.php`, `err_admin_role`).
const OWN_ROLE: &str = "The current user\u{2019}s role must have user editing capabilities.";

fn field(name: &str, label: &str, kind: FieldKindDef, required: bool) -> FieldDef {
    FieldDef {
        name: name.into(),
        label: label.into(),
        kind,
        required,
    }
}

fn column(key: &str, label: &str, sortable: bool, searchable: bool) -> ColumnDef {
    ColumnDef {
        key: key.into(),
        label: label.into(),
        sortable,
        searchable,
    }
}

fn text_of<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The role names of a row's `roles`: the JSON list the edit form's row has.
fn role_names(row: &Row) -> Vec<String> {
    match row.get("roles") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The Users screen: Ignite's, with WordPress's fields, columns, views and actions. See the
/// [module](self).
pub(crate) fn users_screen() -> RuntimeResource {
    let mut resource = users_resource();
    resource.title = "Users".to_owned();
    resource.data = Arc::new(McUsersData);
    resource.fields = vec![
        field("name", "Username", FieldKindDef::Text, true),
        field("email", "Email", FieldKindDef::Email, true),
        field(FIRST_NAME, "First Name", FieldKindDef::Text, false),
        field(LAST_NAME, "Last Name", FieldKindDef::Text, false),
        field(URL, "Website", FieldKindDef::Url, false),
        field("password", "Password", FieldKindDef::Password, true),
        field(ROLE, "Role", FieldKindDef::Lookup, false),
    ];
    resource.columns = vec![
        column("name", "Username", true, true),
        column(DISPLAY_NAME, "Name", false, false),
        column("email", "Email", true, true),
        column("roles", "Role", false, false),
        column(POSTS, "Posts", false, false),
    ];
    // WordPress's row actions: Delete stays Ignite's (it refuses the user's own account);
    // Disable and Enable are not WordPress's.
    let delete: Vec<RowAction> = resource
        .row_actions
        .drain(..)
        .filter(|action| action.name() == "delete")
        .collect();
    resource.row_actions = WORDPRESS_ROLES
        .into_iter()
        .map(change_role_action)
        .collect();
    resource.row_actions.extend(delete);
    let mut resource = resource
        .navigation_group(navigation::SITE)
        .navigation_sort(navigation::USERS)
        .navigation_icon(navigation::USERS_ICON)
        .navigation_create_item(true)
        .navigation_create_label(ADD_NEW_USER)
        // "Usernames cannot be changed." (`user-edit.php`).
        .disabled_on("name", &[Operation::Edit]);
    resource = ScreenCaps {
        view: "list_users",
        create: "create_users",
        update: "edit_users",
        delete: "delete_users",
    }
    .apply(resource);
    resource
}

/// "Change role to <Role>", WordPress's `promote` action of the Users list for one row.
fn change_role_action(role: &'static str) -> RowAction {
    // The role's WordPress name: Ignite's labels are installed when the panel is built.
    let label = default_role(role).map_or_else(|| role.to_owned(), |r| r.display_name);
    let shown = label.clone();
    RowAction::make(format!("role-{role}"))
        .label(format!("Change role to {label}"))
        .color(BadgeColor::Gray)
        .hidden(move |row| text_of(row, "roles") == shown)
        .action(move |db, user, id, _| Box::pin(change_role(db, user, id, role)))
        .success_notification_title("Changed roles.")
}

/// Gives user `id` the role `role` in place of their WordPress role, as `users.php`'s `promote`
/// does (`WP_User::set_role`): only for a user with `promote_users`, never so that the current
/// user loses it, and a super-admin only by a super-admin, who keeps the role.
async fn change_role(
    db: &Database,
    user: &ActingUser,
    id: i64,
    role: &str,
) -> Result<Result<(), String>, Error> {
    if !user.can("promote_users") {
        return Ok(Err(NOT_ALLOWED_ROLE.to_owned()));
    }
    let current = authz::roles_of(db, id).await.map_err(database_error)?;
    let super_admin = current.iter().any(|r| r == IGNITE_SUPER_ADMIN);
    if super_admin && !user.is_super_admin() {
        return Ok(Err(
            "Only a super-admin can change a super-admin.".to_owned()
        ));
    }
    if id == user.id() && !user.is_super_admin() {
        let caps = authz::role_permissions(db, role)
            .await
            .map_err(database_error)?;
        if !caps.iter().any(|cap| cap == "promote_users") {
            return Ok(Err(OWN_ROLE.to_owned()));
        }
    }
    let mut roles = vec![role.to_owned()];
    if super_admin {
        roles.push(IGNITE_SUPER_ADMIN.to_owned());
    }
    authz::sync_user_roles(db, id, &roles)
        .await
        .map_err(database_error)?;
    Ok(Ok(()))
}

/// The Roles screen: Ignite's roles and their abilities, which are WordPress's capabilities
/// here, under Users. Changing a role needs `promote_users`, as `users.php` asks for it.
pub(crate) fn roles_screen() -> RuntimeResource {
    let resource = roles_resource()
        .navigation_group(navigation::SITE)
        .navigation_parent_item(USERS_SLUG)
        .navigation_sort(2)
        .navigation_create_item(false);
    ScreenCaps::all("promote_users").apply(resource)
}

/// The "Your Profile" entry under Users: a link to the panel's profile page, which every user
/// with `read` may open (`profile.php`). Its list address redirects there ([`routes`]).
pub(crate) fn profile_entry() -> RuntimeResource {
    let resource = RuntimeResource::new(PROFILE_SLUG, "Your Profile", "Profile", ProfileLink)
        .navigation_group(navigation::SITE)
        .navigation_parent_item(USERS_SLUG)
        .navigation_sort(1)
        .navigation_create_item(false);
    ScreenCaps {
        view: "read",
        create: "do_not_allow",
        update: "do_not_allow",
        delete: "do_not_allow",
    }
    .apply(resource)
}

/// The data of the "Your Profile" entry: none. Its list address is a redirect.
#[derive(Debug, Clone, Copy)]
struct ProfileLink;

impl RuntimeData for ProfileLink {
    fn list<'a>(&'a self, _: &'a Database, _: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async {
            Ok(Page {
                rows: Vec::new(),
                total: 0,
            })
        })
    }

    fn find<'a>(&'a self, _: &'a Database, _: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async { Ok(None) })
    }

    fn create<'a>(&'a self, _: &'a Database, _: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async { Err(Error::NotFound { table: "users" }) })
    }

    fn update<'a>(&'a self, _: &'a Database, _: i64, _: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async { Err(Error::NotFound { table: "users" }) })
    }

    fn delete<'a>(&'a self, _: &'a Database, _: i64) -> RuntimeFuture<'a, u64> {
        Box::pin(async { Ok(0) })
    }
}

/// The route of "Your Profile": a redirect to the panel's profile page, before Ignite's list
/// and the settings screens' list address.
pub(crate) fn routes() -> Vec<Route> {
    let mut routes = rocket::routes![your_profile];
    for route in &mut routes {
        route.rank = super::SETTINGS_RANK - 1;
    }
    routes
}

#[rocket::get("/your-profile")]
fn your_profile() -> Redirect {
    Redirect::to(format!("{ADMIN_PATH}/profile"))
}

/// An error of Ignite's role functions as a database error.
fn database_error(error: authz::AuthzError) -> Error {
    match error {
        authz::AuthzError::Database(error) => error,
        other => Error::Database(orbit_orm::__sqlx::Error::Protocol(other.to_string())),
    }
}

/// The Users screen's rows: Ignite's [`UsersData`], with the first and last names, the website,
/// the role and the number of published posts. See the [module](self).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct McUsersData;

impl McUsersData {
    /// The `first_name`, `last_name` and `user_url` meta of users `ids`.
    async fn meta(db: &Database, ids: &[i64]) -> Result<HashMap<(i64, String), String>, Error> {
        let mut meta = HashMap::new();
        if ids.is_empty() {
            return Ok(meta);
        }
        let mut binds: Vec<Sql> = ids.iter().map(|id| Sql::from(*id)).collect();
        for key in [FIRST_NAME, LAST_NAME, USER_URL] {
            binds.push(key.into());
        }
        let rows: Vec<(i64, String, String)> = raw::fetch_all(
            db,
            &format!(
                "SELECT user_id, meta_key, meta_value FROM user_meta WHERE user_id IN ({}) \
                 AND meta_key IN (?, ?, ?) ORDER BY id",
                vec!["?"; ids.len()].join(", ")
            ),
            &binds,
        )
        .await?;
        for (id, key, value) in rows {
            if let Ok(Value::String(text)) = serde_json::from_str(&value) {
                meta.insert((id, key), text);
            }
        }
        Ok(meta)
    }

    /// The published posts of users `ids`, as `count_many_users_posts`.
    async fn post_counts(db: &Database, ids: &[i64]) -> Result<HashMap<i64, i64>, Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut binds: Vec<Sql> = vec!["post".into(), "publish".into()];
        binds.extend(ids.iter().map(|id| Sql::from(*id)));
        let rows: Vec<(i64, i64)> = raw::fetch_all(
            db,
            &format!(
                "SELECT author_id, COUNT(*) FROM posts WHERE post_type = ? AND status = ? \
                 AND author_id IN ({}) GROUP BY author_id",
                vec!["?"; ids.len()].join(", ")
            ),
            &binds,
        )
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// `row` with the user's first and last names, website and role, for the edit form.
    async fn with_profile(db: &Database, mut row: Row) -> Result<Row, Error> {
        let Some(id) = row.get("id").and_then(Value::as_i64) else {
            return Ok(row);
        };
        let meta = Self::meta(db, &[id]).await?;
        let get = |key: &str| meta.get(&(id, key.to_owned())).cloned().unwrap_or_default();
        row.insert(FIRST_NAME.into(), json!(get(FIRST_NAME)));
        row.insert(LAST_NAME.into(), json!(get(LAST_NAME)));
        row.insert(URL.into(), json!(get(USER_URL)));
        let role = role_names(&row)
            .into_iter()
            .find(|role| role != IGNITE_SUPER_ADMIN)
            .unwrap_or_default();
        row.insert(ROLE.into(), json!(role));
        Ok(row)
    }

    /// Stores the form's first and last names and website for user `id`.
    async fn save_profile(db: &Database, id: i64, input: &Row) -> Result<(), Error> {
        for (field, key) in [
            (FIRST_NAME, FIRST_NAME),
            (LAST_NAME, LAST_NAME),
            (URL, USER_URL),
        ] {
            if let Some(Value::String(text)) = input.get(field) {
                set_user_meta(db, id, key, &json!(text.trim()))
                    .await
                    .map_err(|e| match e {
                        mission_control_core::content::MetaError::Database(e) => e,
                        mission_control_core::content::MetaError::InvalidJson(e) => {
                            Error::Database(orbit_orm::__sqlx::Error::Protocol(e.to_string()))
                        }
                    })?;
            }
        }
        Ok(())
    }

    /// Whether another user than `id` has the username `name`.
    async fn username_taken(db: &Database, name: &str, id: i64) -> Result<bool, Error> {
        let n: i64 = raw::fetch_scalar(
            db,
            "SELECT COUNT(*) FROM users WHERE name = ? AND id <> ?",
            &[name.trim().into(), id.into()],
        )
        .await?;
        Ok(n > 0)
    }

    /// The form's `role` as Ignite's `roles`: the chosen role, and `super-admin` when the user
    /// `target` holds it (it is not a WordPress role, so the select does not offer it).
    async fn roles_input(db: &Database, input: &mut Row, target: Option<i64>) -> Result<(), Error> {
        let Some(role) = input.remove(ROLE) else {
            return Ok(());
        };
        let mut roles: Vec<String> = role
            .as_str()
            .filter(|role| !role.is_empty())
            .map(str::to_owned)
            .into_iter()
            .collect();
        if let Some(id) = target {
            let current = authz::roles_of(db, id).await.map_err(database_error)?;
            if current.iter().any(|r| r == IGNITE_SUPER_ADMIN) {
                roles.push(IGNITE_SUPER_ADMIN.to_owned());
            }
        }
        input.insert("roles".into(), json!(roles));
        Ok(())
    }

    /// The roles the Role select offers: WordPress's five in their order, then the others, but
    /// not Ignite's `super-admin`.
    async fn role_choices(db: &Database) -> Result<Vec<(String, String)>, Error> {
        let stored = authz::roles(db).await.map_err(database_error)?;
        let mut names: Vec<String> = WORDPRESS_ROLES
            .iter()
            .filter(|role| stored.iter().any(|s| s == *role))
            .map(|role| (*role).to_owned())
            .collect();
        for role in stored {
            if role != IGNITE_SUPER_ADMIN && !names.contains(&role) {
                names.push(role);
            }
        }
        Ok(names
            .into_iter()
            .map(|role| {
                let label = role_label(&role);
                (role, label)
            })
            .collect())
    }
}

impl RuntimeData for McUsersData {
    fn list<'a>(&'a self, db: &'a Database, query: &'a ListQuery) -> RuntimeFuture<'a, Page<Row>> {
        Box::pin(async move {
            let mut page = UsersData.list(db, query).await?;
            let ids: Vec<i64> = page
                .rows
                .iter()
                .filter_map(|row| row.get("id").and_then(Value::as_i64))
                .collect();
            let meta = Self::meta(db, &ids).await?;
            let posts = Self::post_counts(db, &ids).await?;
            for row in &mut page.rows {
                let Some(id) = row.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let get = |key: &str| meta.get(&(id, key.to_owned())).cloned().unwrap_or_default();
                let name = format!("{} {}", get(FIRST_NAME), get(LAST_NAME));
                row.insert(DISPLAY_NAME.into(), json!(name.trim()));
                row.insert(POSTS.into(), json!(posts.get(&id).copied().unwrap_or(0)));
            }
            Ok(page)
        })
    }

    fn find<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move {
            match UsersData.find(db, id).await? {
                Some(row) => Ok(Some(Self::with_profile(db, row).await?)),
                None => Ok(None),
            }
        })
    }

    fn create<'a>(&'a self, db: &'a Database, mut input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            Self::roles_input(db, &mut input, None).await?;
            let row = UsersData.create(db, input.clone()).await?;
            let id = row.get("id").and_then(Value::as_i64).unwrap_or_default();
            Self::save_profile(db, id, &input).await?;
            Self::with_profile(db, row).await
        })
    }

    fn update<'a>(&'a self, db: &'a Database, id: i64, mut input: Row) -> RuntimeFuture<'a, Row> {
        Box::pin(async move {
            Self::roles_input(db, &mut input, Some(id)).await?;
            let row = UsersData.update(db, id, input.clone()).await?;
            Self::save_profile(db, id, &input).await?;
            Self::with_profile(db, row).await
        })
    }

    fn delete<'a>(&'a self, db: &'a Database, id: i64) -> RuntimeFuture<'a, u64> {
        UsersData.delete(db, id)
    }

    fn options<'a>(
        &'a self,
        db: &'a Database,
        field: &'a str,
    ) -> RuntimeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            if field == ROLE {
                Self::role_choices(db).await
            } else {
                Ok(Vec::new())
            }
        })
    }

    fn find_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
    ) -> RuntimeFuture<'a, Option<Row>> {
        Box::pin(async move {
            match UsersData.find_for(db, user, id).await? {
                Some(row) => Ok(Some(Self::with_profile(db, row).await?)),
                None => Ok(None),
            }
        })
    }

    fn try_create_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        mut input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            if Self::username_taken(db, text_of(&input, "name"), 0).await? {
                return Ok(Err(ValidationError::with_messages([(
                    "name",
                    USERNAME_TAKEN,
                )])));
            }
            Self::roles_input(db, &mut input, None).await?;
            let row = match UsersData.try_create_for(db, user, input.clone()).await? {
                Ok(row) => row,
                Err(refused) => return Ok(Err(refused)),
            };
            let id = row.get("id").and_then(Value::as_i64).unwrap_or_default();
            Self::save_profile(db, id, &input).await?;
            Ok(Ok(Self::with_profile(db, row).await?))
        })
    }

    fn try_update_for<'a>(
        &'a self,
        db: &'a Database,
        user: &'a ActingUser,
        id: i64,
        mut input: Row,
    ) -> RuntimeFuture<'a, Result<Row, ValidationError>> {
        Box::pin(async move {
            // The username is not changed (the field is disabled on the edit form).
            input.remove("name");
            Self::roles_input(db, &mut input, Some(id)).await?;
            let row = match UsersData
                .try_update_for(db, user, id, input.clone())
                .await?
            {
                Ok(row) => row,
                Err(refused) => return Ok(Err(refused)),
            };
            Self::save_profile(db, id, &input).await?;
            Ok(Ok(Self::with_profile(db, row).await?))
        })
    }

    fn tabs_for<'a>(
        &'a self,
        db: &'a Database,
        _user: &'a ActingUser,
    ) -> RuntimeFuture<'a, Option<Vec<Tab>>> {
        Box::pin(async move {
            // `WP_Users_List_Table::get_views`: All, then each role with users, by its label.
            let mut tabs = vec![Tab::make("all").label("All")];
            for (role, label) in Self::role_choices(db).await? {
                if role == "all" {
                    continue;
                }
                tabs.push(Tab::make(role.clone()).label(label).where_eq("role", role));
            }
            Ok(Some(tabs))
        })
    }

    fn validate_form(&self, form: &FormInput, operation: Operation) -> FieldErrors {
        UsersData.validate_form(form, operation)
    }
}

/// WordPress's "Generate password" button beside the password of Add New User and Edit User
/// (`user-new.php`'s `wp-generate-pw`): it fills the field with a new 24-character password,
/// shown, as `wp_generate_password( 24 )` does.
pub(crate) struct UserFormViews;

/// The button and its script, put after the password input.
const GENERATE_PASSWORD: &str = "<button type=\"button\" class=\"ig-btn button wp-generate-pw\" \
data-mc-generate-password>Generate password</button><script>(function(){var b=document.\
querySelector('[data-mc-generate-password]');if(!b)return;b.addEventListener('click',\
function(){var c='abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*()',\
r=new Uint32Array(24),p='';crypto.getRandomValues(r);for(var i=0;i<r.length;i++)p+=c[r[i]%c.\
length];var f=document.getElementById('password');f.type='text';f.value=p;f.focus();});})();\
</script>";

/// The password input the panel renders.
const PASSWORD_INPUT: &str = "id=\"password\" name=\"password\" type=\"password\"";

/// The form with the "Generate password" button after its password input, or `None` when it
/// has no password input.
fn with_generate_password(html: &str) -> Option<String> {
    let start = html.find(PASSWORD_INPUT)?;
    let end = start + html[start..].find('>')? + 1;
    Some(format!(
        "{}{GENERATE_PASSWORD}{}",
        &html[..end],
        &html[end..]
    ))
}

#[rocket::async_trait]
impl Fairing for UserFormViews {
    fn info(&self) -> Info {
        Info {
            name: "Mission Control user forms",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        if request.method() != Method::Get
            || response.status() != Status::Ok
            || response.content_type() != Some(ContentType::HTML)
        {
            return;
        }
        let base = format!("{ADMIN_PATH}/{USERS_SLUG}/");
        let Some(rest) = request.uri().path().as_str().strip_prefix(&base) else {
            return;
        };
        let form = rest == "new"
            || rest
                .strip_suffix("/edit")
                .is_some_and(|id| id.parse::<i64>().is_ok());
        if !form {
            return;
        }
        let Ok(html) = response.body_mut().to_string().await else {
            return;
        };
        let html = with_generate_password(&html).unwrap_or(html);
        response.set_sized_body(html.len(), Cursor::new(html));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_gets_wordpresss_generate_button() {
        let html = "<input class=\"ig-input\" id=\"password\" name=\"password\" type=\"password\" value=\"\"><p>x</p>";
        let out = with_generate_password(html).unwrap();
        assert!(out.starts_with(
            "<input class=\"ig-input\" id=\"password\" name=\"password\" type=\"password\" value=\"\"><button"
        ));
        assert!(out.contains(">Generate password</button>"));
        assert!(out.ends_with("</script><p>x</p>"));
        assert_eq!(with_generate_password("<p>no form</p>"), None);
    }

    #[test]
    fn the_screens_check_wordpresss_capabilities() {
        use ignite_panel::ignite_core::{RegisteredResource, ResourceAction};
        let users = RegisteredResource::new(users_screen()).unwrap();
        assert_eq!(users.ability_for(ResourceAction::ViewAny), "list_users");
        assert_eq!(users.ability_for(ResourceAction::Create), "create_users");
        assert_eq!(users.ability_for(ResourceAction::Update), "edit_users");
        assert_eq!(users.ability_for(ResourceAction::Delete), "delete_users");
        let roles = RegisteredResource::new(roles_screen()).unwrap();
        assert_eq!(roles.ability_for(ResourceAction::ViewAny), "promote_users");
        assert_eq!(roles.ability_for(ResourceAction::Update), "promote_users");
    }
}
