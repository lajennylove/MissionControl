//! Task 12.1: the plugin manifest (`plugin.toml`) and its permissions.

use mission_control_core::plugins::{
    HOST_API_VERSION, HostPattern, MAX_HOSTS, MAX_MANIFEST_BYTES, MAX_SLUG_LEN, Manifest,
    ManifestError, Permission,
};

/// A minimal valid manifest, with `extra` appended.
fn manifest(extra: &str) -> String {
    format!(
        "slug = \"acme-forms\"\nname = \"Acme Forms\"\nversion = \"1.2.3\"\n\
         license = \"GPL-2.0-or-later\"\nrequires = \"0.1\"\n{extra}"
    )
}

/// The minimal manifest with the line starting with `key` removed, then `extra` appended.
fn without(key: &str, extra: &str) -> String {
    let mut text: String = manifest("")
        .lines()
        .filter(|line| !line.starts_with(&format!("{key} =")))
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(extra);
    text
}

fn parse(text: &str) -> Result<Manifest, ManifestError> {
    Manifest::parse(text)
}

fn refused(text: &str) -> (ManifestError, String) {
    let error = parse(text).expect_err("the manifest should be refused");
    let message = error.to_string();
    (error, message)
}

#[test]
fn full_manifest_reads_back_every_field() {
    let text = r#"
slug = "acme-forms"
name = "  Acme Forms  "
version = "1.2.3-beta.1"
license = "GPL-2.0-or-later"
license_uri = "https://www.gnu.org/licenses/gpl-2.0.html"
requires = "0.1.0"
description = "Contact forms.\nWith a second line."
plugin_uri = "https://acme.example/forms"
author = "Acme"
author_uri = "https://acme.example"
permissions = ["hooks", "read-content", "write-content", "migrations", "register-types",
               "admin-pages", "rest-routes", "shortcodes", "http", "data-files"]
hosts = ["api.acme.example", "Hooks.Acme.Example:8443", "*.cdn.acme.example"]
"#;
    let m = parse(text).expect("a full manifest parses");
    assert_eq!(m.slug, "acme-forms");
    assert_eq!(m.name, "Acme Forms");
    assert_eq!(m.version, semver::Version::parse("1.2.3-beta.1").unwrap());
    assert_eq!(m.license, "GPL-2.0-or-later");
    assert_eq!(
        m.license_uri.as_deref(),
        Some("https://www.gnu.org/licenses/gpl-2.0.html")
    );
    assert_eq!(m.requires, semver::Version::new(0, 1, 0));
    assert_eq!(
        m.description.as_deref(),
        Some("Contact forms.\nWith a second line.")
    );
    assert_eq!(m.plugin_uri.as_deref(), Some("https://acme.example/forms"));
    assert_eq!(m.author.as_deref(), Some("Acme"));
    assert_eq!(m.author_uri.as_deref(), Some("https://acme.example"));
    assert_eq!(m.permissions, Permission::ALL.to_vec());
    for permission in Permission::ALL {
        assert!(m.allows(permission));
    }
    assert_eq!(
        m.hosts,
        vec![
            HostPattern {
                host: "api.acme.example".into(),
                subdomains: false,
                port: None
            },
            HostPattern {
                host: "hooks.acme.example".into(),
                subdomains: false,
                port: Some(8443)
            },
            HostPattern {
                host: "cdn.acme.example".into(),
                subdomains: true,
                port: None
            },
        ]
    );
    assert_eq!(m.hosts[1].to_string(), "hooks.acme.example:8443");
    assert_eq!(m.hosts[2].to_string(), "*.cdn.acme.example");
    assert!(m.is_compatible());
    assert!(m.check_compatible().is_ok());
    assert!(m.license_is_known_gpl_compatible());
    assert_eq!(m.rest_namespace(), "acme-forms/v1");
    assert_eq!(m.table_prefix(), "acme_forms__");
}

