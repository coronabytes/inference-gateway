//! Pseudonyms for what a guardrail found: `Person A`, `Location B`, the same one for the same
//! value wherever it appears in a request, so the model can still tell who is who. A request's
//! mapping lives only while the request does: it pseudonymizes the assistant's earlier turns (which
//! the client got back with the real values) and restores the real values in the answer.

use std::collections::HashMap;

/// A request's pseudonyms, each with the value it stands for.
#[derive(Debug, Default, Clone)]
pub struct Pseudonyms {
    /// (pseudonym, original), in the order they were given out.
    pairs: Vec<(String, String)>,
    /// By entity type and normalized value: the pseudonym given out.
    given: HashMap<(String, String), usize>,
    /// How many pseudonyms each label has given out.
    counts: HashMap<String, usize>,
}

/// The label of an entity type's pseudonyms: `PERSON` is `Person A`, `IBAN_CODE` `IBAN A`.
fn label(kind: &str) -> String {
    match kind {
        "PERSON" => "Person".into(),
        "LOCATION" => "Location".into(),
        "ORGANIZATION" => "Organization".into(),
        "EMAIL_ADDRESS" => "Email".into(),
        "PHONE_NUMBER" => "Phone".into(),
        "IBAN_CODE" => "IBAN".into(),
        "CREDIT_CARD" => "Card".into(),
        "IP_ADDRESS" => "IP".into(),
        "DATE_TIME" => "Date".into(),
        "NRP" => "Group".into(),
        other => other.replace('_', " "),
    }
}

