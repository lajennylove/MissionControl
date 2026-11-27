//! A refused save's message on its form field, for the admin's runtime screens (the settings
//! screens and the Menus screen), as WordPress shows a refused value next to its form.
//!
//! Ignite 0.6.5 shows one kind of data-layer refusal on the form: a unique-index violation named
//! `{slug}_{field}_unique`, which its form answers (422, the typed values kept) with
//! "<label> is already taken." under the field. It has no way yet for a runtime resource's data
//! layer to return a field message of its own (owner item O10). Until it has, a refusal is
//! answered that way ([`refuse_field`]), and the app's response fairing replaces Ignite's text
//! with the refusal's own message ([`with_field_messages`]).
//!
//! The messages are kept by screen and field, the last one of each. Two saves refused at the
//! same moment on the same field of the same screen could show each other's message; for the
//! settings screens the message of a field is always the same.
//
// TODO(Ignite field errors): when a runtime resource's data layer can return field errors,
// return them, and remove this module and the fairing.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use orbit_orm::Error;

/// (screen, field) => (the field's label, the message).
type Messages = HashMap<(String, String), (String, String)>;

static MESSAGES: LazyLock<Mutex<Messages>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The error that refuses a save of `screen` with `message` on its field `field` (labelled
/// `label` on the form): Ignite answers it with the form (422) and the field's error, which
/// [`with_field_messages`] makes `message`.
pub(crate) fn refuse_field(screen: &str, field: &str, label: &str, message: &str) -> Error {
    MESSAGES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            (screen.to_owned(), field.to_owned()),
            (label.to_owned(), message.to_owned()),
        );
    Error::UniqueViolation {
        index: Some(format!("{screen}_{field}_unique")),
    }
}

/// `html`, Ignite's form answer of a refused save of `screen`, with the error Ignite shows for
/// each field refused by [`refuse_field`] ("<label> is already taken.") replaced by the
/// refusal's message. `None` when nothing was replaced.
pub fn with_field_messages(screen: &str, html: &str) -> Option<String> {
    let messages: Vec<(String, String)> = MESSAGES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|((s, _), _)| s == screen)
        .map(|(_, (label, message))| (label.clone(), message.clone()))
        .collect();
    let mut out = html.to_owned();
    let mut replaced = false;
    for (label, message) in messages {
        let ignite = format!(">{} is already taken.<", escape(&label));
        if out.contains(&ignite) {
            out = out.replace(&ignite, &format!(">{}<", escape(&message)));
            replaced = true;
        }
    }
    replaced.then_some(out)
}

/// `text` escaped for HTML text and attributes.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_message_replaces_ignites_text_on_its_screen_only() {
        let error = refuse_field("test-screen", "email", "Email <address>", "Not \"valid\".");
        assert!(matches!(
            error,
            Error::UniqueViolation { index: Some(ref i) } if i == "test-screen_email_unique"
        ));
        let html = "<p class=\"e\">Email &lt;address&gt; is already taken.</p>";
        assert_eq!(
            with_field_messages("test-screen", html).as_deref(),
            Some("<p class=\"e\">Not &quot;valid&quot;.</p>")
        );
        assert_eq!(with_field_messages("other-screen", html), None);
        assert_eq!(with_field_messages("test-screen", "<p>Name</p>"), None);
    }
}
