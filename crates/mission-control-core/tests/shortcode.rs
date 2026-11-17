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
        // Hyphens in names, as `[\w-]+`; empty quoted positionals are dropped.
        (r#"[attrs data-id="5"]"#, "[data-id=5||none]"),
        ("[attrs aria-label='x y']", "[aria-label=x y||none]"),
        ("[attrs data-x=y]", "[data-x=y||none]"),
        (r#"[attrs "" '' w e=""]"#, "[e=|w|none]"),
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
        // wpautop (priority 10) makes the paragraph first.
        "<p>x HELLO</p>\n"
    );

    // A filter at priority 10 runs first and can emit a shortcode; one at 12 cannot.
    hooks.add_filter(THE_CONTENT, 10, Owner::Core, |c| c.replace("@", "[hello]"));
    hooks.add_filter(THE_CONTENT, 12, Owner::Core, |c| c.replace("%", "[hello]"));
    assert_eq!(
        hooks.apply_filters(THE_CONTENT, "@ %".to_string()),
        "<p>HELLO [hello]</p>\n"
    );

    // Shortcodes added later are picked up (the registry is live), and removed ones stop.
    s.add_shortcode("late", Owner::Plugin("p".into()), |_, _| "L".into())
        .unwrap();
    // A shortcode alone in its paragraph is taken out of it (shortcode_unautop).
    assert_eq!(hooks.apply_filters(THE_CONTENT, "[late]".into()), "L\n");
    s.remove_owner(&Owner::Plugin("p".into()));
    assert_eq!(
        hooks.apply_filters(THE_CONTENT, "[late]".into()),
        "<p>[late]</p>\n"
    );
}

#[test]
fn the_content_makes_paragraphs_as_wordpress_does() {
    let hooks = Hooks::new();
    let s = Arc::new(registry());
    s.register_the_content(&hooks);
    assert_eq!(
        hooks.apply_filters(THE_CONTENT, "One\n\nTwo\nlines".to_string()),
        "<p>One</p>\n<p>Two<br />\nlines</p>\n"
    );
    // Block markup is left to its blocks, as `do_blocks` removes wpautop.
    let blocks = "<!-- wp:paragraph -->\n<p>Hi</p>\n<!-- /wp:paragraph -->";
    assert_eq!(hooks.apply_filters(THE_CONTENT, blocks.to_string()), blocks);
}

#[test]
fn shortcodes_inside_html_tags_follow_wordpress() {
    let s = registry();
    // In a quoted attribute, the output is kept when kses keeps the attribute.
    assert_eq!(
        s.do_shortcodes("<a title=\"[hello]\">x</a>"),
        "<a title=\"HELLO\">x</a>"
    );
    // A bracket elsewhere in a tag is never read as a shortcode.
    assert_eq!(
        s.do_shortcodes("<p data-x=]>[hello]</p>"),
        "<p data-x=]>HELLO</p>"
    );
    // A comment's brackets stay as written.
    assert_eq!(
        s.do_shortcodes("<!-- [hello] --> [hello]"),
        "<!-- [hello] --> HELLO"
    );
    // Table cells are checked inside a table, as kses reads them.
    assert_eq!(
        s.do_shortcodes("<td title=\"[hello]\">x</td>"),
        "<td title=\"HELLO\">x</td>"
    );
    // An attribute kses drops keeps the shortcode text.
    assert_eq!(
        s.do_shortcodes("<a onclick=\"[hello]\">x</a>"),
        "<a onclick=\"[hello]\">x</a>"
    );
}

