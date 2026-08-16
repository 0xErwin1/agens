//! Value-shape credential and host-path redaction shared by every failure-detail sink.
//!
//! Detection matches credential-SHAPED VALUES rather than keywords, so prose like
//! "request exceeds 128000 tokens" survives untouched while a real `sk-`-prefixed key,
//! bearer token, JWT, or credential-keyed value is replaced. Only the matched value is
//! ever removed: the surrounding text, and for the key/value shape the key and operator
//! themselves, always survive. This module is pure text transformation with no I/O, so it
//! is reachable from every crate in the redaction-consuming graph without adding an edge.
//!
//! Detection requires context: a known credential-key prefix, a value introduced by an
//! authentication scheme, a JWT, or a credential-keyed pair. There is no standalone
//! high-entropy rule — a raw key with no prefix and no preceding credential key is not
//! distinguishable from other high-entropy text (a padded base64 blob, for example) on shape
//! alone, so it is accepted as a residual risk rather than mangling unrelated content.

const CREDENTIAL_KEYS: [&str; 16] = [
    "api_key",
    "apikey",
    "api-key",
    "x-api-key",
    "authorization",
    "auth",
    "credential",
    "password",
    "passwd",
    "pat",
    "private_key",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "client_secret",
];

/// Authentication scheme words that introduce a credential rather than being one. A scheme is
/// always followed by the value it describes, so the scheme word itself must survive and the
/// token after it is what gets replaced. `aws4-hmac-sha256` is the SigV4 algorithm name, which
/// occupies the same position in an `Authorization` header.
const AUTH_SCHEMES: [&str; 7] = [
    "basic",
    "bearer",
    "digest",
    "negotiate",
    "token",
    "apikey",
    "aws4-hmac-sha256",
];

const PREFIXED_KEY_PREFIXES: [&str; 3] = ["sk-", "sk_", "rk-"];

/// A credential value is at least this long when nothing else marks it as opaque.
const MIN_OPAQUE_VALUE_CHARS: usize = 16;

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

/// Rules whose match spans two tokens: `<scheme> <credential>`, a bare `key:`/`key=` token
/// immediately followed, across whitespace, by a separate value token, and a bare credential
/// key token followed by a separately-tokenized operator and value (the JSON-quoted shape,
/// where the surrounding quotes are delimiters). All three consume the value token so the
/// single-token pass below never reprocesses it.
///
/// The auth-scheme pass runs to completion before the key/value pass starts. Otherwise a
/// preceding credential key such as `Authorization:` would be visited first in a single combined
/// pass, consume the literal scheme word — `Bearer`, `Basic`, `AWS4-HMAC-SHA256` — as if it were
/// the value, and mark it `consumed`, hiding the real credential from the scheme rule entirely
/// and leaving it in the output.
fn apply_cross_token_rules(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
) {
    redact_auth_scheme_pass(segments, replacements, consumed);
    redact_key_value_pass(segments, replacements, consumed);
}

fn redact_auth_scheme_pass(
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

        if is_auth_scheme(word) || is_bare_key_with_operator_and_scheme(word) {
            redact_scheme_value(segments, replacements, consumed, index);
        }
    }
}

fn is_auth_scheme(word: &str) -> bool {
    AUTH_SCHEMES
        .iter()
        .any(|scheme| word.eq_ignore_ascii_case(scheme))
}

/// Matches a credential key glued directly to its scheme with no separating whitespace, for
/// example `authorization=Bearer` or `authorization:Basic`. The whole token — key, operator,
/// and the scheme — is left untouched; only the real credential in the next word is replaced,
/// by the same logic as the bare `<scheme> <credential>` shape.
fn is_bare_key_with_operator_and_scheme(word: &str) -> bool {
    let Some((key, scheme)) = word.split_once(['=', ':']) else {
        return false;
    };

    is_credential_key(key) && is_auth_scheme(scheme)
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

        if is_credential_key(word) {
            redact_separated_key_value(segments, replacements, consumed, index);
        }
    }
}

