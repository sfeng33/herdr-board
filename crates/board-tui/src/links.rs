//! [fork] Link buttons in the card detail.
//!
//! Any line of the card description or of a comment of the form
//! `Issue: <url>`, `PR: <url>` or `Explainer: <url>` becomes a button. Links
//! are deduplicated by URL, the description's first, then comments oldest to
//! newest, and capped at nine so each one has a digit key.

use board_core::protocol::CardDetail;

pub const MAX_LINKS: usize = 9;

const KINDS: [&str; 3] = ["Issue", "PR", "Explainer"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardLink {
    pub label: String,
    pub url: String,
}

/// `Some((kind, url))` when `line` is a link line.
fn parse_line(line: &str) -> Option<(&'static str, &str)> {
    let line = line.trim();
    KINDS.iter().find_map(|kind| {
        let url = line.strip_prefix(kind)?.strip_prefix(':')?.trim();
        let is_url = (url.starts_with("https://") || url.starts_with("http://"))
            && !url.contains(char::is_whitespace);
        is_url.then_some((*kind, url))
    })
}

fn label(kind: &str, url: &str) -> String {
    let number = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if kind != "Explainer" && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
        format!("{kind} #{number}")
    } else {
        kind.to_string()
    }
}

pub fn card_links(detail: &CardDetail) -> Vec<CardLink> {
    let texts = std::iter::once(detail.card.description.as_str())
        .chain(detail.comments.iter().map(|comment| comment.body.as_str()));
    let mut links: Vec<CardLink> = Vec::new();
    for (kind, url) in texts.flat_map(str::lines).filter_map(parse_line) {
        if links.len() == MAX_LINKS {
            break;
        }
        if !links.iter().any(|link| link.url == url) {
            links.push(CardLink {
                label: label(kind, url),
                url: url.to_string(),
            });
        }
    }
    links
}

/// The description without its link lines, which the buttons replace.
pub fn description_without_links(description: &str) -> String {
    description
        .lines()
        .filter(|line| parse_line(line).is_none())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start_matches('\n')
        .to_string()
}

/// A comment body with each link line shortened to its label, e.g.
/// `Explainer` or `PR #13`; the URL stays reachable through the buttons.
pub fn comment_display(body: &str) -> String {
    if !body.lines().any(|line| parse_line(line).is_some()) {
        return body.to_string();
    }
    body.lines()
        .map(|line| match parse_line(line) {
            Some((kind, url)) => label(kind, url),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of button `index` (0-based), e.g. `[1 Issue #60460]`.
pub fn button_text(index: usize, link: &CardLink) -> String {
    format!("[{} {}]", index + 1, link.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_labels_and_ignores_other_lines() {
        assert_eq!(
            parse_line("Issue: https://github.com/o/r/issues/12"),
            Some(("Issue", "https://github.com/o/r/issues/12"))
        );
        assert_eq!(
            parse_line("PR:https://x.test/pull/3 "),
            Some(("PR", "https://x.test/pull/3"))
        );
        assert_eq!(parse_line("Issue: not a url"), None);
        assert_eq!(parse_line("Issues: https://x.test/1"), None);
        assert_eq!(parse_line("see Issue: https://x.test/1"), None);
        assert_eq!(
            label("Issue", "https://github.com/o/r/issues/12"),
            "Issue #12"
        );
        assert_eq!(label("PR", "https://github.com/o/r/pull/7/"), "PR #7");
        assert_eq!(
            label("Explainer", "https://claude.ai/artifact/9"),
            "Explainer"
        );
    }

    #[test]
    fn strips_link_lines_from_the_description() {
        let description = "Issue: https://x.test/issues/1\nPR: https://x.test/pull/2\nFix it.";
        assert_eq!(description_without_links(description), "Fix it.");
        assert_eq!(description_without_links("plain text"), "plain text");
    }

    #[test]
    fn shortens_link_lines_in_comments_to_their_labels() {
        assert_eq!(
            comment_display("Explainer: https://claude.ai/artifact/9"),
            "Explainer"
        );
        assert_eq!(
            comment_display("Opened it.\nPR: https://github.com/o/r/pull/7"),
            "Opened it.\nPR #7"
        );
        assert_eq!(comment_display("no links here"), "no links here");
    }
}
