//! Reading what a permission target actually names, before a pattern is
//! matched against it.
//!
//! A rule is written against the thing the operator has in mind — the file
//! `.env`, the command `rm` — while the value that reaches the policy is
//! whatever spelling the caller happened to produce. `./.env` is the same file
//! as `.env`, and `cd /tmp && sudo /bin/rm -rf x` runs the same `rm` as
//! `rm -rf x`. This module turns the second into the first.
//!
//! For shell commands this closes the ordinary evasions and nothing more. It is
//! not a security boundary: a command line is a program, and a program that
//! wants to hide which binary it runs — through a variable, an alias, an
//! interpreter, an encoded string — can. Treat it as making an honest deny
//! behave the way it reads, not as containment for an adversary.

/// Normalizes a path-shaped target to the spelling a rule is written in.
///
/// Every component that selects nothing is dropped, in whatever combination it
/// was written: repeated separators (`src//secret`), a component of `.`
/// wherever it appears (`./src/./secret/.`), and the trailing separator that
/// names a directory. What remains is what the path names — whether it is
/// relative, whether it is absolute, and which components it walks through.
///
/// `..` is deliberately left alone: resolving it would need the real
/// filesystem and would quietly change which directory a rule is talking
/// about. The URI prefix of a `webfetch` target is left alone for the same
/// reason — the `//` after a scheme introduces an authority rather than
/// repeating a separator, and collapsing it would stop a rule from matching
/// the URL it was written against.
pub(crate) fn normalized_path_target(value: &str) -> String {
    let (prefix, remainder) = split_uri_prefix(value);
    let absolute = remainder.starts_with('/');

    let components = remainder
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();

    if components.is_empty() {
        let bare = if absolute {
            "/"
        } else if remainder.is_empty() {
            ""
        } else {
            "."
        };
        return format!("{prefix}{bare}");
    }

    let separator = if absolute { "/" } else { "" };
    format!("{prefix}{separator}{}", components.join("/"))
}

/// Gives the spellings of one path a rule could be written against: the
/// normalized form, and the directory spelling of it when the caller named a
/// directory.
///
/// The two are kept apart because the glob shapes an operator writes do not
/// agree on the trailing separator — `dir/**` selects `dir/` but not `dir`,
/// while an exact `dir` selects only `dir`. Offering both is what makes either
/// rule shape select a call on the directory it names.
pub(crate) fn path_target_forms(value: &str) -> Vec<String> {
    let normalized = normalized_path_target(value);

    if !names_a_directory(value) || normalized.ends_with('/') {
        return vec![normalized];
    }

    let directory = format!("{normalized}/");
    vec![normalized, directory]
}

/// Reports whether the value spells its last component as a directory, by
/// ending in a separator or in a `.` component that only a directory has.
fn names_a_directory(value: &str) -> bool {
    value.ends_with('/') || value.ends_with("/.") || value == "."
}

/// Splits off a `scheme://` prefix, whose `//` is part of the URI's grammar
/// rather than a repeated path separator.
fn split_uri_prefix(value: &str) -> (&str, &str) {
    let Some(scheme_end) = value.find("://") else {
        return ("", value);
    };

    let scheme = &value[..scheme_end];
    let is_scheme = scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        });

    if is_scheme {
        value.split_at(scheme_end + 3)
    } else {
        ("", value)
    }
}

/// Decomposes a shell command line into the invocations it would run, each
/// given as the equivalent spellings a rule could reasonably be written
/// against: the text as written, and the same invocation with its wrapper
/// commands, environment assignments and directory prefix removed.
///
/// The decomposition sees through `&&`, `||`, `;`, `|`, newlines, command
/// substitution and `sh -c`. Quoting is respected well enough to keep a
/// separator inside a string from splitting the line, but this is a reader
/// rather than a shell parser, and unusual quoting resolves to fewer
/// invocations rather than to wrong ones.
pub(crate) fn command_invocations(command: &str) -> Vec<Vec<String>> {
    let mut invocations = Vec::new();
    collect_invocations(command, 0, &mut invocations);
    invocations
}

/// Decomposes a shell command line into the invocations it would run, each
/// given as its raw tokens, before any wrapper command, environment assignment
/// or directory prefix is stripped away.
///
/// [`command_invocations`] answers what a rule is written against, which is why
/// it drops the wrappers: `sudo rm x` and `rm x` are one subject to a rule
/// naming `rm`. The denylist asks the other question — what the invocation
/// actually does — and there the `sudo` is the answer rather than noise.
pub(crate) fn command_invocation_tokens(command: &str) -> Vec<Vec<String>> {
    let mut tokens = Vec::new();
    collect_invocation_tokens(command, 0, &mut tokens);
    tokens
}

