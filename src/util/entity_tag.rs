//! Entity-tag fields are comma-separated lists of quoted opaque tags, not CSV.

/// Repeated list field lines have the same semantics as one comma-joined value.
pub(crate) fn request_tag_field(
    req: &salvo::Request,
    name: &str,
) -> Result<Option<String>, salvo::http::StatusCode> {
    let mut combined = None::<String>;
    for value in req.headers().get_all(name) {
        let value = value
            .to_str()
            .map_err(|_| salvo::http::StatusCode::BAD_REQUEST)?;
        if let Some(combined) = &mut combined {
            combined.push(',');
            combined.push_str(value);
        } else {
            combined = Some(value.to_owned());
        }
    }
    Ok(combined)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EntityTagList<'a> {
    Any,
    Tags(Vec<EntityTag<'a>>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EntityTag<'a> {
    weak: bool,
    opaque: &'a str,
}

fn tag_at(value: &str, start: usize) -> Option<(EntityTag<'_>, usize)> {
    let bytes = value.as_bytes();
    let weak = bytes.get(start..)?.starts_with(b"W/");
    let quote = start + if weak { 2 } else { 0 };
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let mut end = quote + 1;
    while let Some(&byte) = bytes.get(end) {
        if byte == b'"' {
            return Some((
                EntityTag {
                    weak,
                    opaque: &value[quote + 1..end],
                },
                end + 1,
            ));
        }
        // RFC 9110 etagc: visible bytes other than DQUOTE, plus obs-text.
        if byte < 0x21 || byte == 0x7f {
            return None;
        }
        end += 1;
    }
    None
}

impl<'a> EntityTagList<'a> {
    pub(crate) fn parse(value: &'a str) -> Option<Self> {
        if value.trim_matches([' ', '\t']) == "*" {
            return Some(Self::Any);
        }
        let bytes = value.as_bytes();
        let mut index = 0;
        let mut tags = Vec::new();
        while index < bytes.len() {
            // Recipients tolerate empty list members; an asterisk is not a tag member.
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b','))
            {
                index += 1;
            }
            if index == bytes.len() {
                break;
            }
            let (tag, end) = tag_at(value, index)?;
            tags.push(tag);
            index = end;
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
            {
                index += 1;
            }
            if index < bytes.len() && bytes[index] != b',' {
                return None;
            }
        }
        Some(Self::Tags(tags))
    }

    pub(crate) fn matches(&self, current: Option<&str>, weak_comparison: bool) -> bool {
        if matches!(self, Self::Any) {
            return current.is_some();
        }
        let Some(current) = current else { return false };
        let Some((current_tag, end)) = tag_at(current, 0) else {
            return false;
        };
        if end != current.len() {
            return false;
        }
        let Self::Tags(tags) = self else {
            unreachable!()
        };
        tags.iter().any(|tag| {
            tag.opaque == current_tag.opaque
                && (weak_comparison || (!tag.weak && !current_tag.weak))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::EntityTagList;

    #[test]
    fn commas_and_asterisks_inside_tags_are_literal() {
        let tags = EntityTagList::parse(r#""prefix,*,suffix", W/"other,tag""#).unwrap();
        assert!(tags.matches(Some(r#""prefix,*,suffix""#), false));
        assert!(!tags.matches(Some(r#""unrelated""#), true));
        assert!(tags.matches(Some(r#""other,tag""#), true));
        assert!(!tags.matches(Some(r#""other,tag""#), false));
        assert!(
            EntityTagList::parse(" * ")
                .unwrap()
                .matches(Some(r#""tag""#), false)
        );
        assert!(!EntityTagList::parse("*").unwrap().matches(None, false));
        for value in [
            r#"*, "tag""#,
            r#""tag", *"#,
            r#"W/"unterminated"#,
            r#""tag"junk"#,
        ] {
            assert!(EntityTagList::parse(value).is_none(), "{value}");
        }
    }
}
