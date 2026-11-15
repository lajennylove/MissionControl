//! Tests for kses, WordPress's HTML filter on save: `wp_kses_post` with `$allowedposttags`,
//! `wp_filter_kses` with `$allowedtags`, `safecss_filter_attr` for the `style` attribute, and the
//! save-path filter that runs them for a user without `unfiltered_html` (titles as plain text,
//! Ruling R39), and the nesting cap that keeps parsing linear. Pure logic: no database.

use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use mission_control_core::content::{
    FieldTooLarge, MAX_ACTIVE_FORMATTING, MAX_FILTERED_BYTES, MAX_NESTING, PostFields, kses_data,
    kses_post, kses_post_fields, kses_title, safecss_filter_attr,
};
use mission_control_core::users::{ADMINISTRATOR, AUTHOR, CONTRIBUTOR, EDITOR, UserCaps};

/// The limit for the timing smoke checks: generous, so a slow debug build on CI does not flake;
/// the quadratic cases they guard against take tens of seconds there. Which bound refuses an
/// input is checked by the pre-scan's own unit tests, independent of speed.
const SMOKE_LIMIT: Duration = Duration::from_secs(10);

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
        if kept.is_empty() {
            // Nothing of the input is left.
            assert_eq!(output, "", "{input:?}");
        } else {
            assert!(
                output.contains(kept),
                "{input:?} gave {output:?}, without {kept:?}"
            );
        }
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
        // WordPress compares property names exactly (`in_array(..., true)`, no lower-casing).
        ("COLOR: red", ""),
        ("Color: red; color: blue", "color: blue"),
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
fn deep_nesting_is_refused_in_linear_time() {
    // html5ever scans its stack of open elements, so 100,000 nested elements would take
    // seconds; past the cap the input is not parsed but kept as escaped text.
    let started = Instant::now();
    let deep = "<div>".repeat(100_000);
    let output = kses_post(&deep);
    assert!(!output.contains('<'), "refused input is text");
    assert!(output.starts_with("&lt;div&gt;&lt;div&gt;"));
    assert_eq!(kses_data(&deep), output);
    // A title past the cap is kept as typed: plain text, escaped when it is printed.
    assert_eq!(
        kses_title(&"<b>".repeat(MAX_NESTING + 1)),
        "<b>".repeat(MAX_NESTING + 1)
    );
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());

    // Up to the cap, nesting is parsed.
    let at_cap = format!(
        "{}x{}",
        "<div>".repeat(MAX_NESTING),
        "</div>".repeat(MAX_NESTING)
    );
    assert_eq!(kses_post(&at_cap), at_cap);
    let past_cap = format!("<div>{at_cap}</div>");
    assert!(kses_post(&past_cap).starts_with("&lt;div&gt;"));

    // Closed elements do not count, nor do elements that close themselves or are void.
    let siblings = "<div><span>x</span><img src=\"a.png\"><br></div>".repeat(5_000);
    assert_eq!(kses_post(&siblings), siblings);
    let lists = "<ul><li>a<li>b</ul><dl><dt>t<dd>d</dl><p>unclosed".repeat(1_000);
    assert!(kses_post(&lists).starts_with("<ul><li>a</li><li>b</li></ul>"));
    let tables = "<div><table><tr><td>x</table><p>y</div>".repeat(1_000);
    assert!(kses_post(&tables).starts_with("<div><table>"));
    let comments = "<div><!-- </span> --><p>x</div><script>if (a<b) {}</script>".repeat(1_000);
    assert!(kses_post(&comments).starts_with("<div><p>x</p></div>"));
    let attributes = "<a href=\"/x?a=1&b=2\" title='>' data-x=y>z</a><hr/>".repeat(1_000);
    assert!(kses_post(&attributes).starts_with("<a href="));

    // End tags the parser ignores do not close anything: in a comment, an attribute value, a
    // script, a CDATA section, a bogus comment, or one that does not match the open element.
    for unit in [
        "<div><!--</div>-->",
        "<div><!--x--!</div>-->",
        "<div><!---</div>-->",
        "<div title=\"</div>\">",
        "<div title='</div>' a=b>",
        "<div><script></div></script>",
        "<div><textarea></div></textarea>",
        "<div><![CDATA[</div>]]>",
        "<div><?</div>",
        "<div></span>",
        "<x><div></x>",
        "<rt>",
        "<option><optgroup>",
    ] {
        let attack = unit.repeat(MAX_NESTING + 1);
        let started = Instant::now();
        let output = kses_post(&attack);
        assert!(
            output.starts_with("&lt;"),
            "{unit:?} was parsed: {}",
            &output[..output.len().min(80)]
        );
        assert!(started.elapsed() < SMOKE_LIMIT, "{unit:?}");
    }
    // Markup inside raw text counts when the input has SVG or MathML, where the parser reads it
    // as markup.
    let svg = format!(
        "<svg><style>{}</style></svg>",
        "<div>".repeat(MAX_NESTING + 1)
    );
    assert!(kses_post(&svg).starts_with("&lt;"));
    let math = format!("<MATH><title>{}</title>", "<div>".repeat(MAX_NESTING + 1));
    assert!(kses_post(&math).starts_with("&lt;"));
    let cdata = format!("<![CDATA[>{}]]>", "<div>".repeat(MAX_NESTING + 1));
    assert!(kses_post(&cdata).starts_with("&lt;"));
    // In SVG and MathML, void and self-closing HTML names are ordinary elements that nest.
    for name in ["link", "image", "tr", "p", "html", "img"] {
        let foreign = format!("<svg>{}", format!("<{name}>").repeat(MAX_NESTING + 1));
        assert!(kses_post(&foreign).starts_with("&lt;"), "{name}");
    }
    let plaintext = format!("<svg><plaintext>{}", "<div>".repeat(MAX_NESTING + 1));
    assert!(kses_post(&plaintext).starts_with("&lt;"));
    let script_escape = format!(
        "<script><!--<script>{}",
        "</script><div>".repeat(MAX_NESTING + 1)
    );
    assert!(kses_post(&script_escape).starts_with("&lt;"));
}