#[test]
fn table_prefixes_of_two_slugs_never_make_the_same_table_name() {
    let prefix = |slug: &str| {
        parse(&without("slug", &format!("slug = \"{slug}\"")))
            .unwrap()
            .table_prefix()
    };
    let acme = prefix("acme");
    let acme_forms = prefix("acme-forms");
    assert_eq!(acme, "acme__");
    assert_eq!(acme_forms, "acme_forms__");
    // For a name to be shared, `acme__<a>` = `acme_forms__<b>`: they differ at byte 5 (`_` and
    // `f`) whatever the suffixes are. Check suffixes that try to bridge the gap.
    let suffixes = [
        "",
        "x",
        "forms",
        "forms__x",
        "_forms__x",
        "forms_",
        "f",
        "__",
    ];
    for a in suffixes {
        for b in suffixes {
            assert_ne!(
                format!("{acme}{a}"),
                format!("{acme_forms}{b}"),
                "{a} / {b}"
            );
        }
    }
    assert_ne!(acme.as_bytes()[5], acme_forms.as_bytes()[5]);
    // The longest prefix fits the identifier budget left to Task 12.6.
    assert_eq!(prefix(&"a".repeat(MAX_SLUG_LEN)).len(), 34);
}

#[test]
fn minimal_manifest_has_no_permissions_and_no_hosts() {
    let m = parse(&manifest("")).unwrap();
    assert!(m.permissions.is_empty());
    assert!(m.hosts.is_empty());
    assert!(!m.allows(Permission::Hooks));
    assert_eq!(m.description, None);
    assert_eq!(m.author, None);
}

// Permissions.

