//! Value-shape credential and host-path redaction shared by every failure-detail sink.
//!
//! Detection matches credential-SHAPED VALUES rather than keywords, so prose like
//! "request exceeds 128000 tokens" survives untouched while a real `sk-`-prefixed key,
//! bearer token, JWT, or credential-keyed value is replaced. Only the matched value is
//! ever removed: the surrounding text, and for the key/value shape the key and operator
//! themselves, always survive. This module is pure text transformation with no I/O, so it
//! is reachable from every crate in the redaction-consuming graph without adding an edge.
//!
//! Detection requires context: a known credential-key prefix, a `Bearer` token, a JWT, or a
//! credential-keyed pair. There is no standalone high-entropy rule — a raw key with no prefix
//! and no preceding credential key is not distinguishable from other high-entropy text (a
//! padded base64 blob, for example) on shape alone, so it is accepted as a residual risk
//! rather than mangling unrelated content.

const CREDENTIAL_KEYS: [&str; 8] = [
    "api_key",
    "apikey",
    "authorization",
    "password",
    "secret",
    "access_token",
    "refresh_token",
    "client_secret",
];

const PREFIXED_KEY_PREFIXES: [&str; 3] = ["sk-", "sk_", "rk-"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Segment<'a> {
    Delimiter(&'a str),
    Word(&'a str),
}

impl<'a> Segment<'a> {
    const fn text(self) -> &'a str {
        match self {
            Self::Delimiter(text) | Self::Word(text) => text,
        }
    }
}

/// Splits `value` into maximal runs of delimiter characters (whitespace, quotes, commas,
/// semicolons) and maximal runs of everything else. Segments strictly alternate kind, so
/// every word segment's neighbors are always delimiter segments (or the string boundary),
/// which lets every rule below find an adjacent word by a fixed offset instead of
/// re-scanning for the next boundary.
fn tokenize(value: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut current_is_delimiter: Option<bool> = None;

    for (index, character) in value.char_indices() {
        let is_delimiter = is_delimiter_char(character);

        match current_is_delimiter {
            Some(previous) if previous == is_delimiter => {}
            Some(previous) => {
                push_segment(&mut segments, &value[start..index], previous);
                start = index;
                current_is_delimiter = Some(is_delimiter);
            }
            None => current_is_delimiter = Some(is_delimiter),
        }
    }

    if let Some(is_delimiter) = current_is_delimiter {
        push_segment(&mut segments, &value[start..], is_delimiter);
    }

    segments
}

fn push_segment<'a>(segments: &mut Vec<Segment<'a>>, text: &'a str, is_delimiter: bool) {
    if is_delimiter {
        segments.push(Segment::Delimiter(text));
    } else {
        segments.push(Segment::Word(text));
    }
}

fn is_delimiter_char(character: char) -> bool {
    character.is_whitespace() || matches!(character, ',' | ';' | '"' | '\'')
}

const REDACTED_MARKER_PREFIX: &str = "[redacted:";

fn redacted_marker(value: &str) -> String {
    format!("[redacted: {} characters]", value.chars().count())
}

/// Replaces every credential-shaped VALUE in `value` with a withheld marker, leaving all
/// surrounding text — including bare keywords with no credential-shaped value attached —
/// untouched. See the module documentation for the exact shapes matched.
pub fn redact_credential_values(value: &str) -> String {
    let segments = tokenize(value);
    let mut replacements: Vec<Option<String>> = vec![None; segments.len()];
    let mut consumed = vec![false; segments.len()];

    apply_cross_token_rules(&segments, &mut replacements, &mut consumed);
    apply_single_token_rules(&segments, &mut replacements, &consumed);

    segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            replacements[index]
                .clone()
                .unwrap_or_else(|| segment.text().to_owned())
        })
        .collect()
}