fn redact_scheme_value(
    segments: &[Segment<'_>],
    replacements: &mut [Option<String>],
    consumed: &mut [bool],
    scheme_index: usize,
) {
    let Some(value_index) = next_word_index(segments, scheme_index) else {
        return;
    };
    if consumed[value_index] {
        return;
    }
    let Segment::Word(value_word) = segments[value_index] else {
        return;
    };
    if !is_replaceable_credential_value(value_word) {
        return;
    }

    replacements[value_index] = Some(redacted_marker(value_word));
    consumed[value_index] = true;
    consumed[scheme_index] = true;
}

/// Whether a token that already carries a credential key or an auth scheme in front of it may
/// be replaced.
///
/// The preceding context alone is not enough: `authorization: denied by policy`, `"secret":
/// true` and `password: incorrect` all put a benign word in the value position, and replacing
/// it corrupts failure text while asserting that a secret was there. A real credential is
/// either long, or mixes letters with digits, or carries a character prose does not use inside
/// a word. A hyphen counts as one of those: prose in a value position is a single plain word,
/// while credentials are routinely hyphenated. The withheld marker never qualifies, which is
/// what keeps a second pass a no-op.
fn is_replaceable_credential_value(value: &str) -> bool {
    if value.is_empty()
        || value.starts_with(REDACTED_MARKER_PREFIX)
        || is_auth_scheme(value)
        || CREDENTIAL_KEYS.contains(&value.to_ascii_lowercase().as_str())
    {
        return false;
    }

    is_credential_shaped_value(value)
}

fn is_credential_shaped_value(value: &str) -> bool {
    if value
        .chars()
        .any(|character| matches!(character, '_' | '-' | '+' | '/' | '='))
    {
        return true;
    }
    if value.chars().count() >= MIN_OPAQUE_VALUE_CHARS {
        return true;
    }

    value.chars().any(|character| character.is_ascii_digit())
        && value.chars().any(char::is_alphabetic)
}

/// Whether `name` carries a known credential key as a whole segment.
///
/// Real credentials are namespaced far more often than they are bare — `GITHUB_TOKEN`,
/// `AWS_SECRET_ACCESS_KEY`, `DATABASE_PASSWORD` — so requiring the whole name to equal a known
/// key would recognize only the rarest form. Matching at segment boundaries is what keeps a
/// benign word that merely contains one (`tokenizer`, `passwordless`, `secretary`) from
/// matching.
///
/// A segment boundary is not only a literal `_`, `-` or `.`: names reaching this predicate come
/// from JSON tool payloads as often as from environment variables, and camelCase is the
/// dominant convention there, so `accessToken` has to be recognized exactly as `access_token`
/// is. A plural segment is the same secret as its singular (`tokens`, `api_keys`), and missing
/// it is worse than missing a scalar, because the sinks that prune on this predicate prune at
/// the parent path — a missed plural leaves every element of the array rendered.
pub fn is_credential_key(name: &str) -> bool {
    if name.is_empty() || !name.chars().all(is_credential_key_char) {
        return false;
    }

    let normalized = normalize_key_boundaries(name);

    CREDENTIAL_KEYS
        .iter()
        .any(|key| contains_credential_key_segment(&normalized, key))
}

fn is_credential_key_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

/// Lowercases `name` after making every implicit word boundary explicit, so the segment scan
/// below only ever has to look for a literal separator.
fn normalize_key_boundaries(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut normalized = String::with_capacity(characters.len());

    for (index, character) in characters.iter().enumerate() {
        if index > 0 && opens_camel_case_word(&characters, index) {
            normalized.push('_');
        }

        normalized.push(character.to_ascii_lowercase());
    }

    normalized
}

/// Whether the character at `index` starts a new word without a separator in front of it: an
/// uppercase letter after a lowercase or a digit (`accessToken`), or the last letter of an
/// acronym run that runs into a lowercase word (`XApiKey`).
fn opens_camel_case_word(characters: &[char], index: usize) -> bool {
    if !characters[index].is_ascii_uppercase() {
        return false;
    }

    let previous = characters[index - 1];

    previous.is_ascii_lowercase()
        || previous.is_ascii_digit()
        || (previous.is_ascii_uppercase()
            && characters
                .get(index + 1)
                .is_some_and(char::is_ascii_lowercase))
}

fn contains_credential_key_segment(name: &str, key: &str) -> bool {
    name.match_indices(key).any(|(start, matched)| {
        is_key_segment_boundary(name, start) && ends_key_segment(name, start + matched.len())
    })
}

/// A key segment ends either at a boundary or at a plural `s` that itself sits before one.
fn ends_key_segment(name: &str, offset: usize) -> bool {
    if is_key_segment_boundary(name, offset) {
        return true;
    }

    name.as_bytes().get(offset) == Some(&b's') && is_key_segment_boundary(name, offset + 1)
}

/// `name` is validated as ASCII by [`is_credential_key`] before this is reached, so byte
/// indexing cannot split a character here.
fn is_key_segment_boundary(name: &str, offset: usize) -> bool {
    if offset == 0 || offset == name.len() {
        return true;
    }

    matches!(name.as_bytes().get(offset - 1), Some(b'_' | b'-' | b'.'))
        || matches!(name.as_bytes().get(offset), Some(b'_' | b'-' | b'.'))
}

fn is_bare_key_with_operator(word: &str) -> bool {
    let Some(key) = word.strip_suffix('=').or_else(|| word.strip_suffix(':')) else {
        return false;
    };

    is_credential_key(key)
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
    if !is_replaceable_credential_value(value_word) {
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
    if !is_replaceable_credential_value(value_word) {
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
            .rfind(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
            })
            .map_or(0, |boundary_index| {
                boundary_index
                    + candidate_prefix[boundary_index..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8)
            });
        if is_credential_key(&candidate_prefix[key_start..]) {
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
        || is_auth_scheme(redaction_value)
        || !is_replaceable_credential_value(redaction_value)
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
    let candidate = strip_wrapping_path_punctuation(word);
    if looks_like_absolute_path(candidate) {
        return true;
    }
    if let Some((_, rest)) = candidate.split_once('=')
        && looks_like_absolute_path(rest)
    {
        return true;
    }
    if let Some((_, rest)) = candidate.split_once("://") {
        return looks_like_absolute_path(rest) || rest.starts_with('/');
    }
    false
}

fn strip_wrapping_path_punctuation(word: &str) -> &str {
    word.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
    })
}

fn looks_like_absolute_path(word: &str) -> bool {
    word.starts_with('/') || word.starts_with("~/") || is_windows_drive_path(word)
}

fn is_windows_drive_path(word: &str) -> bool {
    let mut characters = word.chars();

    matches!(characters.next(), Some(letter) if letter.is_ascii_alphabetic())
        && characters.next() == Some(':')
        && characters.next() == Some('\\')
}

/// Bounds `value` to `max_chars` characters, keeping both the head and the tail rather than
/// only the head. A failing native tool call — a bash script, a build — puts its most useful
/// detail (captured stderr, the exit status) at the end of its output, so a head-only bound
/// would drop exactly that detail on any failure larger than the cap. Mirrors
/// `SUBAGENT_RESULT_TRUNCATION_MARKER` (`agens-session/src/turns.rs`): the marker sits between
/// the two kept halves and is never counted against the cap, so truncation is always visible and
/// never silent.
pub fn bounded_detail(value: &str, max_chars: usize) -> String {
    let characters: Vec<char> = value.chars().collect();
    let total_chars = characters.len();
    if total_chars <= max_chars {
        return value.to_owned();
    }

    let head_chars = max_chars.div_ceil(2);
    let tail_chars = max_chars - head_chars;

    let head: String = characters[..head_chars].iter().collect();
    let tail: String = characters[total_chars - tail_chars..].iter().collect();

    format!(
        "{head}{}{tail}",
        truncation_marker(head_chars, tail_chars, total_chars)
    )
}

/// Replaces every exact occurrence of a caller-supplied value with a withheld marker.
///
/// Unlike [`redact_credential_values`], this does not rely on a recognizable shape: the
/// caller already knows the literal secret (for example, an MCP server's own configured
/// transport environment), so it is matched and replaced wherever it appears in `value`,
/// regardless of surrounding context. Values are matched longest-first so a secret that is a
/// substring of another configured secret cannot pre-empt the longer match. Empty values are
/// never matched.
///
/// A withheld marker already present in `value` is copied through untouched rather than being
/// rescanned. Without that, a secret which happens to be a substring of the marker text would
/// match its own replacement and this function would not be idempotent.
pub fn redact_exact_values(value: &str, secrets: &[String]) -> String {
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.len()));

    let mut redacted = String::with_capacity(value.len());
    let mut offset = 0;

    while offset < value.len() {
        let remainder = &value[offset..];

        if let Some(marker_length) = redaction_marker_length(remainder) {
            redacted.push_str(&remainder[..marker_length]);
            offset += marker_length;
            continue;
        }

        if let Some(secret) = ordered
            .iter()
            .find(|secret| remainder.starts_with(**secret))
        {
            redacted.push_str(&redacted_marker(secret));
            offset += secret.len();
            continue;
        }

        let Some(character) = remainder.chars().next() else {
            break;
        };
        redacted.push(character);
        offset += character.len_utf8();
    }

    redacted
}

