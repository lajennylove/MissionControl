//! Tests for kses, WordPress's HTML filter on save: `wp_kses_post` with `$allowedposttags`,
//! `wp_filter_kses` with `$allowedtags`, `safecss_filter_attr` for the `style` attribute, and the
//! save-path filter that runs them for a user without `unfiltered_html`. Pure logic: no database.

use std::collections::BTreeMap;

use mission_control_core::content::{
    PostFields, kses_data, kses_post, kses_post_fields, safecss_filter_attr,
};
use mission_control_core::users::{ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, UserCaps};

/// A user with one default role and no capabilities of their own.
fn user(role: &str) -> UserCaps {
    UserCaps::with_default_roles(1, &[role], &BTreeMap::new())
}

#[test]
fn kses_post_removes_unsafe_markup() {
    // (input, what the output must not contain, what it must contain)
    let table: &[(&str, &str, &str)] = &[
        ("<p>a<script>alert(1)</script>b</p>", "script", "<p>ab</p>"),
        ("<p>a<script>alert(1)</script>b</p>", "alert", "<p>ab</p>"),
        (
            "<img src=\"x.png\" onerror=\"alert(1)\">",
            "onerror",
            "<img src=\"x.png\">",
        ),
        ("<p onclick=\"alert(1)\">hi</p>", "onclick", "<p>hi</p>"),
        (
            "<a href=\"javascript:alert(1)\">x</a>",
            "javascript",
            "<a>x</a>",
        ),
        (
            "<a href=\" JaVaScRiPt:alert(1)\">x</a>",
            "alert",
            "<a>x</a>",
        ),
        (
            "<a href=\"java&#09;script:alert(1)\">x</a>",
            "alert",
            "<a>x</a>",
        ),
        (
            "<a href=\"java&#x3A;script:alert(1)\">x</a>",
            "alert",
            "<a>x</a>",
        ),
        (
            "<a href=\"data:text/html;base64,PHNjcmlwdD4=\">x</a>",
            "data:",
            "<a>x</a>",
        ),
        (
            "<img src=\"data:image/svg+xml,<svg onload=alert(1)>\">",
            "data:",
            "<img>",
        ),
        (
            "<blockquote cite=\"javascript:alert(1)\">q</blockquote>",
            "javascript",
            "<blockquote>q</blockquote>",
        ),
        (
            "<img longdesc=\"vbscript:x\" alt=\"a\">",
            "vbscript",
            "<img alt=\"a\">",
        ),
        (
            "<iframe src=\"https://evil.example\"></iframe>",
            "iframe",
            "",
        ),
        ("<iframe src=\"https://evil.example\"></iframe>", "evil", ""),
        (
            "<style>body{display:none}</style><p>x</p>",
            "style",
            "<p>x</p>",
        ),
        (
            "<style>body{display:none}</style><p>x</p>",
            "display",
            "<p>x</p>",
        ),
        (
            "<p style=\"width: expression(alert(1))\">x</p>",
            "expression",
            "<p>x</p>",
        ),
        (
            "<p style=\"background: url(javascript:alert(1))\">x</p>",
            "javascript",
            "<p>x</p>",
        ),
        (
            "<p style=\"behavior: url(x.htc)\">x</p>",
            "behavior",
            "<p>x</p>",
        ),
        (
            "<p style=\"-moz-binding: url(x.xml)\">x</p>",
            "binding",
            "<p>x</p>",
        ),
        ("<form action=\"/x\"><input name=\"a\"></form>", "form", ""),
        ("<form action=\"/x\"><input name=\"a\"></form>", "input", ""),
        ("<object data=\"x.swf\"></object>", "object", ""),
        ("<embed src=\"x.swf\">", "embed", ""),
        ("<svg><script>alert(1)</script></svg>", "alert", ""),
        (
            "<math><mi xlink:href=\"javascript:alert(1)\">x</mi></math>",
            "javascript",
            "",
        ),
        ("<base href=\"https://evil.example/\">", "base", ""),
        (
            "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">",
            "meta",
            "",
        ),
        (
            "<link rel=\"stylesheet\" href=\"https://evil.example/x.css\">",
            "link",
            "",
        ),
        ("<p>a<!-- secret -->b</p>", "secret", "<p>ab</p>"),
        (
            "<p>a<!--[if IE]><script>alert(1)</script><![endif]-->b</p>",
            "alert",
            "<p>ab</p>",
        ),
        (
            "<p data-x=\"1\" data-a:b=\"2\">x</p>",
            "data-a:b",
            "data-x=\"1\"",
        ),
        (
            "<p>a</p><unknown-tag>b</unknown-tag>",
            "unknown",
            "<p>a</p>b",
        ),
        ("<p>a < b & c</p>", "& c", "<p>a &lt; b &amp; c</p>"),
    ];
    for (input, banned, kept) in table {
        let output = kses_post(input);
        assert!(
            !output.to_lowercase().contains(banned),
            "{input:?} gave {output:?}, which contains {banned:?}"
        );
        assert!(
            output.contains(kept),
            "{input:?} gave {output:?}, without {kept:?}"
        );
        // Filtering what was stored changes nothing more.
        assert_eq!(kses_post(&output), output, "{input:?}");
    }
}