#[test]
fn reopened_formatting_elements_are_refused_in_bounded_time() {
    // The parser reopens every unclosed formatting element (`<b>`, `<em>`…) in each new block:
    // 512 of them and 60,000 paragraphs made 516 MB of output from 486 KB.
    let unclosed =
        |count: usize| -> String { (0..count).map(|k| format!("<b id=a{k}>")).collect() };
    let amplified = format!("<p>{}</p>{}", unclosed(512), "<p>x</p>".repeat(20_000));
    let started = Instant::now();
    let output = kses_post(&amplified);
    assert!(output.starts_with("&lt;p&gt;&lt;b"), "refused");
    assert!(output.len() <= amplified.len() * 5);
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());

    // Up to the cap the formatting elements are parsed; one more is refused.
    let at_cap = format!("<p>{}</p><p>x</p>", unclosed(MAX_ACTIVE_FORMATTING));
    assert!(kses_post(&at_cap).starts_with("<p><b id=\"a0\">"));
    let past_cap = format!("<p>{}</p><p>x</p>", unclosed(MAX_ACTIVE_FORMATTING + 1));
    assert!(kses_post(&past_cap).starts_with("&lt;"));
    // Misnested end tags that leave the element in the parser's list do not close it.
    assert!(kses_post(&"<b><p></b>".repeat(MAX_ACTIVE_FORMATTING + 1)).starts_with("&lt;"));
    assert!(kses_post(&"<i><div></i></div>".repeat(MAX_ACTIVE_FORMATTING + 1)).starts_with("&lt;"));

    // Under the cap, the reopening budget refuses it: 32 elements reopened in each of thousands
    // of paragraphs (the output budget would only catch it after the parser built them all).
    let under_cap = format!(
        "<p>{}</p>{}",
        unclosed(MAX_ACTIVE_FORMATTING),
        "<p>x</p>".repeat(5_000)
    );
    // Refused by the pre-scan's reopening budget, before the parser builds anything (the
    // pre-scan's own unit tests check which bound refuses it).
    let started = Instant::now();
    assert!(kses_post(&under_cap).starts_with("&lt;"));
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());
    // Paragraphs that close the last one implicitly reopen the formatting elements too.
    let implicit = format!(
        "<p>{}{}",
        unclosed(MAX_ACTIVE_FORMATTING),
        "<p>x".repeat(20_000)
    );
    assert!(kses_post(&implicit).starts_with("&lt;"));
    let divs = format!(
        "<p>{}{}",
        unclosed(MAX_ACTIVE_FORMATTING),
        "<div>x</div>".repeat(20_000)
    );
    assert!(kses_post(&divs).starts_with("&lt;"));
    let four_unclosed = format!("<p>{}</p>{}", unclosed(4), "<p>x</p>".repeat(20_000));
    assert!(kses_post(&four_unclosed).starts_with("&lt;"));
    // One unclosed formatting element is reopened once per paragraph, as browsers do; parsed.
    let one_unclosed = format!("<p><b>x</p>{}", "<p>y</p>".repeat(20_000));
    assert!(kses_post(&one_unclosed).starts_with("<p><b>x</b></p><p><b>y</b></p>"));
    let ordinary = format!("<p><b>x</p>{}", "<p>y <em>z</em></p>".repeat(1_000));
    assert!(kses_post(&ordinary).starts_with("<p><b>x</b></p><p><b>y <em>z</em></b></p>"));

    // Closed formatting, any amount, is parsed; so is text that escapes to five times its size.
    let closed =
        "<p><b>x</b> <em>y</em> <a href=\"/\">z</a> <strong><i>w</i></strong></p>".repeat(5_000);
    assert_eq!(kses_post(&closed), closed);
    let ampersands = "&".repeat(20_000);
    assert_eq!(kses_post(&ampersands), "&amp;".repeat(20_000));
    let unclosed_paragraphs = "<p><b>x</p>".repeat(20);
    assert!(kses_post(&unclosed_paragraphs).starts_with("<p><b>x</b></p>"));
}