fn next_word_index(segments: &[Segment<'_>], index: usize) -> Option<usize> {
    let candidate = index + 2;
    matches!(segments.get(candidate), Some(Segment::Word(_))).then_some(candidate)
}

/// Rules whose match spans two tokens: `Bearer <token>`, a bare `key:`/`key=` token
/// immediately followed, across whitespace, by a separate value token, and a bare credential
/// key token followed by a separately-tokenized operator and value (the JSON-quoted shape,
/// where the surrounding quotes are delimiters). All three consume the value token so the
/// single-token pass below never reprocesses it.
///
/// The Bearer pass runs to completion before the key/value pass starts. Otherwise a preceding
/// credential key such as `Authorization:` would be visited first in a single combined pass,
/// consume the literal word `Bearer` as if it were the value, and mark it `consumed` — hiding
/// the real token from the Bearer rule entirely.
fn apply_cross_token_rules(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
) {
    redact_bearer_pass(segments, replacements, consumed);
    redact_key_value_pass(segments, replacements, consumed);
}

fn redact_bearer_pass(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
) {
    for (index, segment) in segments.iter().enumerate() {
        let Segment::Word(word) = *segment else {
            continue;
        };
        if consumed[index] {
            continue;
        }

        if word.eq_ignore_ascii_case("bearer") || is_bare_key_with_operator_and_bearer(word) {
            redact_bearer_value(segments, replacements, consumed, index);
        }
    }
}

/// Matches a credential key glued directly to `Bearer` with no separating whitespace, for
/// example `authorization=Bearer` or `authorization:Bearer`. The whole token — key, operator,
/// and the word `Bearer` — is left untouched; only the real token in the next word is
/// replaced, by the same logic as the bare `Bearer <token>` shape.
fn is_bare_key_with_operator_and_bearer(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    CREDENTIAL_KEYS
        .iter()
        .any(|key| lower == format!("{key}=bearer") || lower == format!("{key}:bearer"))
}

fn redact_key_value_pass(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
) {
    for (index, segment) in segments.iter().enumerate() {
        let Segment::Word(word) = *segment else {
            continue;
        };
        if consumed[index] {
            continue;
        }

        if is_bare_key_with_operator(word) {
            redact_adjacent_key_value(segments, replacements, consumed, index);
            continue;
        }

        if is_bare_credential_key(word) {
            redact_separated_key_value(segments, replacements, consumed, index);
        }
    }
}

fn redact_bearer_value(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
    bearer_index: usize,
) {
    let Some(value_index) = next_word_index(segments, bearer_index) else {
        return;
    };
    if consumed[value_index] {
        return;
    }
    let Segment::Word(value_word) = segments[value_index] else {
        return;
    };
    if value_word.chars().count() < 16 {
        return;
    }

    replacements[value_index] = Some(redacted_marker(value_word));
    consumed[value_index] = true;
    consumed[bearer_index] = true;
}

fn is_bare_key_with_operator(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    CREDENTIAL_KEYS
        .iter()
        .any(|key| lower == format!("{key}=") || lower == format!("{key}:"))
}

fn is_bare_credential_key(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    CREDENTIAL_KEYS.iter().any(|key| lower == *key)
}

fn redact_adjacent_key_value(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
    key_index: usize,
) {
    let Some(value_index) = next_word_index(segments, key_index) else {
        return;
    };
    if consumed[value_index] {
        return;
    }
    let Segment::Word(value_word) = segments[value_index] else {
        return;
    };
    if value_word.is_empty()
        || value_word.starts_with(REDACTED_MARKER_PREFIX)
        || value_word.eq_ignore_ascii_case("bearer")
    {
        return;
    }

    replacements[value_index] = Some(redacted_marker(value_word));
    consumed[value_index] = true;
    consumed[key_index] = true;
}

/// Matches a credential key whose operator has been split into its own token by an
/// intervening delimiter run — for example `"api_key":"value"`, where the surrounding quotes
/// are delimiters and so `api_key`, `:`, and the value are three separate tokens — and
/// replaces only the value token that follows the operator.
fn redact_separated_key_value(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
    key_index: usize,
) {
    let Some(operator_index) = next_word_index(segments, key_index) else {
        return;
    };
    if consumed[operator_index] {
        return;
    }
    let Segment::Word(operator_word) = segments[operator_index] else {
        return;
    };
    if operator_word != ":" && operator_word != "=" {
        return;
    }

    let Some(value_index) = next_word_index(segments, operator_index) else {
        return;
    };
    if consumed[value_index] {
        return;
    }
    let Segment::Word(value_word) = segments[value_index] else {
        return;
    };
    if value_word.is_empty()
        || value_word.starts_with(REDACTED_MARKER_PREFIX)
        || value_word.eq_ignore_ascii_case("bearer")
    {
        return;
    }

    replacements[value_index] = Some(redacted_marker(value_word));
    consumed[value_index] = true;
    consumed[operator_index] = true;
    consumed[key_index] = true;
}

/// Rules that match entirely within one token: a JWT, a prefixed key, a `key=value` or
/// `key:value` pair glued together with no separating whitespace, and a high-entropy token.
fn apply_single_token_rules(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &[bool],
) {
    for (index, segment) in segments.iter().enumerate() {
        let Segment::Word(word) = *segment else {
            continue;
        };
        if consumed[index] || replacements[index].is_some() {
            continue;
        }

        if is_jwt(word) || is_prefixed_key(word) {
            replacements[index] = Some(redacted_marker(word));
            continue;
        }

        if let Some(redacted) = redact_inline_key_value(word) {
            replacements[index] = Some(redacted);
        }
    }
}

fn is_jwt(word: &str) -> bool {
    let mut parts = word.split('.');
    let (Some(first), Some(second), Some(third), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    first.starts_with("eyJ")
        && [first, second, third]
            .into_iter()
            .all(|segment| segment.chars().count() >= 8 && segment.chars().all(is_base64url_char))
}

fn is_base64url_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn is_prefixed_key(word: &str) -> bool {
    PREFIXED_KEY_PREFIXES.iter().any(|prefix| {
        word.strip_prefix(prefix).is_some_and(|remainder| {
            remainder.chars().count() >= 16
                && remainder.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
        })
    })
}

/// A `:` that forms part of a `::` run never binds a credential key: Rust's path-qualification
/// operator is not a key/value operator, and a credential key/value pair always binds with a
/// single `:` or `=`. This is what lets a Rust-qualified path such as
/// `agens_auth::secret::TokenStore::load` survive even though `secret` is a credential key.
fn is_glued_double_colon(word: &str, colon_offset: usize) -> bool {
    word[..colon_offset].ends_with(':') || word[colon_offset + 1..].starts_with(':')
}

/// Finds the byte offset of the `=`/`:` that actually binds a credential key inside a single
/// glued token, rather than assuming it is the first such character in the token. This
/// matters for shapes like a URL query string, where the first `:` is the scheme separator
/// and the credential key sits later, after a `?` or `&`.
fn find_inline_credential_key_operator(word: &str) -> Option<usize> {
    for (operator_offset, operator_char) in word.char_indices() {
        if operator_char != '=' && operator_char != ':' {
            continue;
        }
        if operator_char == ':' && is_glued_double_colon(word, operator_offset) {
            continue;
        }

        let candidate_prefix = &word[..operator_offset];
        let key_start = candidate_prefix
            .rfind(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .map_or(0, |boundary_index| {
                boundary_index
                    + candidate_prefix[boundary_index..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8)
            });
        let candidate_key = &candidate_prefix[key_start..];

        if CREDENTIAL_KEYS
            .iter()
            .any(|key| candidate_key.eq_ignore_ascii_case(key))
        {
            return Some(operator_offset);
        }
    }

    None
}

/// Splits a single token on the operator that binds a credential key and replaces only the
/// value that follows it, leaving everything before the operator — including a URL scheme or
/// path that happens to precede the credential key — untouched.
fn redact_inline_key_value(word: &str) -> Option<String> {
    let operator_offset = find_inline_credential_key_operator(word)?;
    let prefix = &word[..operator_offset];
    let operator = &word[operator_offset..operator_offset + 1];
    let redaction_value = &word[operator_offset + 1..];

    if redaction_value.is_empty()
        || redaction_value.starts_with(REDACTED_MARKER_PREFIX)
        || redaction_value.eq_ignore_ascii_case("bearer")
    {
        return None;
    }

    Some(format!(
        "{prefix}{operator}{}",
        redacted_marker(redaction_value)
    ))
}

/// Replaces leading-`/`, `~/`, and Windows drive (`C:\`) tokens with `[path]`, leaving
/// relative paths such as `src/main.rs:12` untouched. This sink is model-visible only: a
/// user-visible-only sink keeps host paths and must not call this function.
pub fn redact_absolute_paths(value: &str) -> String {
    tokenize(value)
        .into_iter()
        .map(|segment| match segment {
            Segment::Word(word) if is_absolute_path_token(word) => "[path]".to_owned(),
            other => other.text().to_owned(),
        })
        .collect()
}

fn is_absolute_path_token(word: &str) -> bool {
    word.starts_with('/') || word.starts_with("~/") || is_windows_drive_path(word)
}

fn is_windows_drive_path(word: &str) -> bool {
    let mut characters = word.chars();

    matches!(characters.next(), Some(letter) if letter.is_ascii_alphabetic())
        && characters.next() == Some(':')
        && characters.next() == Some('\\')
}

/// Bounds `value` to `max_chars` characters, appending a visible truncation marker when it
/// was cut. Mirrors `SUBAGENT_RESULT_TRUNCATION_MARKER` (`agens-session/src/turns.rs`): the
/// marker is appended after the cap rather than counted within it, so truncation is always
/// visible and never silent.
pub fn bounded_detail(value: &str, max_chars: usize) -> String {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return value.to_owned();
    }

    let mut bounded: String = value.chars().take(max_chars).collect();
    bounded.push_str(&truncation_marker(max_chars));
    bounded
}

fn truncation_marker(max_chars: usize) -> String {
    format!("\n[truncated: only the first {max_chars} characters of this detail were kept]")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case the value-shape detector is expected to handle, shared by the shape test
    /// and the idempotency test so a rule can never be idempotency-checked without also being
    /// shape-checked, or vice versa.
    fn credential_redaction_cases() -> Vec<(&'static str, String, String)> {
        let sk_dash_remainder = "A1".repeat(9);
        let sk_underscore_remainder = "A1".repeat(9);
        let rk_dash_remainder = "A1".repeat(9);
        let bearer_token = "A1".repeat(9);
        let jwt_first = format!("eyJ{}", "A1".repeat(5));
        let jwt_second = "A1".repeat(5);
        let jwt_third = "A1".repeat(5);
        let jwt = format!("{jwt_first}.{jwt_second}.{jwt_third}");
        let equals_value = "SuperSecretValue123";
        let spaced_colon_value = "hunter2VeryLongSecretValue";
        let glued_colon_value = "TopSecretValue999";
        let opaque_bearer_token = "abcdefghijklmnopqrstuvwx";
        let quoted_api_key_value = "hunter2short";
        let quoted_password_value = "hunter2";
        let url_query_value = "abcd1234EFGH5678";

        vec![
            (
                "sk- prefixed key",
                format!("key sk-{sk_dash_remainder} rejected"),
                format!(
                    "key [redacted: {} characters] rejected",
                    format!("sk-{sk_dash_remainder}").chars().count()
                ),
            ),
            (
                "sk_ prefixed key",
                format!("key sk_{sk_underscore_remainder} rejected"),
                format!(
                    "key [redacted: {} characters] rejected",
                    format!("sk_{sk_underscore_remainder}").chars().count()
                ),
            ),
            (
                "rk- prefixed key",
                format!("key rk-{rk_dash_remainder} rejected"),
                format!(
                    "key [redacted: {} characters] rejected",
                    format!("rk-{rk_dash_remainder}").chars().count()
                ),
            ),
            (
                "Bearer with a real token",
                format!("send Bearer {bearer_token} to the api"),
                format!(
                    "send Bearer [redacted: {} characters] to the api",
                    bearer_token.chars().count()
                ),
            ),
            (
                "Bearer word survives with no matching token",
                "Bearer token is required".to_owned(),
                "Bearer token is required".to_owned(),
            ),
            (
                "Authorization key followed by Bearer and an opaque token",
                format!("Authorization: Bearer {opaque_bearer_token} failed"),
                format!(
                    "Authorization: Bearer [redacted: {} characters] failed",
                    opaque_bearer_token.chars().count()
                ),
            ),
            (
                "authorization=Bearer glued key followed by an opaque token",
                format!("authorization=Bearer {opaque_bearer_token} failed"),
                format!(
                    "authorization=Bearer [redacted: {} characters] failed",
                    opaque_bearer_token.chars().count()
                ),
            ),
            (
                "unrelated credential key followed by Bearer and an opaque token",
                format!("secret: Bearer {opaque_bearer_token} failed"),
                format!(
                    "secret: Bearer [redacted: {} characters] failed",
                    opaque_bearer_token.chars().count()
                ),
            ),
            (
                "JWT",
                format!("auth {jwt} sample"),
                format!("auth [redacted: {} characters] sample", jwt.chars().count()),
            ),
            (
                "key=value glued shape",
                format!("config api_key={equals_value} saved"),
                format!(
                    "config api_key=[redacted: {} characters] saved",
                    equals_value.chars().count()
                ),
            ),
            (
                "key: value spaced shape",
                format!("auth password: {spaced_colon_value} submitted"),
                format!(
                    "auth password: [redacted: {} characters] submitted",
                    spaced_colon_value.chars().count()
                ),
            ),
            (
                "key:value glued shape",
                format!("auth secret:{glued_colon_value} now"),
                format!(
                    "auth secret:[redacted: {} characters] now",
                    glued_colon_value.chars().count()
                ),
            ),
            (
                "bare keywords with no value survive",
                "authorization secret password check".to_owned(),
                "authorization secret password check".to_owned(),
            ),
            (
                "JSON-quoted api_key pair",
                format!(r#"{{"api_key":"{quoted_api_key_value}"}}"#),
                format!(
                    r#"{{"api_key":"[redacted: {} characters]"}}"#,
                    quoted_api_key_value.chars().count()
                ),
            ),
            (
                "JSON-quoted password pair",
                format!(r#"{{"password":"{quoted_password_value}"}}"#),
                format!(
                    r#"{{"password":"[redacted: {} characters]"}}"#,
                    quoted_password_value.chars().count()
                ),
            ),
            (
                "URL query string credential",
                format!("POST https://api.example.com/v1/x?api_key={url_query_value} failed"),
                format!(
                    "POST https://api.example.com/v1/x?api_key=[redacted: {} characters] failed",
                    url_query_value.chars().count()
                ),
            ),
            (
                "negative: exceeds tokens sentence survives verbatim",
                "request exceeds 128000 tokens".to_owned(),
                "request exceeds 128000 tokens".to_owned(),
            ),
            (
                "negative: path key is not in the credential key set",
                "path: /input/0".to_owned(),
                "path: /input/0".to_owned(),
            ),
            (
                "negative: absolute path is not credential shaped",
                "/home/user/project".to_owned(),
                "/home/user/project".to_owned(),
            ),
        ]
    }

    #[test]
    fn redact_credential_values_replaces_only_shaped_secret_values() {
        for (name, input, expected) in credential_redaction_cases() {
            assert_eq!(redact_credential_values(&input), expected, "case: {name}");
        }
    }

    #[test]
    fn redact_credential_values_is_idempotent() {
        for (name, input, _expected) in credential_redaction_cases() {
            let once = redact_credential_values(&input);
            let twice = redact_credential_values(&once);

            assert_eq!(once, twice, "case: {name}");
        }
    }

    #[test]
    fn redact_credential_values_preserves_every_named_benign_shape() {
        let cases: Vec<(&str, &str)> = vec![
            (
                "40-char lowercase git SHA",
                "commit a94b8e2c1f3d0a5b7e9c2d4f6a8b0c1e3d5f7a9b done",
            ),
            (
                "40-char uppercase git SHA",
                "commit A94B8E2C1F3D0A5B7E9C2D4F6A8B0C1E3D5F7A9B done",
            ),
            (
                "lowercase UUID",
                "request 550e8400-e29b-41d4-a716-446655440000 accepted",
            ),
            (
                "absolute path",
                "reading /home/user/project/config.toml now",
            ),
            (
                "Rust type path",
                "panic in alloc::collections::btree_map::BTreeMap during insert",
            ),
            (
                "cargo fingerprint path",
                "target/debug/.fingerprint/agens-core-9f3c2a1b4e5d6f7a/lib-agens-core.json missing",
            ),
            (
                "sha256 digest",
                "image sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 pulled",
            ),
            (
                "bash stdout/stderr/exit-status shape",
                "[stdout]\nbuild ok\n[stderr]\n\n[exit status: 127]",
            ),
            (
                "Rust backtrace line with a ::secret:: segment",
                "2: at agens_auth::secret::TokenStore::load",
            ),
            (
                "padded base64 blob with no slash",
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
            ),
        ];

        for (name, input) in cases {
            assert_eq!(redact_credential_values(input), input, "case: {name}");
        }
    }

    #[test]
    fn bounded_detail_marks_truncation_and_leaves_short_values_untouched() {
        let short = "well under the cap";
        assert_eq!(bounded_detail(short, 64), short);

        let long = "x".repeat(100);
        let bounded = bounded_detail(&long, 10);

        assert!(bounded.starts_with(&"x".repeat(10)));
        assert!(bounded.contains("[truncated:"));
        assert!(bounded.contains("10"));
    }

    #[test]
    fn redact_absolute_paths_withholds_only_absolute_and_home_relative_tokens() {
        let cases: Vec<(&str, &str)> = vec![
            ("leading slash", "read /home/user/secret.txt now"),
            ("home relative", "read ~/config/agens.toml now"),
            ("windows drive", r"read C:\Users\name\file.txt now"),
        ];

        for (name, input) in cases {
            let redacted = redact_absolute_paths(input);
            assert!(redacted.contains("[path]"), "case: {name}: {redacted:?}");
            assert!(redacted.starts_with("read "), "case: {name}: {redacted:?}");
            assert!(redacted.ends_with(" now"), "case: {name}: {redacted:?}");
        }

        let relative = "see src/main.rs:12 for detail";
        assert_eq!(redact_absolute_paths(relative), relative);
    }
}