fn collect_invocation_tokens(command: &str, depth: usize, invocations: &mut Vec<Vec<String>>) {
    if depth > MAX_SHELL_DEPTH {
        return;
    }

    let (segments, substitutions) = scan(command);

    for segment in segments.into_iter().chain(substitutions) {
        if invocations.len() >= MAX_INVOCATIONS {
            return;
        }

        let written = segment.trim();
        if written.is_empty() {
            continue;
        }

        let tokens = tokenize(written);
        if tokens.is_empty() {
            continue;
        }

        if let Some(script) = shell_script(strip_prefixes(&tokens)) {
            collect_invocation_tokens(&script, depth + 1, invocations);
        }

        invocations.push(tokens);
    }
}

/// The tokens of one invocation with its leading environment assignments and
/// wrapper commands removed. See [`strip_prefixes`].
pub(crate) fn without_wrapper_prefixes(tokens: &[String]) -> &[String] {
    strip_prefixes(tokens)
}

/// Whether a token is a leading `NAME=value` environment assignment rather than
/// the command being run.
pub(crate) fn is_environment_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(name, _)| !name.is_empty() && is_environment_name(name))
}

/// A command line long enough to exhaust these bounds is not a shape any rule
/// is written against, so it resolves to whatever was found before the bound
/// and is still matched whole.
const MAX_INVOCATIONS: usize = 64;
const MAX_SHELL_DEPTH: usize = 4;

const WRAPPER_COMMANDS: [&str; 8] = [
    "sudo", "doas", "env", "command", "nohup", "time", "xargs", "builtin",
];

const SHELL_COMMANDS: [&str; 6] = ["bash", "sh", "zsh", "dash", "ksh", "busybox"];

/// Short options of the wrapper commands above that consume the word after
/// them, so `sudo -u root rm` reads `root` as an argument rather than as the
/// command being wrapped.
const VALUE_TAKING_FLAGS: [&str; 12] = [
    "-u", "-g", "-p", "-C", "-t", "-r", "-n", "-I", "-d", "-P", "-a", "-o",
];

fn collect_invocations(command: &str, depth: usize, invocations: &mut Vec<Vec<String>>) {
    if depth > MAX_SHELL_DEPTH {
        return;
    }

    let (segments, substitutions) = scan(command);

    for segment in segments.into_iter().chain(substitutions) {
        if invocations.len() >= MAX_INVOCATIONS {
            return;
        }

        let written = segment.trim();
        if written.is_empty() {
            continue;
        }

        let tokens = tokenize(written);
        let stripped = strip_prefixes(&tokens);

        if let Some(script) = shell_script(stripped) {
            collect_invocations(&script, depth + 1, invocations);
        }

        let mut forms = vec![written.to_owned()];
        let normalized = normalized_form(stripped);
        if normalized.is_empty() {
            continue;
        }
        if normalized != written {
            forms.push(normalized);
        }

        invocations.push(forms);
    }
}

/// Splits one expression into its top-level segments and the bodies of the
/// command substitutions it embeds.
fn scan(command: &str) -> (Vec<String>, Vec<String>) {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut substitutions = Vec::new();
    let mut segment_start = 0;
    let mut substitution_start = 0;
    let mut substitution_depth = 0usize;
    let mut inside_backtick = false;
    let mut quote: Option<u8> = None;
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];

        if substitution_depth > 0 {
            match byte {
                b'(' => substitution_depth += 1,
                b')' => {
                    substitution_depth -= 1;
                    if substitution_depth == 0 {
                        substitutions.push(command[substitution_start..index].to_owned());
                    }
                }
                _ => {}
            }
            index += 1;
            continue;
        }

        if inside_backtick {
            if byte == b'`' {
                inside_backtick = false;
                substitutions.push(command[substitution_start..index].to_owned());
            }
            index += 1;
            continue;
        }

        if let Some(open) = quote {
            if byte == open {
                quote = None;
            } else if open == b'"' && byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
                substitution_depth = 1;
                substitution_start = index + 2;
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }

        match byte {
            b'\'' | b'"' => {
                quote = Some(byte);
                index += 1;
            }
            b'\\' => index += 2,
            b'`' => {
                inside_backtick = true;
                substitution_start = index + 1;
                index += 1;
            }
            b'$' if bytes.get(index + 1) == Some(&b'(') => {
                substitution_depth = 1;
                substitution_start = index + 2;
                index += 2;
            }
            b';' | b'\n' | b'|' | b'&' => {
                segments.push(command[segment_start..index].to_owned());
                let mut end = index + 1;
                while end < bytes.len() && matches!(bytes[end], b'|' | b'&') {
                    end += 1;
                }
                segment_start = end;
                index = end;
            }
            _ => index += 1,
        }
    }

    segments.push(command[segment_start..].to_owned());
    (segments, substitutions)
}