#[test]
fn many_ampersands_are_refused_in_bounded_time() {
    // html5ever 0.40's serialiser searches the rest of a text or attribute value once per `&`
    // it escapes, so 400,000 `&` took seconds to write.
    let started = Instant::now();
    let text = format!("{}<b>x</b>", "a&".repeat(200_000));
    let output = kses_post(&text);
    assert!(output.ends_with("&lt;b&gt;x&lt;/b&gt;"), "refused");
    assert!(kses_data(&text).ends_with("&lt;b&gt;x&lt;/b&gt;"));
    assert_eq!(kses_title(&text), text);
    let attribute = format!("<a title=\"{}\">x</a>", "&".repeat(400_000));
    assert!(kses_post(&attribute).starts_with("&lt;a title"));
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());
    // Ampersands in ordinary amounts are parsed.
    let links = "<p><a href=\"/?a=1&amp;b=2\">Tom &amp; Jerry</a></p>".repeat(2_000);
    assert_eq!(kses_post(&links), links);
}

#[test]
fn titles_are_plain_text() {
    // Ruling R39: a title is text, so tags are stripped and entities decoded once.
    assert_eq!(kses_title("A & <em>B</em>"), "A & B");
    assert_eq!(kses_title("Tom &amp; Jerry"), "Tom & Jerry");
    assert_eq!(kses_title("&amp;lt;b&amp;gt;"), "&lt;b&gt;");
    assert_eq!(kses_title("&lt;b&gt; is bold"), "<b> is bold");
    assert_eq!(kses_title("a < b > c"), "a < b > c");
    assert_eq!(
        kses_title("Caf&eacute;&nbsp;\"x\" 'y'"),
        "Caf\u{e9}\u{a0}\"x\" 'y'"
    );
    assert_eq!(
        kses_title("Hi <script>alert(1)</script><!-- c -->there"),
        "Hi there"
    );
    assert_eq!(kses_title("<a href=\"javascript:x\">link</a>"), "link");
}

#[test]
fn targeted_links_get_no_rel_added() {
    // WordPress 5.1 added `rel="noopener"` to links with a `target` on save
    // (`wp_targeted_link_rel`); WordPress 6.7 deprecated it and no longer runs it.
    let link = "<a href=\"https://example.com/\" target=\"_blank\">x</a>";
    assert_eq!(kses_post(link), link);
    let fields = PostFields {
        content: link.to_owned(),
        ..PostFields::default()
    };
    for role in [AUTHOR, EDITOR] {
        assert_eq!(
            kses_post_fields(&user(role), fields.clone())
                .unwrap()
                .content,
            link,
            "{role}"
        );
    }
}

