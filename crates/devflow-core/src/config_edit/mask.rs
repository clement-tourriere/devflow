//! Masking of `[env]` values for display, in one place.
//!
//! A frontend must never be handed a recipe or a resolved value it could
//! print by mistake, so the views of this module's parent
//! ([`RecipeRow`](super::RecipeRow), [`PreviewRow`](super::PreviewRow)) are
//! built with these functions and carry only the masked text. They know the
//! *name* of the variable, which a string-only masker cannot: `APP_PASSWORD =
//! "hunter2"` has no URL, no `password=` assignment, nothing in its text for a
//! pattern to find. What is not masked here is not secret by name or by
//! content (`redact::text`), and a person who wants the real text asks for it
//! explicitly (`RecipeRow::reveal`, `PreviewRow::reveal`).
use crate::env_file::{self, Segment};
use crate::redact::{self, MASK};

/// A recipe's template (`NAME = "template"`) as it may be shown or serialized.
///
/// - A variable whose name says it holds a credential
///   ([`redact::is_secret_variable`]) and whose template has literal text is
///   shown as `***`: the literal is the credential. A template made only of
///   `${NAME}` references is shown as written, since a reference names a
///   variable and holds no value.
/// - Any other template has the credentials the patterns of
///   [`redact::text`] find replaced (`postgres://app:***@host/db`).
pub fn masked_template(key: &str, template: &str) -> String {
    if redact::is_secret_variable(key) && has_literal_text(template) {
        return MASK.to_string();
    }
    redact::text(template)
}

/// A resolved value (what a recipe or a preset produces in a workspace) as it
/// may be shown. `known_secrets` are the credentials devflow generated for the
/// workspace; each is masked wherever it appears. A variable whose name says
/// it holds a credential is shown as `***` whatever its value.
pub fn masked_value(key: &str, value: &str, known_secrets: &[&str]) -> String {
    if redact::is_secret_variable(key) && !value.is_empty() {
        return MASK.to_string();
    }
    redact::text_with(value, known_secrets)
}

/// The template has something besides `${NAME}` references. A template that
/// cannot be read counts as text: better masked than guessed at.
pub(crate) fn has_literal_text(template: &str) -> bool {
    match env_file::parse_template(template) {
        Ok(segments) => segments
            .iter()
            .any(|segment| matches!(segment, Segment::Text(text) if !text.is_empty())),
        Err(_) => true,
    }
}