/// Splits one invocation into its words, dropping the quotes and the
/// backslashes that grouped them — `\rm` and `"rm"` both name `rm`.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut started = false;

    for character in segment.chars() {
        if escaped {
            escaped = false;
            current.push(character);
            started = true;
            continue;
        }

        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
            None if character == '\\' => {
                escaped = true;
                started = true;
            }
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }

    if started {
        tokens.push(current);
    }

    tokens
}

/// Drops the leading environment assignments and wrapper commands that stand
/// between the operator's `sudo`/`env`/`xargs` habit and the command actually
/// being run.
fn strip_prefixes(tokens: &[String]) -> &[String] {
    let mut remaining = tokens;

    loop {
        let Some((first, rest)) = remaining.split_first() else {
            return remaining;
        };

        let is_assignment = is_environment_assignment(first);
        let is_wrapper = WRAPPER_COMMANDS.contains(&command_name(first));

        if !is_assignment && !is_wrapper {
            return remaining;
        }

        remaining = rest;
        while is_wrapper
            && let Some((flag, rest)) = remaining.split_first()
            && flag.starts_with('-')
        {
            remaining = if VALUE_TAKING_FLAGS.contains(&flag.as_str()) {
                rest.split_first().map_or(rest, |(_, rest)| rest)
            } else {
                rest
            };
        }
    }
}

/// Reports the script a shell invocation was asked to run, so the commands
/// inside it are read as invocations rather than as an opaque argument.
fn shell_script(tokens: &[String]) -> Option<String> {
    let (first, rest) = tokens.split_first()?;
    if !SHELL_COMMANDS.contains(&command_name(first)) {
        return None;
    }

    let flag = rest.iter().position(|token| token == "-c")?;
    rest.get(flag + 1).cloned()
}