#[test]
fn authors_and_contributors_are_filtered_editors_and_administrators_are_not() {
    let sent = PostFields {
        title: "A & <em>B</em><script>alert(1)</script>".to_owned(),
        content: "<p onclick=\"x()\">Body</p><iframe src=\"https://e.example\"></iframe>"
            .to_owned(),
        excerpt: "<em>Short</em><img src=x onerror=alert(1)>".to_owned(),
    };
    for role in [AUTHOR, CONTRIBUTOR] {
        let saved = kses_post_fields(&user(role), sent.clone()).unwrap();
        assert_eq!(saved.title, "A & B", "{role}");
        assert_eq!(saved.content, "<p>Body</p>", "{role}");
        assert_eq!(saved.excerpt, "<em>Short</em><img src=\"x\">", "{role}");
    }
    for role in [EDITOR, ADMINISTRATOR] {
        assert_eq!(
            kses_post_fields(&user(role), sent.clone()),
            Ok(sent.clone()),
            "{role}"
        );
    }
    // A user's own denial of `unfiltered_html` wins over their role.
    let denied = UserCaps::with_default_roles(
        1,
        &[EDITOR],
        &BTreeMap::from([("unfiltered_html".to_owned(), false)]),
    );
    assert_eq!(
        kses_post_fields(&denied, sent.clone()).unwrap().content,
        "<p>Body</p>"
    );
}

#[test]
fn fields_too_large_to_filter_are_refused_not_stored() {
    // Ruling R42: past 256 KB, a user without `unfiltered_html` gets an error to report.
    let large = PostFields {
        content: "a".repeat(MAX_FILTERED_BYTES + 1),
        ..PostFields::default()
    };
    let error = kses_post_fields(&user(AUTHOR), large.clone()).unwrap_err();
    assert_eq!(
        error,
        FieldTooLarge {
            field: "content",
            size: MAX_FILTERED_BYTES + 1
        }
    );
    assert!(error.to_string().contains("content"));
    let large_title = PostFields {
        title: "t".repeat(MAX_FILTERED_BYTES + 1),
        ..PostFields::default()
    };
    assert_eq!(
        kses_post_fields(&user(CONTRIBUTOR), large_title)
            .unwrap_err()
            .field,
        "title"
    );
    let large_excerpt = PostFields {
        excerpt: "e".repeat(MAX_FILTERED_BYTES + 1),
        ..PostFields::default()
    };
    assert_eq!(
        kses_post_fields(&user(AUTHOR), large_excerpt)
            .unwrap_err()
            .field,
        "excerpt"
    );
    // At the cap it is filtered; editors are not capped (stored as sent).
    let at_cap = PostFields {
        content: "a".repeat(MAX_FILTERED_BYTES),
        ..PostFields::default()
    };
    assert_eq!(kses_post_fields(&user(AUTHOR), at_cap.clone()), Ok(at_cap));
    assert_eq!(kses_post_fields(&user(EDITOR), large.clone()), Ok(large));
}

#[test]
fn latin1_symbols_in_bulk_are_refused_in_bounded_time() {
    // html5ever's serialiser also rescans after every 0xC2 byte (no-break space, ©, °, «…).
    let started = Instant::now();
    for text in [
        format!("<p>{}</p>", "\u{a0}".repeat(150_000)),
        format!("<p>{}</p>", "\u{b0}".repeat(150_000)),
        format!("<a title=\"{}\">x</a>", "\u{a9}".repeat(150_000)),
    ] {
        assert!(kses_post(&text).starts_with("&lt;"));
    }
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());
    // In ordinary amounts they are kept.
    let ordinary = "<p>20\u{b0}C \u{ab}caf\u{e9}\u{bb}&nbsp;\u{a9}</p>".repeat(1_000);
    assert_eq!(kses_post(&ordinary), ordinary.replace('\u{a0}', "&nbsp;"));
}

#[test]
fn unclosed_cells_and_paragraph_ends_are_scanned_in_linear_time() {
    // Uncounted elements piling up, then end tags looking for a paragraph: quadratic in the
    // round-2 pre-scan (640 KB took 2 s in a release build).
    let started = Instant::now();
    for html in [
        format!("{}{}", "<td>".repeat(40_000), "</p>".repeat(40_000)),
        format!("{}{}", "<td>".repeat(30_000), "</i>x</div>".repeat(30_000)),
    ] {
        kses_post(&html);
    }
    assert!(started.elapsed() < SMOKE_LIMIT, "{:?}", started.elapsed());
}
