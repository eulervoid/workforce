//! Showing an error together with its causes.
//!
//! An error's own message does not repeat its source (`thiserror` variants wrap with `#[from]`),
//! so wherever an error becomes text without passing through `anyhow`, the chain is joined here.

use std::error::Error;

/// `outer: inner: innermost`
pub fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[from] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    #[test]
    fn causes_follow_the_error() {
        assert_eq!(error_chain(&Outer(Inner)), "outer: inner");
        assert_eq!(error_chain(&Inner), "inner");
    }
}