#[test]
fn kses_post_keeps_allowed_markup() {
    let table: &[&str] = &[
        "<p>Hello <strong>world</strong> and <em>you</em>.</p>",
        "<p><a href=\"https://example.com/a?b=c&amp;d=e\">secure</a> \
         <a href=\"http://example.com/\">plain</a> <a href=\"/relative\">relative</a> \
         <a href=\"#top\">anchor</a> <a href=\"mailto:a@example.com\">mail</a> \
         <a href=\"tel:+15555550100\">call</a></p>",
        "<p><a href=\"https://example.com/\" rel=\"nofollow\" target=\"_blank\">kept rel</a></p>",
        "<img src=\"https://example.com/a.jpg\" alt=\"An image\" width=\"300\" height=\"200\" \
         loading=\"lazy\">",
        "<ul><li>one</li><li>two</li></ul><ol start=\"3\" reversed=\"\"><li>three</li></ol>",
        "<table><thead><tr><th scope=\"col\">h</th></tr></thead><tbody><tr><td colspan=\"2\">d</td>\
         </tr></tbody></table>",
        "<div class=\"wp-block-group alignwide\" id=\"intro\"><p>x</p></div>",
        "<p data-align=\"center\" aria-label=\"label\" role=\"note\" lang=\"en\" dir=\"ltr\">x</p>",
        "<figure class=\"wp-block-image\"><img src=\"/a.png\" alt=\"\"><figcaption>c</figcaption>\
         </figure>",
        "<blockquote cite=\"https://example.com/\"><p>q</p></blockquote>",
        "<h2>Heading</h2><pre>code</pre><code>x</code><hr><br>",
        "<p>Before</p><!--more--><p>After</p>",
        "<p>One</p><!--nextpage--><p>Two</p>",
        "<p>One</p><!--more Read on--><p>Two</p>",
        "<p style=\"color: red;text-align: center\">styled</p>",
        "<p style=\"background-image: url(https://example.com/a.png)\">bg</p>",
        "<p style=\"width: calc(100% - 2em)\">calc</p>",
        "<p style=\"--wp--custom: 1px\">var</p>",
        "<p style=\"background: linear-gradient(135deg,rgb(6,147,227) 0%,rgb(155,81,224) 100%)\">g</p>",
        "<video src=\"https://example.com/a.mp4\" controls=\"\" poster=\"https://example.com/a.jpg\">\
         </video>",
        "<p>Caf\u{e9} &amp; cr\u{e8}me</p>",
    ];
    for input in table {
        assert_eq!(&kses_post(input), input);
    }
}

#[test]
fn kses_post_normalises_markup_and_entities() {
    assert_eq!(kses_post("<p>a &amp b</p>"), "<p>a &amp; b</p>");
    assert_eq!(kses_post("<P CLASS=x>y"), "<p class=\"x\">y</p>");
    assert_eq!(kses_post("<br/>"), "<br>");
    assert_eq!(kses_post("&eacute;&nbsp;&#169;"), "\u{e9}&nbsp;\u{a9}");
}

#[test]
fn kses_post_keeps_the_page_markers_but_cannot_be_used_to_forge_them() {
    // Custom "more" text loses what could break out of an attribute or a comment.
    assert_eq!(
        kses_post("<p>a</p><!--more Read \"on\" <b>now</b> -- x-->"),
        "<p>a</p><!--more Read on bnow/b - x-->"
    );
    // A marker is only kept between elements: not in an attribute value, nor in the text of
    // `<textarea>` or `<title>`, where splitting at it would cut the tag open.
    assert_eq!(
        kses_post("<p title=\"<!--more x\" onclick=\"alert(1)-->\">t</p>"),
        "<p title=\"\">t</p>"
    );
    assert_eq!(
        kses_post("<b title=\"a<!--more-->b\"><p>x<!--nextpage-->y</p>"),
        "<b title=\"ab\"><p>x<!--nextpage-->y</p></b>"
    );
    assert_eq!(
        kses_post(
            "<textarea>a<!--more-->b</textarea><title><!--nextpage--></title><p>c<!--more-->d</p>"
        ),
        "<textarea>ab</textarea><title></title><p>c<!--more-->d</p>"
    );
    assert_eq!(
        kses_post("<p title=\"a &quot;<!--more-->\">x</p><!--more-->"),
        "<p title=\"a &quot;\">x</p><!--more-->"
    );
    // Markers inside removed elements go with them.
    assert_eq!(kses_post("<script><!--more--></script>"), "");
    // Other comments are removed.
    assert_eq!(
        kses_post("<!--morex--><!-- more --><!--nextpage x-->"),
        "<!--morex-->"
    );
}