fn normalized_form(tokens: &[String]) -> String {
    let Some((first, rest)) = tokens.split_first() else {
        return String::new();
    };

    std::iter::once(command_name(first))
        .chain(rest.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reduces an invoked path to the command it names, so `/bin/rm` and `rm` are
/// one subject.
pub(crate) fn command_name(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn is_environment_name(name: &str) -> bool {
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subjects(command: &str) -> Vec<Vec<String>> {
        command_invocations(command)
    }

    fn selects(command: &str, needle: &str) -> bool {
        subjects(command)
            .iter()
            .any(|forms| forms.iter().any(|form| form.starts_with(needle)))
    }

    #[test]
    fn a_simple_command_is_one_invocation() {
        assert_eq!(subjects("rm -rf victim"), vec![vec!["rm -rf victim"]]);
    }

    #[test]
    fn every_separator_starts_a_new_invocation() {
        for command in [
            "cd /tmp && rm -rf victim",
            "cd /tmp; rm -rf victim",
            "cd /tmp || rm -rf victim",
            "cd /tmp\nrm -rf victim",
            "cd /tmp | rm -rf victim",
        ] {
            assert!(selects(command, "rm"), "{command} must expose its rm");
            assert!(selects(command, "cd"), "{command} must expose its cd");
        }
    }

    #[test]
    fn an_invoked_path_and_its_wrappers_reduce_to_the_command_name() {
        for command in [
            "/bin/rm -rf victim",
            "sudo rm -rf victim",
            "sudo -u root /usr/bin/rm -rf victim",
            "env RUST_LOG=debug rm -rf victim",
            "RUST_LOG=debug rm -rf victim",
            "ls | xargs rm",
            "nohup time /bin/rm victim",
        ] {
            assert!(selects(command, "rm"), "{command} must reduce to rm");
        }
    }

    #[test]
    fn a_substitution_and_an_interpreted_script_are_read_as_invocations() {
        for command in [
            "echo $(rm -rf victim)",
            "echo `rm -rf victim`",
            "bash -c \"rm -rf victim\"",
            "sh -c 'cd /tmp && rm -rf victim'",
            "echo \"$(rm -rf victim)\"",
        ] {
            assert!(selects(command, "rm"), "{command} must expose its rm");
        }
    }

    #[test]
    fn a_separator_inside_a_string_is_not_a_separator() {
        assert_eq!(
            subjects("echo 'a && b'"),
            vec![vec!["echo 'a && b'", "echo a && b"]]
        );
    }

    #[test]
    fn the_invocation_count_is_bounded() {
        let command = std::iter::repeat_n("rm x", 500)
            .collect::<Vec<_>>()
            .join(" && ");

        assert!(command_invocations(&command).len() <= MAX_INVOCATIONS);
    }

    #[test]
    fn a_path_target_drops_only_its_no_op_components() {
        assert_eq!(normalized_path_target("./.env"), ".env");
        assert_eq!(normalized_path_target(".././.env"), "../.env");
        assert_eq!(normalized_path_target("./src/./main.rs"), "src/main.rs");
        assert_eq!(normalized_path_target("src/main.rs"), "src/main.rs");
        assert_eq!(
            normalized_path_target("https://example.invalid/a"),
            "https://example.invalid/a"
        );
    }

    /// Every spelling of one file has to reduce to the same string, whichever
    /// no-op components it was written with and however they were combined.
    #[test]
    fn every_spelling_of_one_path_reduces_to_the_same_target() {
        for spelling in [
            "src/secret/key.txt",
            "src//secret/key.txt",
            "src///secret/key.txt",
            "./src//secret/key.txt",
            "src/./secret/key.txt",
            "././src/.//secret///key.txt",
            ".//src/secret/./key.txt",
            "src/secret/key.txt/.",
        ] {
            assert_eq!(
                normalized_path_target(spelling),
                "src/secret/key.txt",
                "{spelling} must name the same file as src/secret/key.txt"
            );
        }
    }

    #[test]
    fn a_path_keeps_what_changes_which_file_it_names() {
        assert_eq!(normalized_path_target("../.env"), "../.env");
        assert_eq!(normalized_path_target("//etc/passwd"), "/etc/passwd");
        assert_eq!(normalized_path_target("/etc//passwd"), "/etc/passwd");
        assert_eq!(normalized_path_target("."), ".");
        assert_eq!(normalized_path_target("./"), ".");
        assert_eq!(normalized_path_target("/"), "/");
        assert_eq!(normalized_path_target(""), "");
    }

    /// A URL's `//` separates its scheme from its authority and is not a
    /// repeated path separator, so collapsing it would stop a `webfetch` rule
    /// from matching the URL it was written against.
    #[test]
    fn a_url_keeps_the_separator_that_introduces_its_authority() {
        assert_eq!(
            normalized_path_target("https://example.invalid/a"),
            "https://example.invalid/a"
        );
        assert_eq!(
            normalized_path_target("https://example.invalid//a/./b"),
            "https://example.invalid/a/b"
        );
        assert_eq!(normalized_path_target("file:///tmp/x"), "file:///tmp/x");
    }

    /// A trailing separator names the same directory, but the glob shapes a
    /// rule is written in do not all agree on that: `dir/**` selects `dir/`
    /// and not `dir`, while an exact `dir` selects only `dir`. Both spellings
    /// are therefore offered so either rule shape selects the call.
    #[test]
    fn a_directory_named_with_a_trailing_separator_is_offered_both_ways() {
        assert_eq!(
            path_target_forms("src//secret/"),
            vec!["src/secret".to_owned(), "src/secret/".to_owned()]
        );
        assert_eq!(
            path_target_forms("src/secret/."),
            vec!["src/secret".to_owned(), "src/secret/".to_owned()]
        );
        assert_eq!(
            path_target_forms("src/secret"),
            vec!["src/secret".to_owned()]
        );
    }

    /// A backslash is the shell's own way of spelling a command plainly, and
    /// `\rm` runs the same `rm`.
    #[test]
    fn an_escaped_command_name_reduces_to_the_command_it_runs() {
        for command in [
            r"\rm -rf victim",
            r"\/bin/rm -rf victim",
            r"cd /tmp && \rm -rf victim",
        ] {
            assert!(selects(command, "rm"), "{command} must reduce to rm");
        }
    }
}
