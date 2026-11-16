//! Loading a theme from its folder, its parent theme, and the Propel loader that renders it.
//!
//! This follows WordPress's `WP_Theme`: the folder name is the slug, a child theme names its
//! parent's slug in `template` and the parent is the sibling folder of that name, one level
//! only. Templates are found child first, then parent, as `locate_template`.
//!
//! Templates are rendered by Propel from a confined namespace per theme: `@extends` and
//! `@include` resolve only inside the theme and its parent, never elsewhere on disk.

use std::fmt;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use propel_runtime::{FileLoader, Loader, RenderError};

use super::manifest::{
    MANIFEST_FILE, MAX_MANIFEST_BYTES, Manifest, MenuLocation, PageTemplate, Sidebar,
};

/// The file extension of a Propel template.
const VIEW_EXTENSION: &str = "propel";

/// A theme loaded from its folder, with its parent theme for a child theme.
#[derive(Debug, Clone)]
pub struct Theme {
    slug: String,
    dir: PathBuf,
    manifest: Manifest,
    parent: Option<Box<Theme>>,
}

/// A template found by [`Theme::locate_template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedTemplate {
    /// The template hierarchy name that matched, such as `single-post` or `templates/wide`.
    pub name: String,
    /// The slug of the theme that has it: the child, or its parent.
    pub theme: String,
    /// The Propel view to render with [`Theme::loader`], such as `my-theme::templates.wide`.
    pub view: String,
    /// The template file, with symbolic links resolved.
    pub path: PathBuf,
}

/// The Propel loader for a theme, from [`Theme::loader`].
///
/// It renders only the theme's namespaces (`slug::view`): an unqualified top-level name is
/// refused. Rendering starts with the child theme active, so a parent template that includes a
/// partial picks up the child's copy first, as WordPress's `get_template_part` does.
#[derive(Debug, Clone)]
pub struct ThemeLoader {
    files: FileLoader,
    active: String,
}

/// Why a theme could not be loaded. Each message says what to fix.
#[derive(Debug)]
pub enum ThemeError {
    /// A file or folder could not be read.
    Read {
        /// The file or folder.
        path: PathBuf,
        /// The error from the file system.
        source: io::Error,
    },
    /// The theme's folder name cannot be used as a slug.
    InvalidSlug {
        /// The theme's folder.
        dir: PathBuf,
    },
    /// The folder has no `theme.toml` (WordPress: "Stylesheet is missing.").
    MissingManifest {
        /// The theme's folder.
        dir: PathBuf,
    },
    /// `theme.toml` is not a regular file in the theme's folder: a symbolic link, a FIFO, a
    /// device or a folder. It is refused before it is opened.
    ManifestNotAFile {
        /// The manifest path.
        path: PathBuf,
    },
    /// `theme.toml` is over [`MAX_MANIFEST_BYTES`].
    ManifestTooLarge {
        /// The manifest file.
        path: PathBuf,
        /// Its size, in bytes.
        len: u64,
    },
    /// `theme.toml` is not valid: bad TOML, an unknown key, an unknown `supports` value or a
    /// value of the wrong type.
    InvalidManifest {
        /// The manifest file.
        path: PathBuf,
        /// What is wrong and where (line and column). It may name a key or a `supports` value
        /// written as a slug, but never echoes any other value from the file.
        message: String,
    },
    /// `theme.toml` has no `name`, or an empty one.
    MissingName {
        /// The manifest file.
        path: PathBuf,
    },
    /// A theme without a parent has no `index` template (WordPress: "Template is missing.").
    MissingIndex {
        /// The theme's slug.
        slug: String,
    },
    /// `template` is not a theme slug, so it cannot name a sibling folder.
    InvalidParentSlug {
        /// The child theme's slug.
        slug: String,
        /// The value of `template`.
        template: String,
    },
    /// The theme names itself as its parent.
    OwnParent {
        /// The theme's slug.
        slug: String,
    },
    /// The parent theme is not installed.
    MissingParent {
        /// The child theme's slug.
        slug: String,
        /// The parent theme's slug.
        parent: String,
    },
    /// The parent theme is itself a child theme. One level of parent is allowed, as WordPress.
    InvalidParent {
        /// The child theme's slug.
        slug: String,
        /// The parent theme's slug.
        parent: String,
    },
}

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeError::Read { path, source } => {
                write!(f, "Could not read `{}`: {source}", path.display())
            }
            ThemeError::InvalidSlug { dir } => write!(
                f,
                "The theme folder `{}` cannot be used as a theme slug. Rename it with letters, \
                 digits, `-` and `_` only.",
                dir.display()
            ),
            ThemeError::MissingManifest { dir } => write!(
                f,
                "The theme in `{}` has no {MANIFEST_FILE}. Add a {MANIFEST_FILE} with at least a \
                 `name`.",
                dir.display()
            ),
            ThemeError::ManifestNotAFile { path } => write!(
                f,
                "`{}` is not a regular file. Replace it with a {MANIFEST_FILE} file in the \
                 theme's folder, not a link or a special file.",
                path.display()
            ),
            ThemeError::ManifestTooLarge { path, len } => write!(
                f,
                "`{}` is {len} bytes, over the {MAX_MANIFEST_BYTES}-byte limit for a theme \
                 manifest. Keep only the theme's details in it.",
                path.display()
            ),
            ThemeError::InvalidManifest { path, message } => write!(
                f,
                "`{}` is not a valid theme manifest: {message}. Fix it and load the theme again.",
                path.display()
            ),
            ThemeError::MissingName { path } => write!(
                f,
                "The theme manifest `{}` has no `name`. Add a line such as \
                 `name = \"My Theme\"`.",
                path.display()
            ),
            ThemeError::MissingIndex { slug } => write!(
                f,
                "Template is missing. The theme `{slug}` has no parent theme, so it needs an \
                 index.{VIEW_EXTENSION} template."
            ),
            ThemeError::InvalidParentSlug { slug, template } => write!(
                f,
                "The theme `{slug}` names `{template}` as its parent theme, which is not a theme \
                 slug. Set `template` to the parent theme's folder name."
            ),
            ThemeError::OwnParent { slug } => write!(
                f,
                "The theme `{slug}` defines itself as its parent theme. Please check `template` \
                 in its {MANIFEST_FILE}."
            ),
            ThemeError::MissingParent { slug, parent } => write!(
                f,
                "The parent theme of `{slug}` is missing. Please install the `{parent}` parent \
                 theme."
            ),
            ThemeError::InvalidParent { slug, parent } => write!(
                f,
                "The `{parent}` theme is not a valid parent theme for `{slug}`: it is itself a \
                 child theme. Make `{slug}` a child of `{parent}`'s parent instead."
            ),
        }
    }
}

