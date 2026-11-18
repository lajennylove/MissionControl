# MissionControl

Mission Control: a WordPress-compatible CMS in Rust, built on Rocket with Propel, Ignite and Orbit.

Part of the RocketBoost family: [Propel](https://github.com/lajennylove/Propel) (templates), [Ignite](https://github.com/lajennylove/Ignite) (admin panel) and [Orbit](https://github.com/lajennylove/Orbit) (ORM).

Work in progress.

## Crates

- `mission-control`: the library entry point; builds the Rocket application for the prebuilt binary and for a site's own binary.
- `mission-control-core`: content, users, hooks and configuration.
- `mission-control-app`: the prebuilt binary; adds setup mode and restart on top of the library.
- `mission-control-testing`: shared test helpers, SQLite and MariaDB test database setup.

## Restart after setup

A site with no `.env` starts in setup mode. When setup finishes, the binary starts again in normal mode:

- **Started by hand:** it shuts down gracefully and replaces itself (`exec`) with the same arguments and environment.
- **Under systemd or a container:** it shuts down gracefully and exits with status 0, and the supervisor starts it again. The restart policy must cover a clean exit: `Restart=always` for systemd (`on-failure` does not restart status 0), `--restart unless-stopped` or `always` for Docker.

If `.env` is broken after setup (for example the database is unreachable), the restarted process stops with an error, and under `Restart=always` or Docker `always` the supervisor starts it again and again: a crash loop. Read the log (`journalctl -u <unit>` or `docker logs`), fix `.env`, and the next start works.

Mission Control detects systemd (`INVOCATION_ID` or `NOTIFY_SOCKET`) and containers (`/.dockerenv`, `/run/.containerenv` or `container`). Set `MC_RESTART=exec` or `MC_RESTART=exit` to choose yourself.

## Deploy

Both ways start in setup mode when there is no `.env`: open `/mc-admin/install.php`, finish the wizard, and the site restarts into normal mode. The restart needs a policy that covers a clean exit (see above). Neither way holds a secret: `DATABASE_URL` and `IGNITE_SECRET_KEY` live in the install folder's `.env`.

- **systemd:** `deploy/mission-control.service` (`Restart=always`, `RestartSec=2`, runs as the `mission-control` user, read-only file system except the install folder). The binary is installed root-owned at `/usr/local/bin/mission-control-app`; only the install folder is writable for the service. The install steps are in the file's header. CI runs `systemd-analyze verify` on it.
- **Docker:** `docker build -t mission-control .`, then `docker run -d --restart unless-stopped -p 8000:8000 -v mission-control-data:/data mission-control`, or `docker compose -f deploy/docker-compose.yml up -d`. `/data` is the install folder (`.env`, `database.sqlite`, `uploads/`, `themes/`); the default theme is copied there on first start. The container runs as uid 10001, so a bind mount must be writable for it (`chown 10001:10001 ./data`). Setup mode is open to the first visitor, so finish setup right after the first start, or publish the port on loopback only until then (`-p 127.0.0.1:8000:8000`, as the Compose file does). To pass settings instead of using the wizard, use `--env-file` with a git-ignored file, not `-e`. The image listens on `0.0.0.0:8000` (`ROCKET_ADDRESS`, `ROCKET_PORT`); put a reverse proxy with TLS in front.

## Admin

The admin is Ignite's panel at `/mc-admin`: sign in at `/mc-admin/login`. Posts are at `/mc-admin/post`, pages at `/mc-admin/page`, and each custom post type the admin shows at `/mc-admin/<post type>`. Ignite also answers a health check at `/up`.

- **MySQL or MariaDB only, for now.** Ignite 0.6.1 signs users in and keeps sessions on MySQL and MariaDB only. On SQLite and PostgreSQL, `/mc-admin` and everything under it answer a 503 page that says so; the site and the REST API work as usual.
- **One database.** Ignite reads `DATABASE_URL` and `IGNITE_SECRET_KEY` from the environment or `.env` itself. They must be the site's: when `DATABASE_URL` is not the site's database, the admin answers the 503 page and the log says why.
- **Who can use the post and page screens:** as in WordPress, by capability. Roles are Ignite roles named after WordPress's (`administrator`, `editor`, `author`, `contributor`, `subscriber`; Ignite's `super-admin` counts as an administrator), created when the site starts. Administrators and editors see and edit every post and page. Authors and contributors see and edit only their own posts (not pages); an author publishes, a contributor's Published is saved as Pending, and their HTML is filtered by kses, as WordPress does for users without `unfiltered_html`. Subscribers reach the dashboard and their profile only. Another user's post is not found (404) for a user who may not edit it, and an action the user may not take is refused (403).
- **Post locks:** as in WordPress, opening a post's edit form locks it for 150 seconds, renewed every minute while the form is open (the editor's heartbeat) and released when it is left. Another user who opens it is told who is editing and may take over; the first editor's form then says so and can no longer be saved. The heartbeat runs on forms with the rich text editor.
- **Page templates:** as in WordPress, a template file in the theme's folder or one folder below it is a page template when its header has a `Template Name:` line, written in a Propel comment; the page form offers them with "Default template". A `Template Post Type:` line that does not list `page` keeps it off the page form.

  ```blade
  {{-- Template Name: Full width --}}
  @extends('layouts.app')
  ```

