//! What's new in each version: CHANGELOG.md, built into the exe (shown in About), and the
//! same entry at the top of each GitHub release (shown in the update window).

const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// Where a release's "what's new" entry sits in its notes (written by release.yml).
pub const RELEASE_NOTES_START: &str = "<!-- whats-new -->";
pub const RELEASE_NOTES_END: &str = "<!-- /whats-new -->";

/// The entry for `version` in this build's changelog, as plain text.
pub fn notes_for(version: &str) -> Option<String> {
    section(CHANGELOG, version).map(plain).filter(|s| !s.is_empty())
}

/// The "what's new" entry from a GitHub release's notes, as plain text.
pub fn from_release_notes(body: &str) -> Option<String> {
    let start = body.find(RELEASE_NOTES_START)? + RELEASE_NOTES_START.len();
    let len = body[start..].find(RELEASE_NOTES_END)?;
    Some(plain(&body[start..start + len])).filter(|s| !s.is_empty())
}

/// The lines under `## <version>`, up to the next heading.
fn section<'a>(md: &'a str, version: &str) -> Option<&'a str> {
    let heading = |line: &str| {
        line.strip_prefix("## ")
            .map(|rest| rest.split_whitespace().next() == Some(version))
    };
    let start = md.lines().position(|l| heading(l) == Some(true))?;
    let mut offset = 0;
    let mut begin = None;
    for (i, line) in md.split_inclusive('\n').enumerate() {
        if i == start + 1 {
            begin = Some(offset);
        }
        if let Some(b) = begin
            && i > start
            && line.starts_with("## ")
        {
            return Some(&md[b..offset]);
        }
        offset += line.len();
    }
    begin.map(|b| &md[b..])
}

/// Markdown bullets as plain text: "- item" becomes "• item", continuation lines are
/// joined, **bold**, `code` and [links](url) lose their markup, and lines are separated
/// with CRLF (for Windows edit controls).
pub fn plain(md: &str) -> String {
    let mut items: Vec<String> = Vec::new();
    let mut in_comment = false;
    for line in md.lines() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.starts_with("<!--") {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            items.push(format!("\u{2022} {}", inline(item)));
        } else if let Some(last) = items.last_mut() {
            last.push(' ');
            last.push_str(&inline(t));
        } else {
            items.push(inline(t));
        }
    }
    items.join("\r\n")
}

fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    // [text](url) -> text
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|c| open + c) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|e| close + e) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_version_has_an_entry() {
        let v = env!("CARGO_PKG_VERSION");
        let notes = notes_for(v).unwrap_or_else(|| panic!("CHANGELOG.md has no entry for {v}: add one"));
        assert!(notes.starts_with('\u{2022}'), "{notes}");
    }

    #[test]
    fn sections_stop_at_the_next_heading() {
        let md = "# Changelog\n\n<!-- how\nto -->\n\n## 1.1.0\n\n- New **thing**\n  continued.\n- Fix in `x`, see [notes](https://e.invalid).\n\n## 1.0.0\n\n- First\n";
        assert_eq!(
            notes_for_in(md, "1.1.0").as_deref(),
            Some("\u{2022} New thing continued.\r\n\u{2022} Fix in x, see notes.")
        );
        assert_eq!(notes_for_in(md, "1.0.0").as_deref(), Some("\u{2022} First"));
        assert_eq!(notes_for_in(md, "1.0"), None);
        assert_eq!(notes_for_in(md, "2.0.0"), None);
    }

    fn notes_for_in(md: &str, v: &str) -> Option<String> {
        section(md, v).map(plain).filter(|s| !s.is_empty())
    }

    #[test]
    fn release_notes_entry() {
        let body =
            format!("## What's new\n\n{RELEASE_NOTES_START}\n- One\n- Two\n{RELEASE_NOTES_END}\n\n## Install\n...");
        assert_eq!(
            from_release_notes(&body).as_deref(),
            Some("\u{2022} One\r\n\u{2022} Two")
        );
        assert_eq!(from_release_notes("## Install\nno entry"), None);
    }

    #[test]
    fn every_entry_is_a_list() {
        for line in CHANGELOG.lines().filter(|l| l.starts_with("## ")) {
            let v = line[3..].split_whitespace().next().unwrap();
            assert!(notes_for(v).is_some(), "empty entry for {v}");
        }
    }
}