impl std::error::Error for ThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ThemeError::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Loads the theme in `dir`, and its parent for a child theme.
///
/// The slug is `dir`'s folder name. The manifest is read and checked; a theme without a parent
/// must have an `index` template. A child's parent is the sibling folder named by `template`; it
/// must exist, must not be the child itself and must not be a child theme.
pub fn load_theme(dir: &Path) -> Result<Theme, ThemeError> {
    // `.` components are dropped, so `themes/mine/.` is the theme `mine`.
    let dir: PathBuf = dir.components().collect();
    let slug = match dir.components().next_back() {
        Some(Component::Normal(name)) => name.to_str().filter(|name| is_slug(name)),
        _ => None,
    }
    .ok_or_else(|| ThemeError::InvalidSlug { dir: dir.clone() })?
    .to_owned();

    let mut theme = read_theme(slug, dir)?;
    let Some(template) = theme.manifest.template.clone() else {
        theme.require_index()?;
        return Ok(theme);
    };
    if !is_slug(&template) {
        return Err(ThemeError::InvalidParentSlug {
            slug: theme.slug,
            template,
        });
    }
    if template == theme.slug {
        return Err(ThemeError::OwnParent { slug: theme.slug });
    }

    let missing = || ThemeError::MissingParent {
        slug: theme.slug.clone(),
        parent: template.clone(),
    };
    // `template` is a single safe segment, so this is a sibling of the child's folder.
    let parent_dir = theme.dir.with_file_name(&template);
    if !parent_dir.is_dir() {
        return Err(missing());
    }
    let parent = match read_theme(template.clone(), parent_dir) {
        Err(ThemeError::MissingManifest { .. }) => return Err(missing()),
        other => other?,
    };
    if parent.manifest.template.is_some() {
        return Err(ThemeError::InvalidParent {
            slug: theme.slug,
            parent: template,
        });
    }
    parent.require_index()?;
    theme.parent = Some(Box::new(parent));
    Ok(theme)
}