#[test]
fn unknown_permission_is_refused_by_name() {
    let (error, message) = refused(&manifest(r#"permissions = ["hooks", "raw-sql"]"#));
    assert_eq!(
        error,
        ManifestError::UnknownPermission {
            name: "raw-sql".into()
        }
    );
    assert!(message.contains("`raw-sql`"), "{message}");
}

#[test]
fn unknown_permission_with_odd_characters_is_named_escaped() {
    let (_, message) = refused(&manifest("permissions = [\"evil\\u001b[31m\"]"));
    assert!(message.contains("evil\\u{1b}[31m"), "{message}");
    assert!(!message.contains('\u{1b}'));
}

#[test]
fn permission_names_are_stable() {
    let names: Vec<&str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
    assert_eq!(
        names,
        [
            "hooks",
            "read-content",
            "write-content",
            "migrations",
            "register-types",
            "admin-pages",
            "rest-routes",
            "shortcodes",
            "http",
            "data-files"
        ]
    );
    for permission in Permission::ALL {
        assert_eq!(permission.as_str().parse::<Permission>(), Ok(permission));
        assert_eq!(permission.to_string(), permission.as_str());
        assert!(!permission.description().is_empty());
    }
}

#[test]
fn duplicate_permission_is_refused() {
    let (error, _) = refused(&manifest(r#"permissions = ["hooks", "hooks"]"#));
    assert_eq!(
        error,
        ManifestError::DuplicatePermission {
            permission: Permission::Hooks
        }
    );
}

#[test]
fn permissions_of_the_wrong_type_are_refused() {
    let (error, message) = refused(&manifest(r#"permissions = "hooks""#));
    assert!(
        matches!(error, ManifestError::InvalidToml { .. }),
        "{error:?}"
    );
    assert!(message.contains("line 6"), "{message}");
}

// Licence.

#[test]
fn missing_licence_is_refused() {
    let (error, message) = refused(&without("license", ""));
    assert_eq!(error, ManifestError::Missing { field: "license" });
    assert!(message.contains("`license`"), "{message}");
}

#[test]
fn empty_or_blank_licence_is_refused() {
    for blank in ["", "   ", "\t"] {
        let text = without("license", &format!("license = \"{blank}\""));
        assert_eq!(
            parse(&text).unwrap_err(),
            ManifestError::Missing { field: "license" }
        );
    }
}

#[test]
fn non_gpl_licence_is_accepted_and_flagged() {
    for license in ["Proprietary", "LicenseRef-Acme-Commercial", "BUSL-1.1"] {
        let m = parse(&without("license", &format!("license = \"{license}\""))).unwrap();
        assert_eq!(m.license, license);
        assert!(!m.license_is_known_gpl_compatible(), "{license}");
    }
}

#[test]
fn gpl_compatible_licences_are_recognised() {
    for license in [
        "GPL-2.0-or-later",
        "gpl-3.0-only",
        "MIT",
        "MIT OR Apache-2.0",
        "Proprietary OR GPL-2.0-or-later",
        "MIT AND BSD-3-Clause",
        "GPL-2.0+",
        "GPLv2 or later",
        "GPL v2 or later",
        "GPLv2",
        "GPL2",
        "GPLv3",
        "gpl-2.0",
        "GPL",
    ] {
        let m = parse(&without("license", &format!("license = \"{license}\""))).unwrap();
        assert!(m.license_is_known_gpl_compatible(), "{license}");
    }
    for license in [
        "MIT AND Proprietary",
        "GPLv2 or something",
        "(MIT)",
        "GPL-2.0 WITH x",
    ] {
        let m = parse(&without("license", &format!("license = \"{license}\""))).unwrap();
        assert!(!m.license_is_known_gpl_compatible(), "{license}");
    }
}

#[test]
fn licence_spelled_the_british_way_is_an_unknown_key() {
    let text = without("license", "licence = \"MIT\"");
    let (error, message) = refused(&text);
    assert!(
        matches!(error, ManifestError::InvalidToml { .. }),
        "{error:?}"
    );
    assert!(message.contains("unknown key `licence`"), "{message}");
}

// Slug.

#[test]
fn missing_slug_is_refused() {
    assert_eq!(
        parse(&without("slug", "")).unwrap_err(),
        ManifestError::Missing { field: "slug" }
    );
}

#[test]
fn invalid_slugs_are_refused() {
    let too_long = "a".repeat(MAX_SLUG_LEN + 1);
    for slug in [
        "Acme",
        "acme_forms",
        "acme forms",
        "-acme",
        "acme-",
        "acme--forms",
        "1acme",
        "acmé",
        "acme/forms",
        "../acme",
        too_long.as_str(),
    ] {
        let text = without("slug", &format!("slug = \"{slug}\""));
        let error = parse(&text).unwrap_err();
        assert!(
            matches!(error, ManifestError::InvalidSlug { .. }),
            "{slug}: {error:?}"
        );
    }
    let longest = "a".repeat(MAX_SLUG_LEN);
    let text = without("slug", &format!("slug = \"{longest}\""));
    assert_eq!(parse(&text).unwrap().slug, longest);
    let text = without("slug", "slug = \"a1-b2\"");
    assert_eq!(parse(&text).unwrap().slug, "a1-b2");
}

#[test]
fn reserved_slugs_are_refused() {
    for slug in [
        "mc",
        "wp",
        "oembed",
        "batch",
        "mission-control",
        "wp-site-health",
        "wp-block-editor",
        "wp-abilities",
        "wp-mail-smtp",
        // Mission Control's own prefix (its initials, owner decision).
        "mc-forms",
        "mc-seo",
        // Their table prefixes would start with `sqlite_`, which SQLite keeps.
        "sqlite",
        "sqlite-cache",
    ] {
        let text = without("slug", &format!("slug = \"{slug}\""));
        assert_eq!(
            parse(&text).unwrap_err(),
            ManifestError::ReservedSlug {
                slug: slug.to_owned()
            }
        );
    }
    let text = without("slug", "slug = \"wpforms\"");
    assert_eq!(parse(&text).unwrap().slug, "wpforms");
    let text = without("slug", "slug = \"mcforms\"");
    assert_eq!(parse(&text).unwrap().slug, "mcforms");
    let text = without("slug", "slug = \"sqlitex\"");
    assert_eq!(parse(&text).unwrap().table_prefix(), "sqlitex__");
}

// Name and other text.

#[test]
fn missing_or_blank_name_is_refused() {
    assert_eq!(
        parse(&without("name", "")).unwrap_err(),
        ManifestError::Missing { field: "name" }
    );
    assert_eq!(
        parse(&without("name", "name = \"  \"")).unwrap_err(),
        ManifestError::Missing { field: "name" }
    );
}

#[test]
fn control_characters_in_a_one_line_field_are_refused() {
    let (error, _) = refused(&without("name", "name = \"Acme\\nForms\""));
    assert_eq!(error, ManifestError::ControlCharacter { field: "name" });
}

#[test]
fn line_and_paragraph_separators_are_refused_in_one_line_text() {
    for c in ["\\u2028", "\\u2029"] {
        let (error, _) = refused(&without("name", &format!("name = \"Acme{c}Forms\"")));
        assert_eq!(
            error,
            ManifestError::ControlCharacter { field: "name" },
            "{c}"
        );
    }
}

#[test]
fn invisible_format_characters_are_refused() {
    for c in [
        "\\u200B",
        "\\u200F",
        "\\u202A",
        "\\u202E",
        "\\u2060",
        "\\u2064",
        "\\u2066",
        "\\u2069",
        "\\uFEFF",
        "\\uFFF9",
        "\\U000E0041",
    ] {
        let (error, _) = refused(&without("name", &format!("name = \"Acme{c}Forms\"")));
        assert_eq!(
            error,
            ManifestError::ControlCharacter { field: "name" },
            "{c}"
        );
        let (error, _) = refused(&manifest(&format!("description = \"Line one\\n{c}two\"")));
        assert_eq!(
            error,
            ManifestError::ControlCharacter {
                field: "description"
            },
            "{c}"
        );
        let (error, _) = refused(&manifest(&format!(
            "plugin_uri = \"https://acme.example/{c}\""
        )));
        assert_eq!(
            error,
            ManifestError::InvalidUri {
                field: "plugin_uri"
            },
            "{c}"
        );
    }
}

#[test]
fn over_long_text_is_refused() {
    let (error, _) = refused(&manifest(&format!("author = \"{}\"", "x".repeat(201))));
    assert!(
        matches!(
            error,
            ManifestError::TooLong {
                field: "author",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn uris_must_be_http_or_https() {
    for uri in [
        "javascript:alert(1)",
        "ftp://acme.example",
        "acme.example",
        "https://a b",
    ] {
        let (error, _) = refused(&manifest(&format!("plugin_uri = \"{uri}\"")));
        assert_eq!(
            error,
            ManifestError::InvalidUri {
                field: "plugin_uri"
            },
            "{uri}"
        );
    }
    let m = parse(&manifest("author_uri = \"http://acme.example\"")).unwrap();
    assert_eq!(m.author_uri.as_deref(), Some("http://acme.example"));
}

#[test]
fn empty_optional_text_is_none() {
    let m = parse(&manifest("description = \" \"\nauthor = \"\"")).unwrap();
    assert_eq!(m.description, None);
    assert_eq!(m.author, None);
}

// Version and requires.

#[test]
fn version_must_be_semver() {
    assert_eq!(
        parse(&without("version", "")).unwrap_err(),
        ManifestError::Missing { field: "version" }
    );
    let too_long = format!("1.0.0-{}", "a".repeat(64));
    for version in ["1.2", "v1.2.3", "1.2.3.4", "latest", too_long.as_str()] {
        let (error, message) = refused(&without("version", &format!("version = \"{version}\"")));
        assert!(
            matches!(error, ManifestError::InvalidVersion { .. }),
            "{version}: {error:?}"
        );
        assert!(message.contains("`version`"), "{message}");
    }
}

#[test]
fn requires_must_be_a_version() {
    assert_eq!(
        parse(&without("requires", "")).unwrap_err(),
        ManifestError::Missing { field: "requires" }
    );
    let too_long = format!("0.1.0-{}", "a".repeat(64));
    for requires in [
        "any",
        "*",
        "^0.1",
        ">=0.1",
        ">=0.1, <0.3",
        "0.1.2.3",
        "01.2",
        "0.",
        "v0.1",
        too_long.as_str(),
    ] {
        let (error, message) = refused(&without("requires", &format!("requires = \"{requires}\"")));
        assert!(
            matches!(error, ManifestError::InvalidRequires { .. }),
            "{requires}: {error:?}"
        );
        assert!(message.contains("`requires`"), "{message}");
    }
}

#[test]
fn requires_is_a_minimum_version_with_missing_parts_zero() {
    for (requires, expected) in [
        ("0", semver::Version::new(0, 0, 0)),
        ("0.1", semver::Version::new(0, 1, 0)),
        ("1.2", semver::Version::new(1, 2, 0)),
        ("0.1.2", semver::Version::new(0, 1, 2)),
    ] {
        let m = parse(&without("requires", &format!("requires = \"{requires}\""))).unwrap();
        assert_eq!(m.requires, expected, "{requires}");
    }
}

#[test]
fn incompatible_plugin_parses_but_is_not_compatible() {
    let m = parse(&without("requires", "requires = \"2.0\"")).unwrap();
    assert!(!m.is_compatible());
    let error = m.check_compatible().unwrap_err();
    assert_eq!(
        error,
        ManifestError::Incompatible {
            requires: semver::Version::new(2, 0, 0),
            host: HOST_API_VERSION,
        }
    );
    let message = error.to_string();
    assert!(message.contains("2.0.0"), "{message}");
    assert!(message.contains(&HOST_API_VERSION.to_string()), "{message}");
}

#[test]
fn requires_at_least_is_compatible_with_the_same_or_an_older_version() {
    // HOST_API_VERSION is 0.1.0: requiring it, or anything older, is compatible, as WordPress
    // compares `Requires at least` with its own version.
    assert_eq!(HOST_API_VERSION, semver::Version::new(0, 1, 0));
    for requires in ["0.1", "0.1.0", "0", "0.0.9", "0.1.0-alpha", "0.1.0+build.5"] {
        let m = parse(&without("requires", &format!("requires = \"{requires}\""))).unwrap();
        assert!(m.is_compatible(), "{requires}");
    }
    for requires in ["0.1.1", "0.2", "1"] {
        let m = parse(&without("requires", &format!("requires = \"{requires}\""))).unwrap();
        assert!(!m.is_compatible(), "{requires}");
    }
}

// Hosts.

#[test]
fn hosts_without_http_permission_are_refused() {
    let (error, _) = refused(&manifest(r#"hosts = ["api.acme.example"]"#));
    assert_eq!(error, ManifestError::HostsWithoutHttp);
}

#[test]
fn http_permission_without_hosts_is_refused() {
    let (error, _) = refused(&manifest(r#"permissions = ["http"]"#));
    assert_eq!(error, ManifestError::HttpWithoutHosts);
    let (error, _) = refused(&manifest("permissions = [\"http\"]\nhosts = []"));
    assert_eq!(error, ManifestError::HttpWithoutHosts);
}

#[test]
fn invalid_hosts_are_refused() {
    let long_label = format!("{}.example", "a".repeat(64));
    for host in [
        "https://api.acme.example",
        "api.acme.example/path",
        "localhost",
        "*.example",
        "*",
        "api.*.example",
        "**.acme.example",
        "127.0.0.1",
        "[::1]",
        "api.acme.example:0",
        "api.acme.example:65536",
        "api.acme.example:",
        "api.acme.example:http",
        "-api.acme.example",
        "api-.acme.example",
        "api..acme.example",
        "api.acme.example.",
        "user@api.acme.example",
        "",
        long_label.as_str(),
        "acme.0x7f000001",
        "a.0x7F",
        "acme.0x",
        "api.acme.example:80",
        "api.acme.example:443",
        "bücher.example",
        "*.bücher.example",
    ] {
        let text = manifest(&format!("permissions = [\"http\"]\nhosts = [\"{host}\"]"));
        let error = parse(&text).unwrap_err();
        assert!(
            matches!(error, ManifestError::InvalidHost { .. }),
            "{host}: {error:?}"
        );
    }
}

#[test]
fn punycode_and_hex_looking_names_that_are_not_addresses_are_accepted() {
    let text = manifest(
        "permissions = [\"http\"]\n\
         hosts = [\"xn--bcher-kva.example\", \"0xcafe.example\", \"api.acme.example:8080\"]",
    );
    let m = parse(&text).unwrap();
    assert_eq!(m.hosts[0].host, "xn--bcher-kva.example");
    assert_eq!(m.hosts[1].host, "0xcafe.example");
    assert_eq!(m.hosts[2].port, Some(8080));
}

#[test]
fn duplicate_host_is_refused() {
    let text =
        manifest("permissions = [\"http\"]\nhosts = [\"api.acme.example\", \"API.acme.example\"]");
    assert_eq!(
        parse(&text).unwrap_err(),
        ManifestError::DuplicateHost {
            host: "api.acme.example".into()
        }
    );
}

#[test]
fn too_many_hosts_are_refused() {
    let hosts: Vec<String> = (0..=MAX_HOSTS)
        .map(|i| format!("\"h{i}.acme.example\""))
        .collect();
    let text = manifest(&format!(
        "permissions = [\"http\"]\nhosts = [{}]",
        hosts.join(", ")
    ));
    assert_eq!(
        parse(&text).unwrap_err(),
        ManifestError::TooManyHosts {
            count: MAX_HOSTS + 1
        }
    );
}

#[test]
fn host_patterns_match_as_documented() {
    let text = manifest(
        "permissions = [\"http\"]\n\
         hosts = [\"api.acme.example\", \"*.cdn.acme.example\", \"hooks.acme.example:8443\"]",
    );
    let m = parse(&text).unwrap();
    assert!(m.allows_host("api.acme.example", None));
    assert!(m.allows_host("API.Acme.Example", None));
    assert!(!m.allows_host("api.acme.example", Some(8443)));
    assert!(!m.allows_host("x.api.acme.example", None));
    assert!(m.allows_host("a.cdn.acme.example", None));
    assert!(m.allows_host("a.b.cdn.acme.example", None));
    assert!(!m.allows_host("cdn.acme.example", None));
    assert!(!m.allows_host("evilcdn.acme.example", None));
    assert!(m.allows_host("hooks.acme.example", Some(8443)));
    assert!(!m.allows_host("hooks.acme.example", None));
    assert!(!m.allows_host("other.example", None));
}

// Bounds and strictness.

#[test]
fn oversized_manifest_is_refused() {
    let padding = format!("# {}\n", "x".repeat(MAX_MANIFEST_BYTES));
    let (error, _) = refused(&manifest(&padding));
    assert!(matches!(error, ManifestError::TooLarge { .. }), "{error:?}");
}

#[test]
fn parse_bytes_checks_the_size_before_decoding() {
    let m = Manifest::parse_bytes(manifest("").as_bytes()).unwrap();
    assert_eq!(m.slug, "acme-forms");
    // Over the limit and not UTF-8: refused for its size, before it is decoded.
    let big = vec![0xFF; MAX_MANIFEST_BYTES + 1];
    assert_eq!(
        Manifest::parse_bytes(&big).unwrap_err(),
        ManifestError::TooLarge {
            len: MAX_MANIFEST_BYTES + 1
        }
    );
    let mut bad = manifest("").into_bytes();
    bad.extend_from_slice(b"# \xFF\n");
    assert_eq!(
        Manifest::parse_bytes(&bad).unwrap_err(),
        ManifestError::NotUtf8
    );
}

#[test]
fn unknown_key_is_refused_by_name() {
    let (error, message) = refused(&manifest("network = true"));
    assert!(
        matches!(error, ManifestError::InvalidToml { .. }),
        "{error:?}"
    );
    assert!(message.contains("unknown key `network`"), "{message}");
}

#[test]
fn invalid_toml_is_refused_without_echoing_values() {
    let (error, message) = refused("slug = \"secret-value");
    assert!(
        matches!(error, ManifestError::InvalidToml { .. }),
        "{error:?}"
    );
    assert!(!message.contains("secret-value"), "{message}");
}