/// The n-th letter code, from 0: A, B, …, Z, AA, AB, …
fn letters(mut n: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

/// A value as compared: case and runs of whitespace do not make another value.
fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Pseudonyms {
    /// The pseudonym of `value`, an entity of type `kind`: the one it already has, or the next.
    pub fn of(&mut self, kind: &str, value: &str) -> String {
        let key = (kind.to_string(), normalize(value));
        if let Some(&i) = self.given.get(&key) {
            return self.pairs[i].0.clone();
        }
        let label = label(kind);
        let n = self.counts.entry(label.clone()).or_default();
        let pseudonym = format!("{label} {}", letters(*n));
        *n += 1;
        self.given.insert(key, self.pairs.len());
        self.pairs.push((pseudonym.clone(), value.to_string()));
        pseudonym
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The pseudonyms and the values they stand for.
    #[cfg(test)]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Adds another guardrail's pseudonyms.
    pub fn extend(&mut self, other: &Pseudonyms) {
        for (p, o) in &other.pairs {
            if !self.pairs.iter().any(|(q, _)| q == p) {
                self.pairs.push((p.clone(), o.clone()));
            }
        }
    }

    /// `text` with every known value replaced by its pseudonym: for the assistant's earlier turns.
    pub fn pseudonymize(&self, text: &str) -> String {
        let terms: Vec<(&str, &str)> = self.pairs.iter().map(|(p, o)| (o.as_str(), p.as_str())).collect();
        replace(text, &terms, None, text.len(), true).0
    }

    /// `text` with every pseudonym replaced by its value: for a complete answer.
    pub fn restore(&self, text: &str) -> String {
        replace(text, &self.restorations(false), None, text.len(), true).0
    }

    /// (pseudonym, value), the value escaped for a JSON string when `json`.
    fn restorations(&self, json: bool) -> Vec<(&str, String)> {
        self.pairs
            .iter()
            .map(|(p, o)| {
                let value = if json {
                    let quoted = serde_json::to_string(o).unwrap_or_default();
                    quoted[1..quoted.len() - 1].to_string()
                } else {
                    o.clone()
                };
                (p.as_str(), value)
            })
            .collect()
    }

    /// A holdback for one streamed text, such as a content block: see `Holdback`.
    pub fn holdback(&self, json: bool) -> Holdback {
        let terms = self.restorations(json).into_iter().map(|(p, v)| (p.to_string(), v)).collect();
        Holdback { terms, held: String::new(), last: None }
    }
}

/// Replaces whole-word occurrences of each term's first string with its second, the longest first,
/// in `text[..upto]`: a match must start there, end by `upto` and stand alone (no letter or digit
/// right before or after it; `prev` is what came before the text). A match that ends at the text's
/// end only counts when `complete`, as more text may still follow. Returns the result and the byte
/// in `text` where it stopped.
fn replace<T: AsRef<str>>(text: &str, terms: &[(&str, T)], mut prev: Option<char>, upto: usize, complete: bool) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < upto {
        let rest = &text[at..];
        let starts_word = !prev.is_some_and(is_word);
        let found = starts_word
            .then(|| {
                terms
                    .iter()
                    .filter(|(from, _)| !from.is_empty() && rest.starts_with(from))
                    .filter(|(from, _)| {
                        let end = at + from.len();
                        end <= upto
                            && match text[end..].chars().next() {
                                Some(c) => !is_word(c),
                                None => complete,
                            }
                    })
                    .max_by_key(|(from, _)| from.len())
            })
            .flatten();
        match found {
            Some((from, to)) => {
                out.push_str(to.as_ref());
                prev = from.chars().last();
                at += from.len();
            }
            None => {
                let c = rest.chars().next().expect("not at the end");
                out.push(c);
                prev = Some(c);
                at += c.len_utf8();
            }
        }
    }
    (out, at)
}

/// Restores pseudonyms in a text that arrives in pieces, such as a streamed content block: a piece
/// is passed on at once, but for its end, which is held back while it could still become a
/// pseudonym (`Person` of `Person A`, or `Person A` of `Person AB`).
pub struct Holdback {
    terms: Vec<(String, String)>,
    held: String,
    /// The last character passed on as it came, or of a pseudonym's: what comes before `held`.
    last: Option<char>,
}

impl Holdback {
    /// The next piece, restored as far as it can be.
    pub fn push(&mut self, piece: &str) -> String {
        self.held.push_str(piece);
        let cut = self.cut();
        let terms: Vec<(&str, &str)> = self.terms.iter().map(|(p, v)| (p.as_str(), v.as_str())).collect();
        let (out, stopped) = replace(&self.held, &terms, self.last, cut, false);
        if let Some(c) = self.held[..stopped].chars().last() {
            self.last = Some(c);
        }
        self.held.drain(..stopped);
        out
    }

    /// What is still held, restored: at the end of the text.
    pub fn flush(&mut self) -> String {
        let held = std::mem::take(&mut self.held);
        let terms: Vec<(&str, &str)> = self.terms.iter().map(|(p, v)| (p.as_str(), v.as_str())).collect();
        let last = self.last.take();
        replace(&held, &terms, last, held.len(), true).0
    }

    /// Where the held text must stop for now: the earliest start of a word that begins a
    /// pseudonym, or is a whole one that more letters could still extend or end.
    fn cut(&self) -> usize {
        let text = &self.held;
        let longest = self.terms.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
        let from = text.len().saturating_sub(longest);
        let mut prev = self.last;
        for (i, c) in text.char_indices() {
            if i >= from && !prev.is_some_and(is_word) && self.terms.iter().any(|(p, _)| p.starts_with(&text[i..])) {
                return i;
            }
            prev = Some(c);
        }
        text.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_out_pseudonyms_in_order() {
        let mut p = Pseudonyms::default();
        assert_eq!(p.of("PERSON", "Alice"), "Person A");
        assert_eq!(p.of("PERSON", "Bob"), "Person B");
        assert_eq!(p.of("LOCATION", "Berlin"), "Location A");
        // The same value, however written, is the same person.
        assert_eq!(p.of("PERSON", " alice "), "Person A");
        assert_eq!(p.of("IBAN_CODE", "DE89 3704"), "IBAN A");
        assert_eq!(p.of("US_SSN", "123"), "US SSN A");
        assert_eq!(
            (letters(25), letters(26), letters(27), letters(701), letters(702)),
            ("Z".into(), "AA".into(), "AB".into(), "ZZ".into(), "AAA".into())
        );
    }

    fn mapping() -> Pseudonyms {
        let mut p = Pseudonyms::default();
        p.of("PERSON", "Alice");
        p.of("PERSON", "Bob \"B\" Müller");
        p.of("LOCATION", "Berlin");
        p
    }

    #[test]
    fn replaces_whole_words_both_ways() {
        let p = mapping();
        assert_eq!(p.restore("Person A met Person B in Location A."), "Alice met Bob \"B\" Müller in Berlin.");
        // Not inside other words or longer pseudonyms.
        assert_eq!(p.restore("Person AB and Person Alpha and xPerson A"), "Person AB and Person Alpha and xPerson A");
        assert_eq!(p.pseudonymize("Alice said: Alicensing in Berlin, Alice!"), "Person A said: Alicensing in Location A, Person A!");
    }

    #[test]
    fn holds_back_what_could_become_a_pseudonym() {
        let p = mapping();
        let mut h = p.holdback(false);
        let mut out = String::new();
        for piece in ["Hi Per", "son", " A", ", meet Pers", "on B in Loc", "ation A", "B", " or Person A"] {
            out.push_str(&h.push(piece));
        }
        // `Location AB` is no pseudonym; the last `Person A` is held until the text ends.
        assert_eq!(out, "Hi Alice, meet Bob \"B\" Müller in Location AB or ");
        assert_eq!(h.flush(), "Alice");

        // In JSON, such as a tool call's arguments, values are escaped.
        let mut h = p.holdback(true);
        assert_eq!(h.push("{\"to\": \"Person B\"}"), "{\"to\": \"Bob \\\"B\\\" Müller\"}");
        assert_eq!(h.flush(), "");

        // A pseudonym glued to the word before it, split across pieces, stays as it is.
        let mut h = p.holdback(false);
        let out = h.push("xPer") + &h.push("son A") + &h.flush();
        assert_eq!(out, "xPerson A");
    }
}