/// Reads a theme's manifest, without looking at its parent.
fn read_theme(slug: String, dir: PathBuf) -> Result<Theme, ThemeError> {
    let path = dir.join(MANIFEST_FILE);
    let read_error = |source| ThemeError::Read {
        path: path.clone(),
        source,
    };
    // Checked before opening: following a link would read a file outside the theme, and opening
    // a FIFO for reading blocks until a writer comes.
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(ThemeError::ManifestNotAFile { path }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ThemeError::MissingManifest { dir });
        }
        Err(error) => return Err(read_error(error)),
    }
    let file = match open_manifest(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ThemeError::MissingManifest { dir });
        }
        // `O_NOFOLLOW` refuses a link swapped in after the check with `ELOOP`.
        Err(error) if error.raw_os_error() == Some(ELOOP) => {
            return Err(ThemeError::ManifestNotAFile { path });
        }
        Err(error) => return Err(read_error(error)),
    };
    let meta = file.metadata().map_err(read_error)?;
    if !meta.is_file() {
        return Err(ThemeError::ManifestNotAFile { path });
    }
    let len = meta.len();
    if len > MAX_MANIFEST_BYTES {
        return Err(ThemeError::ManifestTooLarge { path, len });
    }
    // Capped too, in case the file grows after it is measured.
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        let len = bytes.len() as u64;
        return Err(ThemeError::ManifestTooLarge { path, len });
    }
    let text = String::from_utf8(bytes).map_err(|_| ThemeError::InvalidManifest {
        path: path.clone(),
        message: "the file is not UTF-8 text".to_owned(),
    })?;
    let manifest = Manifest::parse(&text, &path)?;
    Ok(Theme {
        slug,
        dir,
        manifest,
        parent: None,
    })
}

/// The `ELOOP` error number, which `O_NOFOLLOW` returns for a symbolic link.
#[cfg(unix)]
const ELOOP: i32 = libc::ELOOP;
#[cfg(not(unix))]
const ELOOP: i32 = -1;

/// Opens the manifest for reading without following a symbolic link and without blocking on a
/// FIFO, so a file swapped in after [`read_theme`]'s check is refused too.
#[cfg(unix)]
fn open_manifest(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Opens the manifest for reading. Links and special files are refused by the check before it.
#[cfg(not(unix))]
fn open_manifest(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

impl Theme {
    /// The theme's slug: its folder name.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The theme's folder, as given to [`load_theme`] (with `.` components dropped).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The theme's manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The parent theme, for a child theme.
    pub fn parent(&self) -> Option<&Theme> {
        self.parent.as_deref()
    }

    /// The menu locations this theme declares, then its parent's that it does not, as a child
    /// theme's `functions.php` runs before its parent's and both register theme locations. A
    /// location the child declares again keeps the child's description.
    pub fn menu_locations(&self) -> Vec<MenuLocation> {
        let mut locations = self.manifest.menus.clone();
        if let Some(parent) = self.parent() {
            for location in &parent.manifest.menus {
                if !locations.iter().any(|l| l.location == location.location) {
                    locations.push(location.clone());
                }
            }
        }
        locations
    }

    /// The page templates a page can choose, as `WP_Theme::get_page_templates` lists them: the
    /// ones this theme declares, then its parent's that it does not (a child's declaration keeps
    /// its own name), each only when its file is found ([`locate_template`](Self::locate_template),
    /// in the child or the parent), sorted by the name the admin shows, as
    /// `page_template_dropdown` sorts them.
    pub fn page_templates(&self) -> Vec<PageTemplate> {
        let mut templates = self.manifest.templates.clone();
        if let Some(parent) = self.parent() {
            for template in &parent.manifest.templates {
                if !templates.iter().any(|t| t.name == template.name) {
                    templates.push(template.clone());
                }
            }
        }
        templates.retain(|t| self.locate_template(&[t.name.as_str()]).is_some());
        templates.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.name.cmp(&b.name)));
        templates
    }

    /// The widget areas this theme declares, then its parent's that it does not, as for
    /// [`menu_locations`](Self::menu_locations).
    pub fn sidebars(&self) -> Vec<Sidebar> {
        let mut sidebars = self.manifest.sidebars.clone();
        if let Some(parent) = self.parent() {
            for sidebar in &parent.manifest.sidebars {
                if !sidebars.iter().any(|s| s.id == sidebar.id) {
                    sidebars.push(sidebar.clone());
                }
            }
        }
        sidebars
    }

    /// The Propel loader that renders this theme: a confined namespace named by the slug, with
    /// the parent theme's confined namespace as its declared parent. Unqualified names in
    /// `@extends` and `@include` resolve in the child first, then the parent, and never outside
    /// the two folders.
    pub fn loader(&self) -> ThemeLoader {
        let parent = self.parent();
        let mut files = FileLoader::new(&self.dir).confined_namespace(
            &self.slug,
            &self.dir,
            parent.map(Theme::slug),
        );
        if let Some(parent) = parent {
            files = files.confined_namespace(&parent.slug, &parent.dir, None);
        }
        ThemeLoader {
            files,
            active: self.slug.clone(),
        }
    }

    /// The first of `names` that this theme or its parent has, as WordPress's
    /// `locate_template`: for each name in order, the child is checked, then the parent. Pass
    /// [`templates_for`](super::templates_for)'s list.
    ///
    /// A name may have one folder (`templates/wide` is `templates/wide.propel`). A name Propel
    /// cannot express as a view is skipped: one with letters outside ASCII, `%`, `.` or any
    /// character other than letters, digits, `-` and `_`. A file whose real path is outside the
    /// theme's folder (a symbolic link leading out) is skipped too.
    pub fn locate_template<S: AsRef<str>>(&self, names: &[S]) -> Option<LocatedTemplate> {
        let themes: Vec<&Theme> = std::iter::once(self).chain(self.parent()).collect();
        let roots: Vec<Option<PathBuf>> = themes
            .iter()
            .map(|theme| std::fs::canonicalize(&theme.dir).ok())
            .collect();
        for name in names {
            let name = name.as_ref();
            let Some(segments) = view_segments(name) else {
                continue;
            };
            for (theme, root) in themes.iter().zip(&roots) {
                let Some(root) = root else { continue };
                if let Some(path) = view_file(root, &segments) {
                    return Some(LocatedTemplate {
                        name: name.to_owned(),
                        theme: theme.slug.clone(),
                        view: format!("{}::{}", theme.slug, segments.join(".")),
                        path,
                    });
                }
            }
        }
        None
    }

    /// Refuses a standalone theme without an `index` template, as WordPress does.
    fn require_index(&self) -> Result<(), ThemeError> {
        let found = std::fs::canonicalize(&self.dir)
            .ok()
            .and_then(|root| view_file(&root, &["index"]));
        match found {
            Some(_) => Ok(()),
            None => Err(ThemeError::MissingIndex {
                slug: self.slug.clone(),
            }),
        }
    }
}

