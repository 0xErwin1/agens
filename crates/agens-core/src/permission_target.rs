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
/// Only the no-op components are removed — a leading `./` and any interior
/// `/./`. `..` is deliberately left alone: resolving it would need the real
/// filesystem and would quietly change which directory a rule is talking
/// about.
pub(crate) fn normalized_path_target(value: &str) -> String {
    let mut normalized = value.replace("/./", "/");

    while let Some(remainder) = normalized.strip_prefix("./") {
        normalized = remainder.to_owned();
    }

    normalized
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

/// Splits one invocation into its words, dropping the quotes that grouped them.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;

    for character in segment.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => current.push(character),
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

        let is_assignment = first
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && is_environment_name(name));
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
fn command_name(token: &str) -> &str {
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
}