- **The editor:** rich text fields, the post content included, use WordPress's classic editor: TinyMCE 7.9.3 with WordPress's toolbar, the Visual and Text tabs (Text is the HTML itself) and Add Media. Add Media opens a small media picker: upload a file, or search and pick one from the media library, choose an image's size, alignment and alternative text, and insert it with WordPress's markup (`<img ... class="alignnone size-medium wp-image-<id>">`, or a link for other files); an image with a caption goes in WordPress's `[caption]` shortcode, which the site shows as a `<figure class="wp-caption">` with its `<figcaption>`. The editor's files are compiled into the binary and served at `/mc-includes/js/editor.js` and `/mc-includes/js/tinymce/`; nothing is loaded from another site. The post screens store the HTML as sent for users with `unfiltered_html` (administrators and editors), and through kses for the others. The Visual tab, as in WordPress, cleans the HTML it edits; switch to Text for markup it would change.
- **The media library** is the `attachment` post type's screen, at `/mc-admin/attachment`, for administrators and editors (WordPress's Media › Library: the grid of thumbnails by default, with a search, Load more and the attachment details panel; `?mode=list` opens the list). It lists every file with its title, file name, type, author and upload date, searches titles, captions, descriptions and file names, edits an item's title, alternative text, caption and description, and Delete removes the item for good with its file and its resized copies (WordPress keeps no trash for media by default). Add New (`/mc-admin/attachment/new`) is a plain upload form. Files go through the same checks and size limit (`MC_UPLOAD_MAX_BYTES`) as the REST API and are served from `/mc-content/uploads/`. The editor's picker uploads to `/mc-admin/async-upload.php` and lists from `/mc-admin/admin-ajax.php?action=query-attachments`, with the admin session and its CSRF token; any user with `upload_files` (authors too) may use those. Image, file and gallery fields (the featured image included) take an item's id, show the item, and refuse an id that is not one.
- **Settings**, for administrators (`manage_options`): WordPress's General, Writing, Reading, Discussion, Media, Permalinks and Privacy screens, at `/mc-admin/options-general`, `/mc-admin/options-writing`, `/mc-admin/options-reading`, `/mc-admin/options-discussion`, `/mc-admin/options-media`, `/mc-admin/options-permalink` and `/mc-admin/options-privacy`. Each saves WordPress's options, cleaned by the same rules as the REST API's `/mc-json/mc/v2/settings` (WordPress's `sanitize_option`); a value those rules refuse (a custom permalink structure without a `%tag%`, an address that is not `http` or `https`) is not saved and the old one stays, as in WordPress. A `UTC+N` time zone is saved as an offset, as WordPress's General screen saves it. The Permalinks screen's structure, category base and tag base apply from the next request; the old `?p=123` addresses redirect to the new ones. The Media sizes apply to the next upload. Options of features Mission Control does not have yet (membership, site language, post formats, post by email, update services) are not on the screens.
- **Themes, site title and tagline:** `site.name` and `site.description` in a template are plain text. Print them with `{{ }}`, which escapes them, never raw. The settings store them escaped, as WordPress does (`Tom &amp; Jerry`), and the site decodes them before the template escapes them, so they show once escaped.
- **Menus**, for administrators (`edit_theme_options`), at `/mc-admin/nav-menus`: each menu's name, the theme locations that show it (one checkbox per location the active theme declares in `[menus]`), and its items. The items are edited as rows of JSON for now, one object per item, in order: `title`, `type` (`custom`, a post type such as `page` or `post`, or a taxonomy such as `category` or `post_tag`), `object_id` (the post or term id) or `url` (a custom link), `parent` (the parent's row number, 1 for the first row, 0 for none), `classes`, `new_tab`, and `id` to keep an existing item. Rows left out are deleted on save. Drag-and-drop ordering is a follow-up.

### The first administrator (MySQL or MariaDB)

`mission-control-app ignite <command>` runs Ignite's command line with the site's settings, as Laravel's `artisan` does. Run it in the install folder, where `.env` is, after the installer has finished:

```sh
mission-control-app ignite role create administrator   # only before the site's first start
mission-control-app ignite user create --email owner@example.com --name "Site Owner"
mission-control-app ignite user role add owner@example.com administrator
```

`user create` asks for the password twice without echoing it (or reads it from standard input with `--password-stdin`). The site creates the five roles each time it starts, so `role create` is only needed before the first start. `mission-control-app ignite` with no command lists the others (`user roles`, `user password`, `user disable`, ...). The installer has already run Ignite's migrations, so `ignite migrate` has nothing to do.

## License

GPL-2.0-or-later, like WordPress. See `LICENSE`. Copyright (c) 2026 Jenny Martinez.

The admin's editor bundles TinyMCE 7.9.3 (GPL-2.0-or-later) in `assets/editor/tinymce/`. Its version, licence, source and checksums are in `NOTICE`.