/// The view segments for a template hierarchy name: one, or two for a name with a folder. `None`
/// when a segment is empty or has a character Propel does not allow in a view name.
fn view_segments(name: &str) -> Option<Vec<&str>> {
    let segments: Vec<&str> = name.split('/').collect();
    (segments.len() <= 2 && segments.iter().all(|segment| is_slug(segment))).then_some(segments)
}

/// The template file for `segments` under the canonical `root`, if it is a file whose real path
/// is inside `root`. The segments are already checked; this is the second check.
fn view_file(root: &Path, segments: &[&str]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for segment in segments {
        path.push(segment);
    }
    path.set_extension(VIEW_EXTENSION);
    let real = std::fs::canonicalize(&path).ok()?;
    (real.starts_with(root) && real.is_file()).then_some(real)
}

/// True for a non-empty run of ASCII letters, digits, `-` and `_`: a theme slug, and a segment
/// Propel accepts in a view or namespace name.
fn is_slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ThemeLoader {
    /// Refuses a name outside a namespace, so a render never reads the loader's unconfined root.
    fn check(name: &str) -> io::Result<()> {
        if name.contains("::") {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("`{name}` is not a theme view: name it as `theme::view`"),
            ))
        }
    }
}

impl Loader for ThemeLoader {
    fn source(&self, name: &str) -> io::Result<String> {
        Self::check(name)?;
        self.files.source(name)
    }

    fn resolve_reference(&self, from: &str, theme: Option<&str>, name: &str) -> io::Result<String> {
        self.files.resolve_reference(from, theme, name)
    }

    fn enter_theme(&self, theme: Option<&str>, template: &str) -> Option<String> {
        self.files
            .enter_theme(theme.or(Some(&self.active)), template)
    }

    fn template(&self, name: &str) -> Result<Arc<propel_parser::Template>, RenderError> {
        Self::check(name).map_err(|source| RenderError::Load {
            name: name.to_owned(),
            source,
        })?;
        self.files.template(name)
    }
}