/// The byte length of a `[redacted: N characters]` marker starting at the front of `value`.
fn redaction_marker_length(value: &str) -> Option<usize> {
    const MARKER_SUFFIX: &str = " characters]";

    let remainder = value
        .strip_prefix(REDACTED_MARKER_PREFIX)?
        .strip_prefix(' ')?;
    let digits = remainder.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || !remainder[digits..].starts_with(MARKER_SUFFIX) {
        return None;
    }

    Some(REDACTED_MARKER_PREFIX.len() + 1 + digits + MARKER_SUFFIX.len())
}

fn truncation_marker(head_chars: usize, tail_chars: usize, total_chars: usize) -> String {
    format!(
        "\n[truncated: kept the first {head_chars} and last {tail_chars} of {total_chars} characters]\n"
    )
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
        let token_equals_value = "SuperSecretToken123";
        let api_key_hyphenated_value = "AKIAHYPHENVALUE12345";
        let x_api_key_value = "xk-remote-body-value123";
        let basic_credential = "YWxhZGRpbjpvcGVuc2VzYW1l";
        let token_scheme_credential = "0123456789abcdef0123";
        let negotiate_credential = "YIIZkwYGKwYBBQUCoIIZ";
        let digest_credential = "nonce=deadbeefcafe1234";
        let sigv4_credential = "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request";
        let short_bearer_token = "abc123def456ghi";
        let github_token_value = "ghp_abcdefghijklmnop";
        let aws_secret_value = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let database_password_value = "hunter2";
        let namespaced_api_key_value = "abcd1234";

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
                "Basic scheme survives while its credential is withheld",
                format!("Authorization: Basic {basic_credential}"),
                format!(
                    "Authorization: Basic [redacted: {} characters]",
                    basic_credential.chars().count()
                ),
            ),
            (
                "JSON-quoted authorization pair carrying a Basic credential",
                format!(r#"{{"authorization":"Basic {basic_credential}"}}"#),
                format!(
                    r#"{{"authorization":"Basic [redacted: {} characters]"}}"#,
                    basic_credential.chars().count()
                ),
            ),
            (
                "Token scheme survives while its credential is withheld",
                format!("Authorization: Token {token_scheme_credential} rejected"),
                format!(
                    "Authorization: Token [redacted: {} characters] rejected",
                    token_scheme_credential.chars().count()
                ),
            ),
            (
                "Negotiate scheme survives while its credential is withheld",
                format!("Authorization: Negotiate {negotiate_credential} rejected"),
                format!(
                    "Authorization: Negotiate [redacted: {} characters] rejected",
                    negotiate_credential.chars().count()
                ),
            ),
            (
                "Digest scheme survives while its credential is withheld",
                format!("Authorization: Digest {digest_credential} rejected"),
                format!(
                    "Authorization: Digest [redacted: {} characters] rejected",
                    digest_credential.chars().count()
                ),
            ),
            (
                "AWS SigV4 algorithm survives while its credential is withheld",
                format!("Authorization: AWS4-HMAC-SHA256 {sigv4_credential} rejected"),
                format!(
                    "Authorization: AWS4-HMAC-SHA256 [redacted: {} characters] rejected",
                    sigv4_credential.chars().count()
                ),
            ),
            (
                "Bearer token shorter than an opaque-value floor",
                format!("Authorization: Bearer {short_bearer_token} rejected"),
                format!(
                    "Authorization: Bearer [redacted: {} characters] rejected",
                    short_bearer_token.chars().count()
                ),
            ),
            (
                "namespaced GITHUB_TOKEN environment assignment",
                format!("env GITHUB_TOKEN={github_token_value} exported"),
                format!(
                    "env GITHUB_TOKEN=[redacted: {} characters] exported",
                    github_token_value.chars().count()
                ),
            ),
            (
                "namespaced AWS_SECRET_ACCESS_KEY environment assignment",
                format!("env AWS_SECRET_ACCESS_KEY={aws_secret_value} exported"),
                format!(
                    "env AWS_SECRET_ACCESS_KEY=[redacted: {} characters] exported",
                    aws_secret_value.chars().count()
                ),
            ),
            (
                "namespaced DATABASE_PASSWORD environment assignment",
                format!("env DATABASE_PASSWORD={database_password_value} exported"),
                format!(
                    "env DATABASE_PASSWORD=[redacted: {} characters] exported",
                    database_password_value.chars().count()
                ),
            ),
            (
                "namespaced MY_API_KEY environment assignment",
                format!("env MY_API_KEY={namespaced_api_key_value} exported"),
                format!(
                    "env MY_API_KEY=[redacted: {} characters] exported",
                    namespaced_api_key_value.chars().count()
                ),
            ),
            (
                "negative: NPM_TOKEN with no value survives",
                "env NPM_TOKEN= exported".to_owned(),
                "env NPM_TOKEN= exported".to_owned(),
            ),
            (
                "negative: a benign word merely containing a credential key survives",
                "tokenizer=simple passwordless=true secretary=alice".to_owned(),
                "tokenizer=simple passwordless=true secretary=alice".to_owned(),
            ),
            (
                "negative: a benign word after a credential key survives",
                "authorization: denied by policy".to_owned(),
                "authorization: denied by policy".to_owned(),
            ),
            (
                "negative: a JSON boolean after a credential key survives",
                r#"{"secret": true}"#.to_owned(),
                r#"{"secret": true}"#.to_owned(),
            ),
            (
                "negative: a benign word after a password key survives",
                "password: incorrect".to_owned(),
                "password: incorrect".to_owned(),
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
                "authorization secret password token check".to_owned(),
                "authorization secret password token check".to_owned(),
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
                "bare token key=value glued shape",
                format!("auth token={token_equals_value} sent"),
                format!(
                    "auth token=[redacted: {} characters] sent",
                    token_equals_value.chars().count()
                ),
            ),
            (
                "hyphenated api-key=value glued shape",
                format!("config api-key={api_key_hyphenated_value} saved"),
                format!(
                    "config api-key=[redacted: {} characters] saved",
                    api_key_hyphenated_value.chars().count()
                ),
            ),
            (
                "hyphenated x-api-key header, spaced value",
                format!("X-Api-Key: {x_api_key_value} rejected"),
                format!(
                    "X-Api-Key: [redacted: {} characters] rejected",
                    x_api_key_value.chars().count()
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
            (
                "negative: glued short password without credential shape survives like the spaced form",
                "password=hunterhunt".to_owned(),
                "password=hunterhunt".to_owned(),
            ),
            (
                "negative: spaced short password without credential shape survives",
                "password: hunterhunt".to_owned(),
                "password: hunterhunt".to_owned(),
            ),
            (
                "namespaced GH_PAT environment assignment",
                format!("env GH_PAT={github_token_value} exported"),
                format!(
                    "env GH_PAT=[redacted: {} characters] exported",
                    github_token_value.chars().count()
                ),
            ),
            (
                "namespaced MCP_AUTH environment assignment",
                format!("env MCP_AUTH={opaque_bearer_token} exported"),
                format!(
                    "env MCP_AUTH=[redacted: {} characters] exported",
                    opaque_bearer_token.chars().count()
                ),
            ),
            (
                "passwd key=value glued shape",
                format!("config passwd={equals_value} saved"),
                format!(
                    "config passwd=[redacted: {} characters] saved",
                    equals_value.chars().count()
                ),
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
            let three_times = redact_credential_values(&twice);

            assert_eq!(once, twice, "case: {name}");
            assert_eq!(twice, three_times, "case: {name}");
        }
    }

    /// A configured secret that happens to be a substring of the withheld marker would match
    /// its own replacement on any later pass, so the marker has to be copied through rather
    /// than rescanned.
    #[test]
    fn redact_exact_values_is_idempotent_even_for_a_marker_shaped_secret() {
        let secrets = vec!["characters".to_owned()];
        let once = redact_exact_values("the value characters was rejected", &secrets);
        let twice = redact_exact_values(&once, &secrets);
        let three_times = redact_exact_values(&twice, &secrets);

        assert_eq!(once, "the value [redacted: 10 characters] was rejected");
        assert_eq!(once, twice);
        assert_eq!(twice, three_times);
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

    /// The predicate is the single gate every key-based sink prunes on, so a name it misses is
    /// a secret rendered verbatim somewhere. JSON tool payloads name their arguments in
    /// camelCase and hold collections under plural names, and both were invisible to a scan
    /// that only recognized `_`/`-` boundaries.
    #[test]
    fn is_credential_key_recognizes_camel_case_plural_and_dotted_names() {
        let credential_names = [
            "accessToken",
            "authToken",
            "refreshToken",
            "clientSecret",
            "bearerToken",
            "privateKey",
            "apiKey",
            "xApiKey",
            "XApiKey",
            "sessionPassword",
            "tokens",
            "secrets",
            "passwords",
            "api_keys",
            "credentials",
            "accessTokens",
            "token.value",
            "auth.accessToken",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "x-api-key",
            "token",
            "GH_PAT",
            "MCP_AUTH",
            "passwd",
        ];

        for name in credential_names {
            assert!(is_credential_key(name), "must be credential-shaped: {name}");
        }

        let benign_names = [
            "tokenizer",
            "passwordless",
            "secretary",
            "tokenizers",
            "path",
            "command",
            "content",
            "timeout_ms",
            "key",
            "keys",
            "publicId",
            "",
            "token value",
            "secret/token",
        ];

        for name in benign_names {
            assert!(
                !is_credential_key(name),
                "must not be credential-shaped: {name}"
            );
        }
    }

    /// Widening the predicate widens every value rule built on it, so the shapes that were
    /// deliberately left alone have to stay alone: a benign value after a credential key, and a
    /// sentence that merely counts tokens.
    #[test]
    fn widened_key_matching_does_not_redact_benign_values() {
        let cases = [
            "request exceeds 128000 tokens",
            "maxTokens: 128000",
            "max_tokens: 128000",
            "tokens: none",
            "accessToken: missing",
            "credentials: invalid",
        ];

        for input in cases {
            assert_eq!(redact_credential_values(input), input, "case: {input}");
        }
    }

    /// The glued `key=value` rule used to skip the value-shape check the other three rules
    /// apply. That withheld a token COUNT written as `max_tokens=128000` and redacted
    /// `password=hunterhunt` while the spaced and JSON forms of the same value survived. All
    /// four rules now share the shape check. A short alphabetic-only secret is a residual in
    /// every syntax.
    #[test]
    fn glued_and_spaced_key_value_share_the_value_shape_check() {
        assert_eq!(
            redact_credential_values("max_tokens=128000 exceeded"),
            "max_tokens=128000 exceeded"
        );
        assert_eq!(
            redact_credential_values("max_tokens: 128000 exceeded"),
            "max_tokens: 128000 exceeded"
        );
    }

    #[test]
    fn bounded_detail_marks_truncation_and_leaves_short_values_untouched() {
        let short = "well under the cap";
        assert_eq!(bounded_detail(short, 64), short);

        let long = "x".repeat(100);
        let bounded = bounded_detail(&long, 10);

        assert!(bounded.starts_with("xxxxx"));
        assert!(bounded.ends_with("xxxxx"));
        assert!(bounded.contains("[truncated:"));
        assert_eq!(
            bounded
                .chars()
                .filter(|&character| character == 'x')
                .count(),
            10
        );
    }

    /// A failing bash tool call puts its most useful detail — the captured stderr and the exit
    /// status — at the very end of the content (`render_bash_result`,
    /// `agens-tools/src/lib.rs:6226-6260`). A head-only bound would drop exactly that detail on
    /// any failure larger than the cap, which is the case this bound exists to serve.
    #[test]
    fn bounded_detail_keeps_both_ends_so_tail_content_survives() {
        let head_filler = "h".repeat(200);
        let tail_signal = "TAILSIGNAL";
        let long = format!("{head_filler}{tail_signal}");

        let bounded = bounded_detail(&long, 20);

        assert!(bounded.starts_with(&"h".repeat(10)));
        assert!(bounded.ends_with(tail_signal));
        assert!(bounded.contains("[truncated:"));
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

    #[test]
    fn redact_absolute_paths_withholds_paths_wrapped_by_punctuation_or_flags() {
        let cases: Vec<(&str, &str)> = vec![
            ("backticks", "failed at `/home/user/proj/crates/x`"),
            ("parentheses", "open (/home/user/proj/crates/x)"),
            (
                "equals flag",
                "cargo --manifest-path=/home/user/proj/Cargo.toml",
            ),
            ("file url", "see file:///home/user/proj/Cargo.toml"),
        ];

        for (name, input) in cases {
            let redacted = redact_absolute_paths(input);
            assert!(redacted.contains("[path]"), "case: {name}: {redacted:?}");
            assert!(
                !redacted.contains("/home/user"),
                "case: {name}: {redacted:?}"
            );
        }

        assert_eq!(
            redact_absolute_paths("see `src/main.rs` for detail"),
            "see `src/main.rs` for detail"
        );
    }

    #[test]
    fn redact_exact_values_withholds_only_the_given_values() {
        let secrets = vec!["SENTINEL_MCP_REMOTE_BODY".to_owned()];
        let redacted = redact_exact_values(
            "server rejected the call: SENTINEL_MCP_REMOTE_BODY was invalid",
            &secrets,
        );

        assert!(!redacted.contains("SENTINEL_MCP_REMOTE_BODY"));
        assert!(redacted.starts_with("server rejected the call: [redacted:"));
        assert!(redacted.ends_with("was invalid"));
    }

    #[test]
    fn redact_exact_values_matches_every_occurrence_and_every_configured_secret() {
        let secrets = vec!["FIRST_SECRET".to_owned(), "SECOND_SECRET".to_owned()];
        let redacted = redact_exact_values(
            "FIRST_SECRET arrived twice: FIRST_SECRET, alongside SECOND_SECRET",
            &secrets,
        );

        assert!(!redacted.contains("FIRST_SECRET"));
        assert!(!redacted.contains("SECOND_SECRET"));
        assert_eq!(redacted.matches("[redacted:").count(), 3);
    }

    #[test]
    fn redact_exact_values_leaves_unrelated_text_untouched() {
        let secrets = vec!["CONFIGURED_SECRET".to_owned()];
        let benign = "request exceeds 128000 tokens";

        assert_eq!(redact_exact_values(benign, &secrets), benign);
        assert_eq!(redact_exact_values(benign, &[]), benign);
    }

    #[test]
    fn redact_exact_values_ignores_empty_configured_values() {
        let secrets = vec![String::new(), "REAL_SECRET".to_owned()];
        let redacted = redact_exact_values("value REAL_SECRET here", &secrets);

        assert!(!redacted.contains("REAL_SECRET"));
        assert!(redacted.starts_with("value "));
        assert!(redacted.ends_with(" here"));
    }
}