#[test]
fn hostile_input_stays_linear_through_the_content_filters() {
    use std::time::Instant;
    let hooks = Hooks::new();
    let s = Arc::new(registry());
    s.register_the_content(&hooks);
    for unit in [
        "[hello]\n\n",
        "<p>[hello ",
        "<div",
        "<p><li",
        "<svg",
        "<figcaption",
    ] {
        let text = unit.repeat(50_000);
        let started = Instant::now();
        let _ = hooks.apply_filters(THE_CONTENT, text);
        // Linear work is well under a second even in a debug build; quadratic work is minutes.
        assert!(
            started.elapsed().as_secs() < 20,
            "{unit:?} took {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn hostile_input_stays_linear() {
    let s = registry();
    let n = 200_000;
    // (input, expected output)
    let cases: Vec<(String, String)> = vec![
        // No `]` at all: the scan gives up at once and the text is unchanged.
        ("[hello ".repeat(n), "[hello ".repeat(n)),
        // Closers missing: each `[bold]` is replaced, the missing-closer search runs once.
        ("[bold]".repeat(n), "<b>-</b>".repeat(n)),
        ("[[".repeat(n), "[[".repeat(n)),
        ("[attrs /".repeat(n), "[attrs /".repeat(n)),
        // One closer at the very end: the first `[bold]` takes everything.
        (
            "[bold]x".repeat(n) + "[/bold]",
            format!("<b>x{}</b>", "[bold]x".repeat(n - 1)),
        ),
        // Many `[hello` before one `]`: the first takes the `]`, the rest is its attributes.
        ("[hello ".repeat(n) + "]", "HELLO".to_string()),
        // Interleaved tags without closers.
        ("[bold][attrs]".repeat(n), "<b>-</b>[||none]".repeat(n)),
    ];
    for (input, expected) in cases {
        let start = std::time::Instant::now();
        let out = s.do_shortcodes(&input);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "too slow"
        );
        assert!(out == expected, "unexpected output (len {})", out.len());
    }
}

/// WordPress's default `[caption]` (`img_caption_shortcode`, Ruling R134), in its HTML5 output.
#[test]
fn caption_renders_wordpress_html5_figure() {
    let s = Arc::new(Shortcodes::new());
    s.register_core_shortcodes();
    assert!(s.shortcode_exists("caption") && s.shortcode_exists("wp_caption"));
    let img = r#"<img src="/u/a-300x225.jpg" alt="x" width="300" height="225" class="size-medium wp-image-7" />"#;

    // What the editor's Add Media inserts: the caption after the image.
    let out = s.do_shortcodes(&format!(
        r#"[caption id="attachment_7" align="aligncenter" width="300"]{img} Boats at <em>dawn</em>[/caption]"#
    ));
    assert_eq!(
        out,
        format!(
            r#"<figure id="attachment_7" aria-describedby="caption-attachment-7" style="width: 300px" class="wp-caption aligncenter">{img}<figcaption id="caption-attachment-7" class="wp-caption-text">Boats at <em>dawn</em></figcaption></figure>"#
        )
    );
    // A link around the image stays with it; text before the image is dropped, as WordPress.
    let out = s.do_shortcodes(&format!(
        r#"[wp_caption id="attachment_7" width="300px"]before <a href="/x">{img}</a>  Cap [/wp_caption]"#
    ));
    assert_eq!(
        out,
        format!(
            r#"<figure id="attachment_7" aria-describedby="caption-attachment-7" style="width: 300px" class="wp-caption alignnone"><a href="/x">{img}</a><figcaption id="caption-attachment-7" class="wp-caption-text">Cap</figcaption></figure>"#
        )
    );
    // Attributes: ids cleaned as `sanitize_html_class`, the rest escaped; the `caption`
    // attribute's tags stripped; a `caption_id` of its own; extra classes.
    let out = s.do_shortcodes(&format!(
        r#"[caption id='a"t<t>%20x' align='alignleft" onclick="x' width=150 class="big" caption="Hi <script>x</script>there" caption_id="c_1"]{img}[/caption]"#
    ));
    assert_eq!(
        out,
        format!(
            r#"<figure id="attx" aria-describedby="c_1" style="width: 150px" class="wp-caption alignleft&quot; onclick=&quot;x big">{img}<figcaption id="c_1" class="wp-caption-text">Hi there</figcaption></figure>"#
        )
    );
    // No width, a width below 1, or no caption: the content without the wrapper.
    for call in [
        format!(r#"[caption id="attachment_7"]{img} Cap[/caption]"#),
        format!(r#"[caption width="0"]{img} Cap[/caption]"#),
        format!(r#"[caption width="abc"]{img} Cap[/caption]"#),
        format!(r#"[caption width="300"]{img}[/caption]"#),
    ] {
        assert_eq!(s.do_shortcodes(&call), img, "{call}");
    }
    // No id: no ids printed. The caption is printed as stored: its shortcodes do not run.
    s.add_shortcode("hello", Owner::Core, |_, _| "HELLO".to_string())
        .unwrap();
    let out = s.do_shortcodes(&format!(
        r#"[caption width="10"]{img}[hello] Cap[/caption]"#
    ));
    assert_eq!(
        out,
        format!(
            r#"<figure style="width: 10px" class="wp-caption alignnone">{img}<figcaption class="wp-caption-text">[hello] Cap</figcaption></figure>"#
        )
    );
    // A self-closing caption, or one with no image and no caption, is empty.
    assert_eq!(s.do_shortcodes(r#"[caption width="10" /]"#), "");
    // `the_content` applies it.
    let hooks = Hooks::new();
    s.register_the_content(&hooks);
    let shown = hooks.apply_filters(
        THE_CONTENT,
        format!(r#"[caption width="300"]{img} C[/caption]"#),
    );
    assert!(
        shown.starts_with("<figure style=\"width: 300px\""),
        "{shown}"
    );
}
