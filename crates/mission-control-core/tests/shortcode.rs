//! Shortcodes: WordPress's `do_shortcode` grammar, the attribute parser, owners and `the_content`.

use std::sync::Arc;

use mission_control_core::content::{Attributes, Shortcodes};
use mission_control_core::hooks::{Hooks, Owner, THE_CONTENT};

/// A registry with the tags the table below uses.
fn registry() -> Shortcodes {
    let s = Shortcodes::new();
    s.add_shortcode("hello", Owner::Core, |_, _| "HELLO".to_string())
        .unwrap();
    s.add_shortcode("bold", Owner::Core, |_, content| {
        format!("<b>{}</b>", content.unwrap_or("-"))
    })
    .unwrap();
    s.add_shortcode("attrs", Owner::Core, |a, c| {
        let named: Vec<String> = a.named().map(|(k, v)| format!("{k}={v}")).collect();
        let pos: Vec<&str> = a.positionals().iter().map(String::as_str).collect();
        format!(
            "[{}|{}|{}]",
            named.join(","),
            pos.join(","),
            c.map_or("none".to_string(), |c| format!("some:{c}"))
        )
    })
    .unwrap();
    s
}

#[test]
fn do_shortcodes_table() {
    let cases: &[(&str, &str)] = &[
        // No shortcode, no bracket.
        ("plain text", "plain text"),
        // Self-closing, both spellings, and the bare form.
        ("a [hello] b", "a HELLO b"),
        ("a [hello/] b", "a HELLO b"),
        ("a [hello /] b", "a HELLO b"),
        // Enclosing, with and without a closing tag.
        ("[bold]x[/bold]", "<b>x</b>"),
        ("[bold]x", "<b>-</b>x"),
        ("[bold][/bold]", "<b></b>"),
        // Content stops at the first closing tag; the rest is text.
        ("[bold]a[/bold] b [/bold]", "<b>a</b> b [/bold]"),
        // Attributes: double, single, bare, positional, mixed case names.
        (
            r#"[attrs a="1 2" b='3 4' c=5 "six" 'se ven' eight]"#,
            "[a=1 2,b=3 4,c=5|six,se ven,eight|none]",
        ),
        (r#"[attrs Foo="Bar"]"#, "[foo=Bar||none]"),
        (r#"[attrs a="x"/]"#, "[a=x||none]"),
        ("[attrs]in[/attrs]", "[||some:in]"),
        // Unknown tags stay as text.
        ("[nope] [nope]x[/nope]", "[nope] [nope]x[/nope]"),
        // `[[tag]]` prints `[tag]`, including its content and closing tag.
        ("[[hello]]", "[hello]"),
        ("[[bold]x[/bold]]", "[bold]x[/bold]"),
        // Half escapes keep the stray bracket.
        ("[[hello]", "[HELLO"),
        ("[hello]]", "HELLO]"),
        // A tag name must end at a boundary: `hello2` is not `hello`.
        ("[hello2]", "[hello2]"),
        ("[hello-x]", "[hello-x]"),
        // Malformed brackets never panic and stay as text.
        ("[", "["),
        ("[[", "[["),
        ("[hello", "[hello"),
        ("[hello a=\"1", "[hello a=\"1"),
        ("]]][[[", "]]][[["),
        ("[hello] [hello", "HELLO [hello"),
        ("é [hello] ü [", "é HELLO ü ["),
        // Several in one text.
        ("[hello] and [hello]", "HELLO and HELLO"),
    ];
    let s = registry();
    for (input, expected) in cases {
        assert_eq!(&s.do_shortcodes(input), expected, "input: {input:?}");
    }
}

#[test]
fn nested_shortcodes_are_left_to_the_callback() {
    let s = registry();
    assert_eq!(s.do_shortcodes("[bold][hello][/bold]"), "<b>[hello]</b>");
    let s = Arc::new(s);
    let inner = Arc::clone(&s);
    s.add_shortcode("outer", Owner::Core, move |_, c| {
        inner.do_shortcodes(c.unwrap_or(""))
    })
    .unwrap();
    assert_eq!(s.do_shortcodes("[outer][hello][/outer]"), "HELLO");
}

#[test]
fn the_callback_receives_parsed_attributes() {
    let s = Shortcodes::new();
    s.add_shortcode("t", Owner::Core, |a, _| {
        format!(
            "{:?}/{:?}/{:?}/{:?}",
            a.get("id"),
            a.get("missing"),
            a.positional(0),
            a.positional(1)
        )
    })
    .unwrap();
    assert_eq!(
        s.do_shortcodes(r#"[t ID=7 first]"#),
        r#"Some("7")/None/Some("first")/None"#
    );
}

#[test]
fn attributes_follow_shortcode_parse_atts() {
    let a = Attributes::parse(r#"  a = "x y"   b='z' c=d "q" e"#);
    assert_eq!(a.get("a"), Some("x y"));
    assert_eq!(a.get("b"), Some("z"));
    assert_eq!(a.get("c"), Some("d"));
    assert_eq!(a.positionals(), ["q", "e"]);
    assert!(Attributes::parse("   ").is_empty());
    // A later duplicate replaces the earlier one, as in a PHP array.
    assert_eq!(Attributes::parse("a=1 a=2").get("a"), Some("2"));
    // A quote that does not close is a positional word.
    assert_eq!(Attributes::parse(r#"a="x"y"#).positionals(), [r#"a="x"y"#]);
    // A non-breaking space separates words.
    assert_eq!(Attributes::parse("a=1\u{a0}b=2").get("b"), Some("2"));
}

#[test]
fn invalid_tags_are_refused() {
    let s = Shortcodes::new();
    for tag in ["", "a b", "a]", "[a", "a/b", "a=b", "a<b", "a&b", "a\u{1}"] {
        assert!(
            s.add_shortcode(tag, Owner::Core, |_, _| String::new())
                .is_err(),
            "{tag:?}"
        );
    }
    assert!(
        s.add_shortcode("my_tag-2.x", Owner::Core, |_, _| String::new())
            .is_ok()
    );
}

#[test]
fn adding_a_tag_again_replaces_it_and_remove_works() {
    let s = Shortcodes::new();
    s.add_shortcode("a", Owner::Core, |_, _| "1".into())
        .unwrap();
    s.add_shortcode("a", Owner::Core, |_, _| "2".into())
        .unwrap();
    assert_eq!(s.do_shortcodes("[a]"), "2");
    assert!(s.shortcode_exists("a"));
    assert!(s.remove_shortcode("a"));
    assert!(!s.remove_shortcode("a"));
    assert!(!s.shortcode_exists("a"));
    assert_eq!(s.do_shortcodes("[a]"), "[a]");
}

#[test]
fn remove_owner_removes_only_that_owners_tags() {
    let s = Shortcodes::new();
    let plugin = Owner::Plugin("gallery".into());
    s.add_shortcode("core", Owner::Core, |_, _| "C".into())
        .unwrap();
    s.add_shortcode("p1", plugin.clone(), |_, _| "P1".into())
        .unwrap();
    s.add_shortcode("p2", plugin.clone(), |_, _| "P2".into())
        .unwrap();
    s.add_shortcode("t", Owner::Theme("sage".into()), |_, _| "T".into())
        .unwrap();
    s.remove_owner(&plugin);
    assert_eq!(s.do_shortcodes("[core][p1][p2][t]"), "C[p1][p2]T");
}

#[test]
fn the_content_applies_shortcodes_at_priority_eleven() {
    let hooks = Hooks::new();
    let s = Arc::new(registry());
    s.register_the_content(&hooks);
    assert_eq!(
        hooks.apply_filters(THE_CONTENT, "x [hello]".to_string()),
        "x HELLO"
    );

    // A filter at priority 10 runs first and can emit a shortcode; one at 12 cannot.
    hooks.add_filter(THE_CONTENT, 10, Owner::Core, |c| c.replace("@", "[hello]"));
    hooks.add_filter(THE_CONTENT, 12, Owner::Core, |c| c.replace("%", "[hello]"));
    assert_eq!(
        hooks.apply_filters(THE_CONTENT, "@ %".to_string()),
        "HELLO [hello]"
    );

    // Shortcodes added later are picked up (the registry is live), and removed ones stop.
    s.add_shortcode("late", Owner::Plugin("p".into()), |_, _| "L".into())
        .unwrap();
    assert_eq!(hooks.apply_filters(THE_CONTENT, "[late]".into()), "L");
    s.remove_owner(&Owner::Plugin("p".into()));
    assert_eq!(hooks.apply_filters(THE_CONTENT, "[late]".into()), "[late]");
}

#[test]
fn hostile_input_stays_linear() {
    let s = registry();
    let n = 200_000;
    for input in [
        "[hello ".repeat(n),
        "[bold]".repeat(n),
        "[[".repeat(n),
        "[attrs /".repeat(n),
    ] {
        let start = std::time::Instant::now();
        let _ = s.do_shortcodes(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "too slow"
        );
    }
}
