//! Conversion between the composer/renderer's markdown dialect and the
//! single-character "chat markup" used by WhatsApp and Slack.
//!
//! | Style   | Composer / renderer | WhatsApp / Slack |
//! |---------|---------------------|------------------|
//! | bold    | `**x**`             | `*x*`            |
//! | italic  | `_x_`               | `_x_`            |
//! | strike  | `~~x~~`             | `~x~`            |
//! | code    | `` `x` ``           | `` `x` ``        |
//!
//! Both directions leave code spans (inline and fenced), URLs and
//! angle-bracket tokens (`<@U1>`, `<!here>`, `<https://…|label>`) untouched,
//! and only convert delimiter pairs that follow word-boundary rules, so
//! literal asterisks and tildes in prose (`2*3*4`, `~/path`) survive.

/// Chat markup (`*bold*`, `~strike~`) → renderer markdown (`**bold**`,
/// `~~strike~~`). Italic (`_x_`) and code are the same in both dialects.
pub fn chat_markup_to_markdown(text: &str) -> String {
    map_outside_code_spans(text, |segment| {
        map_lines_outside_literals(segment, |line| {
            let line = convert_single_delimiter_line(line, '*', "**");
            convert_single_delimiter_line(&line, '~', "~~")
        })
    })
}

/// Renderer markdown (`**bold**`, `__bold__`, `~~strike~~`) → chat markup
/// (`*bold*`, `~strike~`), for sending to WhatsApp and Slack.
pub fn markdown_to_chat_markup(text: &str) -> String {
    map_outside_code_spans(text, |segment| {
        map_lines_outside_literals(segment, |line| {
            let line = convert_double_delimiter_line(line, "**", '*');
            let line = convert_double_delimiter_line(&line, "__", '*');
            convert_double_delimiter_line(&line, "~~", '~')
        })
    })
}

/// Applies `transform` to the parts of `text` outside backtick code spans
/// (inline and fenced), copying the code spans verbatim.
pub fn map_outside_code_spans(text: &str, transform: impl Fn(&str) -> String) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        output.push_str(&transform(&rest[..start]));
        let code = &rest[start..];
        let fence = if code.starts_with("```") { "```" } else { "`" };
        match code[fence.len()..].find(fence) {
            Some(end) => {
                let code_end = fence.len() + end + fence.len();
                output.push_str(&code[..code_end]);
                rest = &code[code_end..];
            }
            None => {
                output.push_str(code);
                return output;
            }
        }
    }
    output.push_str(&transform(rest));
    output
}

/// Rewrites single-character delimiter pairs on one line into doubled
/// markers. A pair only converts when it opens before a non-space, closes
/// after a non-space, and sits on word boundaries.
pub fn convert_single_delimiter_line(line: &str, delimiter: char, replacement: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut output = String::with_capacity(line.len() + 8);
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == delimiter
            && single_delimiter_opens(&chars, index, delimiter)
            && let Some(close) = single_delimiter_close(&chars, index, delimiter)
        {
            output.push_str(replacement);
            output.extend(&chars[index + 1..close]);
            output.push_str(replacement);
            index = close + 1;
        } else {
            output.push(chars[index]);
            index += 1;
        }
    }
    output
}

fn single_delimiter_opens(chars: &[char], index: usize, delimiter: char) -> bool {
    let preceded_ok =
        index == 0 || (!chars[index - 1].is_alphanumeric() && chars[index - 1] != delimiter);
    let next = chars.get(index + 1);
    preceded_ok && next.is_some_and(|next| !next.is_whitespace() && *next != delimiter)
}

fn single_delimiter_close(chars: &[char], open: usize, delimiter: char) -> Option<usize> {
    (open + 2..chars.len()).find(|&index| {
        chars[index] == delimiter
            && !chars[index - 1].is_whitespace()
            && chars[index - 1] != delimiter
            && chars
                .get(index + 1)
                .is_none_or(|next| !next.is_alphanumeric() && *next != delimiter)
    })
}

