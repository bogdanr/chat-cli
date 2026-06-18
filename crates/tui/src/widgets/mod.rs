pub mod chat_list;
pub mod message_list;

/// Wrap a pane/overlay block title with a one-cell margin on both sides so it
/// reads like a label instead of sitting flush against the border corner.
/// Trimming first makes this idempotent for titles that are already padded
/// (e.g. `" Thread "`), avoiding double spaces.
pub(crate) fn padded_title(title: impl AsRef<str>) -> String {
    format!(" {} ", title.as_ref().trim())
}
