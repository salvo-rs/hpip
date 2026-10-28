#![allow(clippy::too_many_arguments)]

use std::{fmt, mem};

pub trait HtmlResponseElement {
    fn commit(self, data: &mut Vec<u8>);
}

impl HtmlResponseElement for &str {
    fn commit(self, data: &mut Vec<u8>) {
        data.extend(self.as_bytes());
    }
}

impl HtmlResponseElement for String {
    fn commit(self, data: &mut Vec<u8>) {
        data.extend(self.as_bytes());
    }
}

impl<'s> HtmlResponseElement for fmt::Arguments<'s> {
    fn commit(self, data: &mut Vec<u8>) {
        let mut orig = unsafe { String::from_utf8_unchecked(std::mem::take(data)) };
        let _ = fmt::write(&mut orig, self);
        let _ = mem::replace(data, orig.into_bytes());
    }
}

impl<F: FnOnce(&mut Vec<u8>)> HtmlResponseElement for F {
    fn commit(self, data: &mut Vec<u8>) {
        self(data)
    }
}

// The generic HTML page to use as response to errors.
include!(concat!(env!("OUT_DIR"), "/error.html.rs"));

/// Render untrusted error details as text, keeping the raw template for trusted markup.
pub fn error_html_text(
    title: &str,
    message: impl fmt::Display,
    details: impl fmt::Display,
) -> String {
    fn escaped(value: impl fmt::Display) -> String {
        let mut output = String::new();
        for character in value.to_string().chars() {
            match character {
                '&' => output.push_str("&amp;"),
                '<' => output.push_str("&lt;"),
                '>' => output.push_str("&gt;"),
                '"' => output.push_str("&quot;"),
                '\'' => output.push_str("&#39;"),
                _ => output.push(character),
            }
        }
        output
    }
    let title = escaped(title);
    error_html(title.as_str(), escaped(message), escaped(details))
}

// The HTML page to use as template for a requested directory's listing.
include!(concat!(env!("OUT_DIR"), "/directory_listing.html.rs"));

#[cfg(test)]
mod error_text_tests {
    #[test]
    fn dynamic_error_fields_are_text_even_in_attributes() {
        let html = super::error_html_text("Error \"<&", "<img data-review='marker'>", "a&b");
        assert!(!html.contains("<img"));
        assert!(html.contains("Error &quot;&lt;&amp;"));
        assert!(html.contains("&lt;img data-review=&#39;marker&#39;&gt;"));
        assert!(html.contains("a&amp;b"));
    }
}
