//! System clipboard access shared by the composer and the conversation.

pub(super) fn copy(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|error| format!("Clipboard unavailable: {error}"))
}