/// Rewrites doubled delimiter pairs (`**x**`) on one line into a single
/// character (`*x*`), with the same boundary rules as the inbound direction.
fn convert_double_delimiter_line(line: &str, marker: &str, replacement: char) -> String {
    let chars: Vec<char> = line.chars().collect();
    let marker: Vec<char> = marker.chars().collect();
    let width = marker.len();
    let at = |index: usize| chars.get(index..index + width) == Some(marker.as_slice());
    let mut output = String::with_capacity(line.len());
    let mut index = 0;
    while index < chars.len() {
        if at(index) {
            let preceded_ok = index == 0 || !chars[index - 1].is_alphanumeric();
            let opens = preceded_ok
                && chars
                    .get(index + width)
                    .is_some_and(|next| !next.is_whitespace() && *next != marker[0]);
            let close = opens
                .then(|| {
                    (index + width + 1..chars.len()).find(|&close| {
                        at(close)
                            && !chars[close - 1].is_whitespace()
                            && chars
                                .get(close + width)
                                .is_none_or(|next| !next.is_alphanumeric() && *next != marker[0])
                    })
                })
                .flatten();
            if let Some(close) = close {
                output.push(replacement);
                output.extend(&chars[index + width..close]);
                output.push(replacement);
                index = close + width;
                continue;
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

/// Splits `text` into lines and, within each line, applies `transform` only
/// to the runs between URLs and angle-bracket tokens, which are copied
/// verbatim.
fn map_lines_outside_literals(text: &str, transform: impl Fn(&str) -> String) -> String {
    text.split('\n')
        .map(|line| map_outside_literals(line, &transform))
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_outside_literals(line: &str, transform: &impl Fn(&str) -> String) -> String {
    let mut output = String::with_capacity(line.len());
    let mut plain_start = 0;
    let mut index = 0;
    let bytes = line.as_bytes();
    while index < line.len() {
        let literal_end = literal_token_end(line, index);
        if let Some(end) = literal_end {
            output.push_str(&transform(&line[plain_start..index]));
            output.push_str(&line[index..end]);
            index = end;
            plain_start = end;
            continue;
        }
        // Advance one UTF-8 character.
        index += 1;
        while index < line.len() && (bytes[index] & 0xC0) == 0x80 {
            index += 1;
        }
    }
    output.push_str(&transform(&line[plain_start..]));
    output
}

/// If a URL or an angle-bracket token starts at byte `index` (at a word
/// boundary), returns the byte offset where it ends.
fn literal_token_end(line: &str, index: usize) -> Option<usize> {
    let rest = &line[index..];
    let at_boundary = line[..index]
        .chars()
        .next_back()
        .is_none_or(|previous| !previous.is_alphanumeric());
    if !at_boundary {
        return None;
    }
    if rest.starts_with('<') {
        let close = rest.find('>')?;
        let token = &rest[1..close];
        let is_token = !token.is_empty()
            && !token.contains(char::is_whitespace)
            && (token.starts_with(['@', '!', '#'])
                || token.contains("://")
                || token.starts_with("mailto:"));
        // Slack links may carry a label with spaces: `<https://x|two words>`.
        let is_labelled_link = token
            .split_once('|')
            .is_some_and(|(url, _)| url.contains("://") && !url.contains(char::is_whitespace));
        return (is_token || is_labelled_link).then_some(index + close + 1);
    }
    let lower = rest.get(..8).unwrap_or(rest).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.") {
        let length = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return Some(index + length);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_converts_bold_and_strike_but_keeps_italic_and_code() {
        assert_eq!(
            markdown_to_chat_markup("**bold** _it_ ~~gone~~ `**code**`"),
            "*bold* _it_ ~gone~ `**code**`"
        );
        assert_eq!(markdown_to_chat_markup("__bold__"), "*bold*");
        assert_eq!(markdown_to_chat_markup("**_both_**"), "*_both_*");
    }

    #[test]
    fn inbound_converts_single_markers() {
        assert_eq!(
            chat_markup_to_markdown("*bold* _it_ ~gone~ `*code*`"),
            "**bold** _it_ ~~gone~~ `*code*`"
        );
    }

    #[test]
    fn literals_in_prose_survive_both_directions() {
        // Text without formatting pairs is unchanged in both directions.
        for text in [
            "2*3*4 and ~/path and a ** b",
            "snake_case_name and file_v2_final",
            "see https://example.com/a__b**c**d~~e~~ ok",
            "see https://example.com/a_b*c*d~e~ ok",
            "ping <@U123> and <!here> and <https://x.io/**a**|a label>",
            "```\n**fenced**\n*still*\n```",
        ] {
            assert_eq!(markdown_to_chat_markup(text), text);
        }
        for text in [
            "2*3*4 and ~/path",
            "see https://example.com/a_b*c*d~e~ ok",
            "<https://x.io/*a*|a label>",
            "```\n*fenced*\n```",
        ] {
            assert_eq!(chat_markup_to_markdown(text), text);
        }
        assert_eq!(markdown_to_chat_markup("2*3*4"), "2*3*4");
        assert_eq!(chat_markup_to_markdown("2*3*4"), "2*3*4");
        assert_eq!(
            chat_markup_to_markdown("https://example.com/*a*"),
            "https://example.com/*a*"
        );
        assert_eq!(markdown_to_chat_markup("<@U1> **hi**"), "<@U1> *hi*");
        assert_eq!(
            markdown_to_chat_markup("```\n**fenced**\n```"),
            "```\n**fenced**\n```"
        );
    }

    #[test]
    fn multi_line_text_converts_per_line() {
        assert_eq!(
            markdown_to_chat_markup("**one**\n~~two~~\n**unclosed\nclosed**"),
            "*one*\n~two~\n**unclosed\nclosed**"
        );
    }

    #[test]
    fn round_trip_is_stable() {
        for markdown in [
            "**bold** and _italic_ and ~~strike~~ and `code`",
            "hi @Bogdan, **see** https://a.io/x_y",
            "plain text",
        ] {
            let wire = markdown_to_chat_markup(markdown);
            assert_eq!(chat_markup_to_markdown(&wire), markdown, "wire: {wire}");
            assert_eq!(
                markdown_to_chat_markup(&chat_markup_to_markdown(&wire)),
                wire
            );
        }
    }
}
