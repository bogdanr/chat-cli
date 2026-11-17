//! Text normalisation shared by search and filter code.

/// Lowercases and strips diacritics from Latin letters so a search typed
/// without them ("invatat") matches names written with them ("Învățat"),
/// and the other way round.
pub fn fold_for_search(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for ch in text.chars().flat_map(char::to_lowercase) {
        match fold_latin_diacritic(ch) {
            Some(base) => folded.push_str(base),
            None => folded.push(ch),
        }
    }
    folded
}

/// Base letters for the lowercase Latin-1 Supplement and Latin Extended-A
/// letters, plus Romanian comma-below ș/ț. Combining marks are dropped.
fn fold_latin_diacritic(ch: char) -> Option<&'static str> {
    Some(match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        '\u{0300}'..='\u{036f}' => "",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_romanian_diacritics_both_cedilla_and_comma_below() {
        assert_eq!(fold_for_search("Învățat"), "invatat");
        assert_eq!(fold_for_search("ŞTIINŢĂ și Țară"), "stiinta si tara");
    }

    #[test]
    fn drops_combining_marks() {
        assert_eq!(
            fold_for_search("i\u{0302}nva\u{0306}t\u{0326}at"),
            "invatat"
        );
    }

    #[test]
    fn keeps_non_latin_text() {
        assert_eq!(fold_for_search("Grüße Ærø"), "grusse aero");
        assert_eq!(fold_for_search("Привет 日本"), "привет 日本");
    }
}