#[test]
fn kses_data_keeps_only_the_small_set() {
    assert_eq!(
        kses_data("<a href=\"https://example.com/\" title=\"t\" class=\"c\">x</a>"),
        "<a href=\"https://example.com/\" title=\"t\">x</a>"
    );
    assert_eq!(
        kses_data("<strong>b</strong> <em>i</em> <code>c</code>"),
        "<strong>b</strong> <em>i</em> <code>c</code>"
    );
    assert_eq!(
        kses_data("<p style=\"color:red\">x</p><img src=\"/a.png\">"),
        "x"
    );
    assert_eq!(
        kses_data("Tom & Jerry <script>alert(1)</script>"),
        "Tom &amp; Jerry "
    );
    assert_eq!(
        kses_data("<a href=\"javascript:alert(1)\">x</a>"),
        "<a>x</a>"
    );
    assert_eq!(kses_data("a<!--more-->b"), "ab");
}

#[test]
fn safecss_follows_wordpress() {
    let table: &[(&str, &str)] = &[
        ("color: red", "color: red"),
        (
            "color: red; text-align: center;",
            "color: red;text-align: center",
        ),
        (
            "position: absolute; z-index: 9",
            "position: absolute;z-index: 9",
        ),
        ("unknown-prop: 1; color: blue", "color: blue"),
        ("width: expression(alert(1))", ""),
        ("width: EXPRESSION(alert(1))", ""),
        ("color: red /* x */", ""),
        ("color: \\72 ed", ""),
        ("background: url(javascript:alert(1))", ""),
        ("background: url('javascript:alert(1)')", ""),
        (
            "background-image: url(\"https://example.com/a.png\")",
            "background-image: url(\"https://example.com/a.png\")",
        ),
        (
            "background-image: url(/a.png)",
            "background-image: url(/a.png)",
        ),
        ("background-image: url(data:image/png;base64,AAAA)", ""),
        ("color: url(https://example.com/a.png)", ""),
        ("behavior: url(x.htc)", ""),
        ("--x: javascript:alert(1)", ""),
        ("--x: 1px", "--x: 1px"),
        ("--x: url(javascript:alert(1))", ""),
        (
            "width: calc(100% - var(--gap, 2px))",
            "width: calc(100% - var(--gap, 2px))",
        ),
        ("width: calc(100% - (2px)", ""),
        ("color: red}body{color:blue", ""),
        ("color: red\nbackground: blue", "color: redbackground: blue"),
    ];
    for (input, expected) in table {
        assert_eq!(&safecss_filter_attr(input), expected, "{input:?}");
    }
}

#[test]
fn long_styles_are_filtered_in_linear_time() {
    // Unclosed calls and many `url()`s: quadratic work here would take minutes.
    let unclosed = format!("width: {}", "calc(".repeat(200_000));
    assert_eq!(safecss_filter_attr(&unclosed), "");
    let urls = format!("background: {}", "url(/a.png) ".repeat(100_000));
    assert_eq!(safecss_filter_attr(&urls), urls.trim());
    let html = format!("<p style=\"{unclosed}\">x</p>");
    assert_eq!(kses_post(&html), "<p>x</p>");
}

#[test]
fn authors_and_contributors_are_filtered_editors_and_administrators_are_not() {
    let sent = PostFields {
        title: "Hi <script>alert(1)</script><b>there</b>".to_owned(),
        content: "<p onclick=\"x()\">Body</p><iframe src=\"https://e.example\"></iframe>"
            .to_owned(),
        excerpt: "<em>Short</em><img src=x onerror=alert(1)>".to_owned(),
    };
    for role in [AUTHOR, CONTRIBUTOR] {
        let saved = kses_post_fields(&user(role), sent.clone());
        assert_eq!(saved.title, "Hi <b>there</b>", "{role}");
        assert_eq!(saved.content, "<p>Body</p>", "{role}");
        assert_eq!(saved.excerpt, "<em>Short</em><img src=\"x\">", "{role}");
    }
    for role in [EDITOR, ADMINISTRATOR] {
        assert_eq!(kses_post_fields(&user(role), sent.clone()), sent, "{role}");
    }
    // A user's own denial of `unfiltered_html` wins over their role.
    let denied = UserCaps::with_default_roles(
        1,
        &[EDITOR],
        &BTreeMap::from([("unfiltered_html".to_owned(), false)]),
    );
    assert_eq!(
        kses_post_fields(&denied, sent.clone()).content,
        "<p>Body</p>"
    );
}
