//! Credential masking shared by hook logs, the CLI and the GUI. Everything a
//! user can read (output, logs, JSON, notifications) goes through here unless
//! the command's purpose is to reveal a credential (`db url`).
//!
//! Every detector looks at the original text and reports byte ranges; the
//! ranges are merged and the text is rendered once. No detector ever sees
//! another's output, so masking one value can never hide the keyword that
//! identifies the next. When unsure where a value ends, mask more: a lost
//! closing quote is better than a leaked suffix.
//!
//! Redaction guards against accidental disclosure (a hook printing
//! `DATABASE_URL`, a URL in an error). Hooks are trusted code; one that
//! deliberately obfuscates a secret can always reveal it.
use serde_json::Value;

pub const MASK: &str = "***";

/// Keys whose values are secrets (`password`, `PGPASSWORD`, `secret`, …).
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["password", "passwd", "secret", "token", "api_key", "apikey"]
        .iter()
        .any(|word| key.contains(word))
}

/// Whether an environment variable called `name` is meant to hold a
/// credential, judging by the name alone.
///
/// Broader than [`is_secret_key`], which also classifies the keys of
/// arbitrary JSON and so cannot afford a word as common as `key`. For a
/// variable name that is affordable, and the cost of a false positive is only
/// that a value is shown as [`MASK`]. Besides `is_secret_key`'s words: a
/// passphrase, a credential, a private key, and any word of the name (split
/// at `_`, `.`, `-` and between a lower-case and an upper-case letter) that
/// ends in `pass`, `pwd`, `key` or `dsn` (`DB_PASS`, `SESSION_KEY`,
/// `stripeKey`, `SENTRY_DSN`). Compare with the value-based detectors of
/// [`text`], which catch credentials whatever the name.
pub fn is_secret_variable(name: &str) -> bool {
    if is_secret_key(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    if ["passphrase", "credential", "private_key", "privatekey"]
        .iter()
        .any(|word| lower.contains(word))
    {
        return true;
    }
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(c.to_ascii_lowercase());
        previous_lower = c.is_ascii_lowercase();
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.iter().any(|word| {
        ["pass", "pwd", "key", "dsn"]
            .iter()
            .any(|s| word.ends_with(s))
    })
}

/// Mask credentials in free text: URL userinfo passwords, secret URL query
/// parameters (their names may be percent-encoded: `%70assword=`), and
/// `password` / `sslpassword` / `PGPASSWORD` assignments as written in conninfo
/// strings and shells (optional spaces around `=`, quotes, escapes).
pub fn text(input: &str) -> String {
    render(input, pattern_spans(input))
}

/// [`text`], plus every occurrence of the known `secrets` (catches values
/// printed in forms the patterns miss, e.g. shell-escaped). Every non-empty
/// secret is masked, however short; overlapping matches are masked as one.
pub fn text_with(input: &str, secrets: &[&str]) -> String {
    let mut spans = pattern_spans(input);
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        let mut from = 0;
        while let Some(found) = input[from..].find(secret) {
            let start = from + found;
            spans.push((start, start + secret.len()));
            // Overlapping occurrences of one secret count too (`aa` in `aaa`).
            from = start + input[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    render(input, spans)
}

/// JSON fields whose object value is a map keyed by names the user chose (a
/// service key, a scratch database, a snapshot, a file): `receipt.services`,
/// `service_sources`, `service_source_ids`, `extras`, `snapshot_versions`,
/// `phases`, `input_hashes`, `workspaces`, `projects` and `services` itself.
/// A key there is an identity, not a field name, so `tokens` or
/// `password_store` as a service key says nothing about its value. The keys
/// are still scanned as text (a URL in a key must not leak).
const IDENTITY_MAPS: &[&str] = &[
    "services",
    "service_sources",
    "service_source_ids",
    "extras",
    "snapshot_versions",
    "phases",
    "input_hashes",
    "workspaces",
    "projects",
];

/// Mask secrets in a JSON value: secret-like field names, `[KEY, value]` pairs
/// whose KEY is an environment variable name, and credentials inside strings.
///
/// The structure of a document is never changed. A scalar under a secret-like
/// key becomes [`MASK`] (and so does a list of plain values under one); an
/// object under one keeps its shape, because a map keyed by a user-chosen name
/// (`[env]` variable names, service keys) is not a credential for being called
/// `tokens`: its own fields are judged one by one. The maps of
/// [`IDENTITY_MAPS`] are not judged by key at all.
pub fn json(value: &mut Value) {
    walk(value, false);
}

fn walk(value: &mut Value, secret_context: bool) {
    match value {
        Value::String(s) => {
            if secret_context {
                *s = MASK.to_string();
                return;
            }
            let masked = text(s);
            if masked != *s {
                *s = masked;
            }
        }
        Value::Number(_) | Value::Bool(_) if secret_context => {
            *value = Value::String(MASK.to_string());
        }
        Value::Array(items) => {
            // Only a real variable name makes an array a pair: a two-command
            // list such as ["echo password", "echo SECOND"] is not one.
            if !secret_context
                && let [Value::String(key), Value::String(secret)] = items.as_mut_slice()
                && is_env_name(key)
                && is_secret_variable(key)
            {
                *secret = MASK.to_string();
                return;
            }
            items.iter_mut().for_each(|item| match item {
                // Objects keep their shape, their fields are judged on their own.
                Value::Object(_) => walk(item, false),
                _ => walk(item, secret_context),
            });
        }
        Value::Object(map) => {
            // Keys are text too: a map keyed by connection URL must not leak.
            // An object keeps its shape under a secret-like key; its own
            // fields are judged one by one.
            for (key, mut entry) in std::mem::take(map) {
                match entry {
                    Value::Object(ref mut children) if IDENTITY_MAPS.contains(&key.as_str()) => {
                        for (name, mut child) in std::mem::take(children) {
                            walk(&mut child, false);
                            children.insert(text(&name), child);
                        }
                    }
                    _ => {
                        let secret = is_secret_field(&key)
                            && !entry.is_null()
                            && !is_reference(&key, &entry);
                        // A list of plain values under a secret-like name is
                        // one secret (`"token": ["a"]`); a list of objects
                        // keeps its shape.
                        let plain_list = matches!(&entry, Value::Array(items)
                            if items.iter().all(|i| !i.is_object() && !i.is_array()));
                        if secret && plain_list {
                            entry = Value::String(MASK.to_string());
                        } else {
                            walk(&mut entry, secret);
                        }
                    }
                }
                map.insert(text(&key), entry);
            }
        }
        _ => {}
    }
}

/// Whether the JSON field `key` holds a credential. A key shaped like an
/// environment variable name (`^[A-Z_][A-Z0-9_]*$`: a map of variables, as in
/// `scope.env.vars`) is judged as a variable is, by [`is_secret_variable`], so
/// `DB_PASS` and `SESSION_KEY` are caught there as they are in `env list`. Any
/// other key only by [`is_secret_key`]: field names are ordinary words, and
/// `key` (a service key, a recipe's variable name) is one of them.
fn is_secret_field(key: &str) -> bool {
    is_secret_key(key) || (is_env_name(key) && is_secret_variable(key))
}

/// A variable's value that is made of `${NAME}` references only (the
/// template of an `[env]` recipe): it names other variables and holds no
/// credential, whatever the variable is called. Only for keys shaped like
/// variable names; in any other field `${x}` is just text.
fn is_reference(key: &str, value: &Value) -> bool {
    is_env_name(key)
        && matches!(value, Value::String(text)
            if text.contains("${") && !crate::config_edit::has_literal_text(text))
}

/// `^[A-Z_][A-Z0-9_]*$`
fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Characters that end a URL inside surrounding text. libpq accepts quotes
/// and most punctuation inside a URI, so only whitespace ends one here; a
/// closing quote of the surrounding text is then masked along with a value.
fn ends_url(c: char) -> bool {
    c.is_ascii_whitespace()
}

/// Byte ranges of `input` to mask.
fn pattern_spans(input: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let urls = url_spans(input, &mut spans);
    assignment_spans(input, &urls, &mut spans);
    spans
}

/// Credentials in every `scheme://` URL, nested ones included. Returns the
/// URL regions. libpq reads credentials up to the first `@` before any `/`
/// (`?` and `#` are ordinary there); the password is everything after the
/// first `:`. Query values of secret parameters are masked whole: libpq URIs
/// have no fragment.
fn url_spans(input: &str, spans: &mut Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut search = 0;
    while let Some(found) = input[search..].find("://") {
        let start = search + found;
        let base = start + 3;
        let url = &input[base..];
        let url = &url[..url.find(ends_url).unwrap_or(url.len())];
        regions.push((start, base + url.len()));
        // libpq takes the first `@` before any `/` as the end of the
        // credentials; a password with a raw `@` reads differently. Mask
        // under both readings (first and last `@`), and check the query that
        // follows each possible host.
        let credentials_end = url.find('/').unwrap_or(url.len());
        let mut hosts = vec![0];
        for at in [
            url[..credentials_end].find('@'),
            url[..credentials_end].rfind('@'),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(colon) = url[..at].find(':')
                && colon + 1 < at
            {
                spans.push((base + colon + 1, base + at));
            }
            hosts.push(at + 1);
        }
        for host in hosts {
            let Some(question) = url[host..].find('?') else {
                continue;
            };
            let mut offset = host + question + 1;
            for pair in url[offset..].split('&') {
                if let Some(eq) = pair.find('=')
                    && eq + 1 < pair.len()
                    && secret_parameter(&pair[..eq])
                {
                    spans.push((base + offset + eq + 1, base + offset + pair.len()));
                }
                offset += pair.len() + 1;
            }
        }
        // Keep scanning inside this URL: a parameter may hold another one.
        search = base;
    }
    regions
}

/// `KEY = value` and `KEY: value` where KEY is a secret-named identifier
/// ([`is_secret_key`]: `PGPASSWORD`, `POSTGRES_PASSWORD`, `db.password`,
/// `"password"` in serialized JSON, YAML `password:`). Any whitespace may
/// surround the separator. The value is one word (see [`word_len`]); an
/// unquoted `KEY=value` value runs to the end of its line, since `env`
/// listings and log lines do not quote spaces (conninfo parameters after a
/// password are masked with it).
fn assignment_spans(input: &str, urls: &[(usize, usize)], spans: &mut Vec<(usize, usize)>) {
    let is_key_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut pos = 0;
    while pos < input.len() {
        let Some(key_start) = input[pos..].find(is_key_char).map(|i| pos + i) else {
            break;
        };
        let key_end = input[key_start..]
            .find(|c: char| !is_key_char(c))
            .map_or(input.len(), |i| key_start + i);
        pos = key_end;
        if !is_secret_key(&input[key_start..key_end]) {
            continue;
        }
        let before = input[..key_start].chars().next_back();
        // A query parameter name inside a URL: the URL pass decides it.
        if matches!(before, Some('?' | '&'))
            && urls.iter().any(|&(a, b)| a < key_start && key_start < b)
        {
            continue;
        }
        // The key's closing quote in serialized JSON (`"password": …`).
        let mut rest = &input[key_end..];
        if matches!(before, Some('"' | '\'')) && rest.starts_with(before.unwrap()) {
            rest = &rest[1..];
        }
        let rest = rest.trim_start();
        let (separator, after) = match rest.chars().next() {
            Some('=') => ('=', &rest[1..]),
            // `scheme://` is a URL, not a key.
            Some(':') if !rest.starts_with("://") => (':', &rest[1..]),
            _ => continue,
        };
        let value = after.trim_start();
        let value_start = input.len() - value.len();
        let mut len = word_len(value);
        if separator == '=' && !value.starts_with(['"', '\'']) {
            len = len.max(value.find(['\n', '\r']).unwrap_or(value.len()));
        }
        if len > 0 {
            spans.push((value_start, value_start + len));
        }
    }
}

/// Length in bytes of the value at the start of `value`: the longest of its
/// readings as a shell word, as a conninfo value, and with quotes opening a
/// quoted part anywhere. Only ASCII whitespace separates words (shells and
/// libpq treat a non-breaking space as an ordinary character).
fn word_len(value: &str) -> usize {
    shell_word_len(value)
        .max(conninfo_value_len(value))
        .max(any_quote_len(value))
}

/// POSIX shell: `\` escapes outside quotes and inside `"…"`; nothing is
/// escaped inside `'…'`.
fn shell_word_len(value: &str) -> usize {
    let mut chars = value.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        match c {
            c if c.is_ascii_whitespace() => return index,
            '\\' => {
                chars.next();
            }
            '\'' => {
                if !chars.any(|(_, c)| c == '\'') {
                    return value.len();
                }
            }
            '"' => loop {
                match chars.next() {
                    Some((_, '\\')) => {
                        chars.next();
                    }
                    Some((_, '"')) => break,
                    Some(_) => {}
                    None => return value.len(),
                }
            },
            _ => {}
        }
    }
    value.len()
}

/// libpq conninfo: a value is either `'…'` with `\` escapes, or runs to
/// whitespace with `\` escapes and quotes as ordinary characters.
fn conninfo_value_len(value: &str) -> usize {
    let mut chars = value.char_indices().peekable();
    if value.starts_with('\'') {
        chars.next();
        loop {
            match chars.next() {
                Some((_, '\\')) => {
                    chars.next();
                }
                Some((_, '\'')) => break,
                Some(_) => {}
                None => return value.len(),
            }
        }
    }
    while let Some((index, c)) = chars.next() {
        if c.is_ascii_whitespace() {
            return index;
        }
        if c == '\\' {
            chars.next();
        }
    }
    value.len()
}

/// Any quote, anywhere, opens a part that runs to its closing quote, with
/// `\` escapes inside; an unmatched quote masks the rest of the text.
fn any_quote_len(value: &str) -> usize {
    let mut chars = value.char_indices().peekable();
    // Open `${…}`, `$(…)` and `{…}` groups (JDBC brace escaping): whitespace
    // inside them does not end the value.
    let mut groups: Vec<char> = Vec::new();
    while let Some((index, c)) = chars.next() {
        match c {
            c if c.is_ascii_whitespace() && groups.is_empty() => return index,
            '{' => groups.push('}'),
            '(' if index > 0 && value[..index].ends_with('$') => groups.push(')'),
            '}' | ')' if groups.last() == Some(&c) => {
                groups.pop();
            }
            '\\' => {
                chars.next();
            }
            '"' | '\'' => loop {
                match chars.next() {
                    Some((_, '\\')) => {
                        chars.next();
                    }
                    Some((_, closing)) if closing == c => break,
                    Some(_) => {}
                    None => return value.len(),
                }
            },
            _ => {}
        }
    }
    value.len()
}

/// `input` with every span (merged where they overlap or touch) replaced by [`MASK`].
fn render(input: &str, mut spans: Vec<(usize, usize)>) -> String {
    spans.sort_unstable();
    let mut out = String::with_capacity(input.len());
    let mut copied = 0;
    let mut spans = spans.into_iter().peekable();
    while let Some((start, mut end)) = spans.next() {
        while let Some(&(next, next_end)) = spans.peek()
            && next <= end
        {
            end = end.max(next_end);
            spans.next();
        }
        if start < copied {
            // Already covered by a previous merged span.
            copied = copied.max(end);
            continue;
        }
        out.push_str(&input[copied..start]);
        out.push_str(MASK);
        copied = end;
    }
    out.push_str(&input[copied..]);
    out
}

fn secret_parameter(key: &str) -> bool {
    let key = percent_encoding::percent_decode_str(key)
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    crate::conn::is_secret_keyword(&key) || is_secret_key(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_that_look_masked_are_still_masked() {
        for (input, expected) in [
            ("password=***LEAK", "password=***"),
            ("PGPASSWORD=***LEAK psql", "PGPASSWORD=***"),
            ("host=h password=***&LEAK dbname=d", "host=h password=***"),
            ("password=\u{0}\u{1}masked\u{1}\u{0}LEAK", "password=***"),
            // A control character between two assignments separates them.
            (
                "PGUSER=u\u{0}PGPASSWORD=LEAK",
                "PGUSER=u\u{0}PGPASSWORD=***",
            ),
            // A quote inside a value is part of it (libpq accepts it).
            ("password=a'SECRET", "password=***"),
            ("password=a\"SECRET rest", "password=***"),
            // Pieces that would rebuild the marker once an inner copy is removed.
            (
                "password=\u{0}\u{1}\u{0}\u{1}masked\u{1}\u{0}masked\u{1}\u{0}LEAK",
                "password=***",
            ),
        ] {
            assert_eq!(text(input), expected, "{input:?}");
        }
        // Parameters after a masked URL password stay readable.
        assert_eq!(
            text("postgres://u@h/d?password=pw&sslmode=require"),
            "postgres://u@h/d?password=***&sslmode=require"
        );
        assert_eq!(
            text("postgres://u:***LEAK@h/d?password=***LEAK&x=1"),
            "postgres://u:***@h/d?password=***&x=1"
        );
    }

    #[test]
    fn common_secret_names_and_formats_are_masked() {
        let leaks = [
            ("export POSTGRES_PASSWORD=SENTINEL", "SENTINEL"),
            ("DB_PASSWORD=SENTINEL", "SENTINEL"),
            ("spring.datasource.password = SENTINEL", "SENTINEL"),
            (r#"{"password":"SENTINEL","x":1}"#, "SENTINEL"),
            ("PGPASSWORD: SENTINEL", "SENTINEL"),
            ("environment:\n  POSTGRES_PASSWORD: SENTINEL\n", "SENTINEL"),
            ("PGPASSWORD=FIRST SECOND", "SECOND"),
            ("PGUSER=u\nPGPASSWORD=FIRST SECOND\nPGHOST=h", "SECOND"),
            ("export PGPASSWORD=${DB_PASSWORD:-FIRST SECOND}", "SECOND"),
            ("export PGPASSWORD=$(printf %s SENTINEL)", "SENTINEL"),
            (
                "jdbc:sqlserver://h;user=u;password={FIRST SECOND};db=d",
                "SECOND",
            ),
            (
                "prisma+postgres://acc.example/?api_key=SENTINEL",
                "SENTINEL",
            ),
            ("GITHUB_TOKEN=SENTINEL", "SENTINEL"),
            ("export  PGPASSWORD=FIRST SECOND", "SECOND"),
            ("[hook] PGPASSWORD=FIRST SECOND", "SECOND"),
        ];
        for (input, secret) in leaks {
            let masked = text(input);
            assert!(!masked.contains(secret), "{input:?} -> {masked:?}");
        }
        // The next line of an env listing stays readable.
        assert!(text("PGPASSWORD=a b\nPGHOST=h").ends_with("\nPGHOST=h"));
        // A URL scheme is not a key.
        assert_eq!(text("secret://host/x"), "secret://host/x");
        let mut value = serde_json::json!({ "password": 123456, "token": ["a"], "name": null });
        json(&mut value);
        assert_eq!(value["password"], "***");
        assert_eq!(value["token"], "***");
        assert!(value["name"].is_null());
    }

    #[test]
    fn credentials_survive_no_libpq_or_shell_reading() {
        let leaks = [
            ("postgres://u:O'Reilly@h/db", "Reilly"),
            ("postgres://u@h/db?password=O'Reilly", "Reilly"),
            ("postgres://u@h?password=abc@DEF", "DEF"),
            ("postgres://u:old@h?password=abc@DEF", "DEF"), // kingfisher:ignore
            ("host=h password=\nSECRET dbname=d", "SECRET"),
            ("host=h password\n=SECRET dbname=d", "SECRET"),
            ("PGPASSWORD=abc\u{a0}SECRET psql", "SECRET"),
            ("PGPASSWORD='abc\\'def' SECRET' psql", "SECRET"),
            ("PGPASSWORD='abc\\'def' SECRET' psql", "def"),
        ];
        for (input, secret) in leaks {
            let masked = text(input);
            assert!(!masked.contains(secret), "{input:?} -> {masked:?}");
            let masked = text_with(input, &["unrelated"]);
            assert!(!masked.contains(secret), "{input:?} -> {masked:?}");
            let mut value = serde_json::json!({ "s": input });
            json(&mut value);
            assert!(!value.to_string().contains(secret), "{value}");
        }
        let mut value = serde_json::json!({ "postgres://u:pw@h/db": "failed" }); // kingfisher:ignore
        json(&mut value);
        assert!(value.get("postgres://u:***@h/db").is_some(), "{value}");
    }

    #[test]
    fn known_secrets_and_patterns_are_found_on_the_original_text() {
        // The pattern masks the value whole, so the known secret is gone too.
        assert_eq!(
            text_with("password=a'SECRET", &["a'SECRET"]),
            "password=***"
        );
        // A known secret overlapping a URL credential: the union is masked.
        assert_eq!(
            text_with("postgres://u:abcdef@h/d", &["defxyz", "cde"]), // kingfisher:ignore
            "postgres://u:***@h/d"
        );
    }

    #[test]
    fn a_short_known_secret_never_unprotects_another_value() {
        let masked = text_with("password=LEAK", &["pass"]);
        assert!(!masked.contains("LEAK"), "{masked}");
        let masked = text_with("PGPASSWORD=LEAK DATABASE_URL=postgres://u:pw@h/d", &["PG"]); // kingfisher:ignore
        assert!(
            !masked.contains("LEAK") && !masked.contains(":pw@"),
            "{masked}"
        );
    }

    #[test]
    fn hash_signs_never_end_a_masked_value() {
        assert_eq!(
            text("DATABASE_URL=postgres://u:abc#def@localhost/db"),
            "DATABASE_URL=postgres://u:***@localhost/db"
        );
        let masked = text("postgres://u@h/d?password=a#b&sslmode=require");
        assert!(
            !masked.contains("a#b") && !masked.contains("#b"),
            "{masked}"
        );
        let masked = text("host=h password=abc#def;x dbname=d");
        assert!(!masked.contains("def"), "{masked}");
    }

    #[test]
    fn masks_every_supported_credential_form() {
        let cases = [
            ("postgres://u:pw@h:5432/db", "postgres://u:***@h:5432/db"), // kingfisher:ignore
            (
                "postgres://u@h/db?password=pw&sslmode=disable",
                "postgres://u@h/db?password=***&sslmode=disable",
            ),
            ("host=h password=pw dbname=d", "host=h password=***"),
            ("PGPASSWORD=pw psql", "PGPASSWORD=***"),
            ("export PGPASSWORD='p w'", "export PGPASSWORD=***"),
            ("sslpassword=pw", "sslpassword=***"),
            ("no secrets here", "no secrets here"),
        ];
        for (input, expected) in cases {
            assert_eq!(text(input), expected, "{input}");
        }
        assert_eq!(
            text_with("echo \\'s3cret\\'", &["s3cret"]),
            "echo \\'***\\'"
        );
    }

    #[test]
    fn percent_encoded_url_parameter_names_are_decoded_before_the_check() {
        let cases = [
            (
                "postgres://u@localhost/db?%70assword=SECRET",
                "postgres://u@localhost/db?%70assword=***",
            ),
            (
                "postgresql://u@h/db?sslmode=disable&%50ASS%57ORD=S3&application_name=x",
                "postgresql://u@h/db?sslmode=disable&%50ASS%57ORD=***&application_name=x",
            ),
            (
                "postgres://u@h/db?ssl%70assword=S3#frag",
                "postgres://u@h/db?ssl%70assword=***",
            ),
            // The raw value is masked whatever its encoding.
            (
                "postgres://u@h/db?password=p%40ss%26x&sslmode=require",
                "postgres://u@h/db?password=***&sslmode=require",
            ),
            // Quoted in running text: only whitespace ends a URL, so the
            // closing quote is masked with the value.
            (
                "connect \"postgres://u@h/db?%70assword=S3\" now",
                "connect \"postgres://u@h/db?%70assword=*** now",
            ),
            // A URL inside another URL's parameter.
            (
                "https://x/?next=postgres://u:pw@h/db", // kingfisher:ignore
                "https://x/?next=postgres://u:***@h/db",
            ),
            (
                "https://x/?next=postgres://u@h/db?%70assword=S3",
                "https://x/?next=postgres://u@h/db?%70assword=***",
            ),
            // Non-secret parameters and empty values stay.
            (
                "postgres://u@h/db?schema=public&password=",
                "postgres://u@h/db?schema=public&password=",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(text(input), expected, "{input}");
        }
    }

    #[test]
    fn conninfo_assignments_allow_spaces_and_escaped_quotes() {
        let cases = [
            (
                "host=h password = 'SPACED_SECRET' dbname=d",
                "host=h password = *** dbname=d",
            ),
            (
                "host=h password   =\tSPACED dbname=d",
                "host=h password   =\t***",
            ),
            (
                "host=h password='it\\'s SECRET_SUFFIX' dbname=d",
                "host=h password=*** dbname=d",
            ),
            (
                "host=h password='back\\\\' dbname=d",
                "host=h password=*** dbname=d",
            ),
            ("host=h password=a\\ b dbname=d", "host=h password=***"),
            (
                "PGPASSWORD = \"a\\\"b SUFFIX\" psql",
                "PGPASSWORD = *** psql",
            ),
            // POSIX quoting of a single quote inside single quotes.
            ("PGPASSWORD='it'\\''s SECRET' psql", "PGPASSWORD=*** psql"),
            // A quote that may close the surrounding string has no partner in
            // the value: mask everything after it rather than guess.
            ("echo \"PGPASSWORD=pw\" psql", "echo \"PGPASSWORD=***"),
            ("run 'PGPASSWORD=pw'; next", "run 'PGPASSWORD=***"),
            // Unterminated: everything after the quote could be the secret.
            ("password='abc def", "password=***"),
            ("host=h sslPassword = pw x=1", "host=h sslPassword = ***"),
            // Multi-byte characters inside values, and repeated parameters.
            (
                "host=h password=p\u{e4}\u{1F512}ss dbname=d",
                "host=h password=***",
            ),
            (
                "postgres://u@h/d?password=a&password=b",
                "postgres://u@h/d?password=***&password=***",
            ),
            ("PGPASSWORD=", "PGPASSWORD="),
            // Not assignments.
            ("the password is wrong", "the password is wrong"),
            // Any secret-named key counts, not just libpq's.
            ("mypassword=x", "mypassword=***"),
            ("password=", "password="),
            ("password = ", "password = "),
        ];
        for (input, expected) in cases {
            assert_eq!(text(input), expected, "{input}");
        }
        // Masking again changes nothing.
        let once = text("host=h password = 'a\\'b' PGPASSWORD=x");
        assert_eq!(text(&once), once);
    }

    #[test]
    fn known_secrets_of_any_length_are_masked_longest_first() {
        // Shorter than four bytes.
        assert_eq!(text_with("pw and pwd", &["pw"]), "*** and ***d");
        assert_eq!(text_with("x=1 y=12", &["1"]), "x=*** y=***2");
        // A prefix of a longer secret must not mask first and leave a suffix.
        for order in [["abcd", "abcdef"], ["abcdef", "abcd"]] {
            assert_eq!(text_with("a abcdef b", &order), "a *** b", "{order:?}");
        }
        // Overlapping secrets and repeated occurrences collapse into one mask.
        assert_eq!(text_with("abcd", &["abc", "bcd"]), "***");
        assert_eq!(text_with("aaa", &["aa"]), "***");
        assert_eq!(text_with("abab", &["ab"]), "***");
        assert_eq!(
            text_with("xx s1 yy s2 zz", &["s1", "s2"]),
            "xx *** yy *** zz"
        );
        // Multi-byte characters and empty secrets.
        assert_eq!(text_with("pä\u{1F512}ss", &["ä\u{1F512}"]), "p***ss");
        assert_eq!(text_with("keep", &[""]), "keep");
        // Patterns still apply on top.
        assert_eq!(text_with("PGPASSWORD=zz and zz", &["zz"]), "PGPASSWORD=***");
    }

    #[test]
    fn masks_json_keys_pairs_and_strings() {
        let mut value = serde_json::json!({
            "password": "pw",
            "env": [["PGPASSWORD", "pw"], ["PGUSER", "u"]],
            "url": "postgres://u:pw@h/db", // kingfisher:ignore
        });
        json(&mut value);
        assert_eq!(value["password"], "***");
        assert_eq!(value["env"][0][1], "***");
        assert_eq!(value["env"][1][1], "u");
        assert_eq!(value["url"], "postgres://u:***@h/db");
    }

    #[test]
    fn user_chosen_keys_keep_their_structure() {
        // A service called `tokens` or `password_store` is still a binding.
        let mut value = serde_json::json!({
            "receipt": {
                "services": {
                    "tokens": {"engine": "valkey", "branch_id": "b1", "url": "redis://u:pw@h/0"}, // kingfisher:ignore
                    "password_store": {"engine": "postgres", "extras": {"secret_test": {"database": "d"}}},
                },
                "service_sources": {"tokens": "workspace:main", "password_store": "empty"},
                "service_source_ids": {"tokens": "branch:1"},
            },
            "catalog": {"snapshot_versions": {"token": 3}},
            "env": {"API_TOKEN": {"template": "${X}", "masked": true}, "PGPASSWORD": "pw"},
            "password": {"nested": "pw"},
            "secrets": ["a", 1],
            "tokens": [{"engine": "x"}, "y"],
        });
        json(&mut value);
        let services = &value["receipt"]["services"];
        assert_eq!(services["tokens"]["engine"], "valkey");
        assert_eq!(services["tokens"]["branch_id"], "b1");
        assert_eq!(services["tokens"]["url"], "redis://u:***@h/0");
        assert_eq!(services["password_store"]["engine"], "postgres");
        assert_eq!(
            services["password_store"]["extras"]["secret_test"]["database"],
            "d"
        );
        assert_eq!(
            value["receipt"]["service_sources"]["tokens"],
            "workspace:main"
        );
        assert_eq!(
            value["receipt"]["service_sources"]["password_store"],
            "empty"
        );
        assert_eq!(value["receipt"]["service_source_ids"]["tokens"], "branch:1");
        assert_eq!(value["catalog"]["snapshot_versions"]["token"], 3);
        // An object under a secret-like name keeps its shape; its scalars
        // under secret-like names are still masked.
        assert_eq!(value["env"]["API_TOKEN"]["template"], "${X}");
        assert_eq!(value["env"]["PGPASSWORD"], "***");
        // A list of plain values under a secret-like name is one secret.
        assert_eq!(value["secrets"], "***");
        assert_eq!(value["tokens"][0]["engine"], "x");
        assert_eq!(value["tokens"][1], "***");
    }

    #[test]
    fn only_environment_variable_names_make_a_pair() {
        // Two commands, the first of which mentions a password: not a pair.
        let mut value = serde_json::json!({
            "setup": ["echo password", "echo SECOND"],
            "hooks": [["echo secret", "echo THIRD"]],
        });
        let before = value.clone();
        json(&mut value);
        assert_eq!(value, before);

        let mut value = serde_json::json!({
            "env": [["MY_SECRET_TOKEN", "t"], ["_PASSWORD2", "p"], ["password", "lower"],
                    ["My_Password", "mixed"], ["1PASSWORD", "digit"], ["PGPASSWORD", ""]],
        });
        json(&mut value);
        assert_eq!(value["env"][0][1], "***");
        assert_eq!(value["env"][1][1], "***");
        assert_eq!(
            value["env"][2][1], "lower",
            "not an environment variable name"
        );
        assert_eq!(value["env"][3][1], "mixed");
        assert_eq!(value["env"][4][1], "digit");
        assert_eq!(value["env"][5][1], "***");
        // The strings of a non-pair array are still scanned.
        let mut value = serde_json::json!(["password", "PGPASSWORD=pw psql"]);
        json(&mut value);
        assert_eq!(value[0], "password");
        assert_eq!(value[1], "PGPASSWORD=***");
    }

    #[test]
    fn variable_names_that_hold_credentials_are_recognised_by_their_words() {
        for secret in [
            "APP_PASSWORD",
            "DB_PASS",
            "DBPASS",
            "PGPASSWORD",
            "DB_PWD",
            "SESSION_SECRET",
            "GITHUB_TOKEN",
            "API_KEY",
            "STRIPE_KEY",
            "SECRET_KEY_BASE",
            "SIGNINGKEY",
            "stripeKey",
            "SENTRY_DSN",
            "SSH_PASSPHRASE",
            "AWS_CREDENTIALS",
            "GOOGLE_APPLICATION_CREDENTIAL",
            "PRIVATE_KEY",
            "db.password",
        ] {
            assert!(is_secret_variable(secret), "{secret}");
        }
        for plain in [
            "DATABASE_URL",
            "DIRECT_URL",
            "APP_URL",
            "PORT",
            "NODE_ENV",
            "KEYCLOAK_REALM",
            "PASSENGER_APP_ENV",
            "FEATURE_FLAG",
            "HOSTNAME",
            "",
        ] {
            assert!(!is_secret_variable(plain), "{plain}");
        }
        // The narrow predicate stays narrow: JSON keys named `key` are common.
        assert!(!is_secret_key("key") && !is_secret_key("STRIPE_KEY"));
    }

    #[test]
    fn variable_shaped_keys_are_judged_as_variables_but_ordinary_fields_are_not() {
        // A map of variables (`scope.env.vars`): the names `is_secret_key`
        // misses are caught by the variable classifier.
        let mut value = serde_json::json!({
            "env": {"vars": {
                "DB_PASS": "literal", // kingfisher:ignore
                "SESSION_KEY": "literal2", // kingfisher:ignore
                "MY_CREDENTIAL": "literal3", // kingfisher:ignore
                "CACHE_PASS": "${REDIS_PASSWORD}",
                "APP_URL": "https://example.test",
                "FEATURE_FLAG": "on",
            }},
        });
        json(&mut value);
        let vars = &value["env"]["vars"];
        for masked in ["DB_PASS", "SESSION_KEY", "MY_CREDENTIAL"] {
            assert_eq!(vars[masked], "***", "{masked}");
        }
        // A reference holds no credential: it is the template, as `env list` shows it.
        assert_eq!(vars["CACHE_PASS"], "${REDIS_PASSWORD}");
        assert_eq!(vars["APP_URL"], "https://example.test");
        assert_eq!(vars["FEATURE_FLAG"], "on");

        // Ordinary field names are not variables: `key` is a service key, a
        // recipe's variable name, a snapshot name.
        let mut fields = serde_json::json!({
            "key": "cache", "id": "a", "pass": "x", "session_key": "y",
            "recipes": [{"key": "DB_PASS", "template": "${X}", "masked": false}],
            "pair": ["DB_PASS", "literal"], // kingfisher:ignore
        });
        json(&mut fields);
        assert_eq!(fields["key"], "cache");
        assert_eq!(fields["pass"], "x");
        assert_eq!(fields["session_key"], "y");
        assert_eq!(fields["recipes"][0]["key"], "DB_PASS");
        assert_eq!(fields["recipes"][0]["template"], "${X}");
        // A [NAME, value] pair uses the variable classifier too.
        assert_eq!(fields["pair"][1], "***");

        // An object under such a key keeps its structure (a1c6efe4).
        let mut nested = serde_json::json!({"DB_PASS": {"template": "${X}", "source": "local"}});
        json(&mut nested);
        assert_eq!(nested["DB_PASS"]["template"], "${X}");
        assert_eq!(nested["DB_PASS"]["source"], "local");
    }
}
