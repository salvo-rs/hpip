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

// The HTML page to use as template for a requested directory's listing.
include!(concat!(env!("OUT_DIR"), "/directory_listing.html.rs"));

