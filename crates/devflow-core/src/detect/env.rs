//! `.env*` files: variable *names* only.
//!
//! Files are streamed through a small state machine that keeps the text
//! before `=` and nothing else. Values are never stored, logged or returned;
//! quoted values (which may span lines and contain text that looks like
//! assignments) are tracked across lines and discarded byte by byte.
//!
//! This is the only code that opens `.env*` files (see `Repo::open_env`):
//! general readers refuse them, so a name that aliases a dotenv file cannot
//! make its values appear in other facts.
use super::facts::{EnvCategory, EnvFacts, EnvFile, EnvKey};
use super::manifest::Discovery;
use super::repo::{Repo, join};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;

/// Most `.env*` files one scan reads.
const MAX_FILES: usize = 40;
/// Most distinct infrastructure names kept per scan.
const MAX_KEYS: usize = 1000;
/// Most distinct names read from one file.
const MAX_NAMES_PER_FILE: usize = 5000;
/// Longest variable name.
const MAX_NAME: usize = 256;
/// UTF-8 byte-order mark some editors put at the start of a file.
const BYTE_ORDER_MARK: [u8; 3] = [0xef, 0xbb, 0xbf];

/// Whether a directory entry name is an env file (`.env`, `.env.local`, ...),
/// not direnv's `.envrc` and not an editor backup.
pub(super) fn is_env_file_name(name: &str) -> bool {
    (name == ".env" || name.starts_with(".env."))
        && !name.ends_with('~')
        && !name.ends_with(".bak")
        && !name.ends_with(".orig")
        && !name.ends_with(".swp")
}

pub(super) fn detect(repo: &Repo, discovery: &Discovery) -> EnvFacts {
    let mut facts = EnvFacts::default();
    let mut keys: BTreeMap<String, (EnvCategory, Vec<String>)> = BTreeMap::new();

    'projects: for project in &discovery.projects {
        for name in project.dir.files().filter(|n| is_env_file_name(n)) {
            if facts.files.len() >= MAX_FILES {
                repo.warn(
                    ".",
                    format!("only the first {MAX_FILES} .env files were read"),
                );
                break 'projects;
            }
            let path = join(&project.path, name);
            let Some(file) = repo.open_env(&path) else {
                continue;
            };
            let scan = read_names(file);
            if scan.truncated {
                repo.warn(
                    &path,
                    format!("only the first {MAX_NAMES_PER_FILE} variable names were read"),
                );
            }
            if scan.unterminated {
                repo.warn(
                    &path,
                    "a quoted value is never closed; the rest of the file was ignored",
                );
            }
            let mut has_database_keys = false;
            let key_count = scan.names.len();
            for name in scan.names {
                let category = classify(&name);
                has_database_keys |= category.is_datastore();
                if category == EnvCategory::Other {
                    continue;
                }
                if keys.len() >= MAX_KEYS && !keys.contains_key(&name) {
                    continue;
                }
                let entry = keys.entry(name).or_insert((category, Vec::new()));
                if !entry.1.contains(&path) {
                    entry.1.push(path.clone());
                }
            }
            facts.has_database_keys |= has_database_keys;
            facts.evidence.push(path.clone());
            facts.files.push(EnvFile {
                path,
                key_count,
                has_database_keys,
            });
        }
    }
    facts.keys = keys
        .into_iter()
        .map(|(name, (category, files))| EnvKey {
            name,
            category,
            files,
        })
        .collect();
    facts
}

/// What a dotenv stream defines.
#[derive(Debug, Default, PartialEq, Eq)]
struct NameScan {
    /// Variable names in order of appearance, without repeats.
    names: Vec<String>,
    /// Reading stopped at [`MAX_NAMES_PER_FILE`] names.
    truncated: bool,
    /// A quote was still open at the end of the stream.
    unterminated: bool,
}

/// Where the parser is within the stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before a key: blank space, or a new line.
    LineStart,
    /// A line start right after `NAME=` with nothing after the `=`. Some
    /// parsers read whitespace across lines there (Node's `dotenv` takes
    /// `\s*` before a quote), so a quote opens the value of that name, and it
    /// does here too.
    AfterEmpty,
    /// Inside a key, up to `=`.
    Key,
    /// After `=`, before the first byte of the value.
    BeforeValue,
    /// Inside a quoted value; `backslash` when the previous byte was one.
    Quoted { quote: u8, backslash: bool },
    /// The rest of the line is a value, a comment or junk: dropped.
    Discard,
}

/// Whether a byte is blank space that is not a line break. Dotenv parsers
/// disagree on what that is: Python's `\s` also matches vertical tab, form
/// feed and the separator controls (and Unicode spaces, see [`read_names`]), so
/// all of them count, and a quote after any of them still opens a string.
fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x0b | 0x0c | b'\r' | 0x1c..=0x1f)
}

/// Variable names defined in a dotenv stream, in order, without repeats.
///
/// A line is `[export ]NAME=value`. A value that starts with a quote
/// (`"`, `'` or a backtick) runs to the matching quote, across lines, and a
/// quote that comes right after a backslash does not close it, in any kind of
/// quote. Blank space before the quote (any ASCII space but a line break, and
/// any byte that may begin a Unicode space) does not matter, and neither does
/// a line break when nothing but blank space follows the `=`.
/// Anything else runs to the end of the line. Lines that are not
/// assignments yield nothing, and neither does anything inside a value. A
/// quote that is never closed swallows the rest of the stream: the lines
/// after it cannot be told from its value.
///
/// Dotenv parsers disagree about quoting (whether a backslash escapes in single
/// quotes, whether `\\` pairs up, what counts as blank space, whether a value
/// may start on the next line), and the scan cannot know which one the
/// project's tooling uses. Wherever they differ it takes the reading under
/// which more text is inside a value, because a name wrongly hidden is a
/// missed hint and a name wrongly read out of a value is a leak.
///
/// Whether a quoted value is open depends only on the `=` and the quote after
/// it, never on whether the text before the `=` is a usable name: a line
/// with a byte-order mark, an overlong or otherwise invalid name still opens
/// its string, or the lines inside that string would be read as assignments.
/// A leading UTF-8 byte-order mark is skipped.
fn read_names(mut reader: impl Read) -> NameScan {
    let mut scan = NameScan::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut state = State::LineStart;
    let mut key: Vec<u8> = Vec::new();
    let mut key_overflow = false;
    let mut buffer = [0u8; 8192];
    // Read the first bytes up front to recognize a byte-order mark; anything
    // else is handed back to the parser.
    let mut head = [0u8; 3];
    let mut have = 0;
    while have < head.len() {
        match reader.read(&mut head[have..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => have += read,
        }
    }
    let replay = if head[..have] == BYTE_ORDER_MARK {
        &head[..0]
    } else {
        &head[..have]
    };
    let mut reader = replay.chain(reader);
    'stream: loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for &byte in &buffer[..read] {
            state = match state {
                at_start @ (State::LineStart | State::AfterEmpty) => match byte {
                    b'\n' => at_start,
                    _ if is_blank(byte) => at_start,
                    b'"' | b'\'' | b'`' if at_start == State::AfterEmpty => State::Quoted {
                        quote: byte,
                        backslash: false,
                    },
                    b'#' => State::Discard,
                    // No name at all, but a value (and so a string) follows.
                    b'=' => State::BeforeValue,
                    _ => {
                        key.clear();
                        key.push(byte);
                        key_overflow = false;
                        State::Key
                    }
                },
                State::Key => match byte {
                    b'\n' => State::LineStart,
                    b'=' => {
                        // A usable name is kept; either way a value starts here.
                        let name = if key_overflow { None } else { parse_name(&key) };
                        if let Some(name) = name
                            && !seen.contains(&name)
                        {
                            if seen.len() >= MAX_NAMES_PER_FILE {
                                scan.truncated = true;
                                break 'stream;
                            }
                            seen.insert(name.clone());
                            scan.names.push(name);
                        }
                        State::BeforeValue
                    }
                    _ => {
                        // Longer than any name (with room for `export` and
                        // blanks): not stored, but the line is still read up
                        // to its `=`.
                        if key.len() > MAX_NAME + 16 {
                            key_overflow = true;
                        } else {
                            key.push(byte);
                        }
                        State::Key
                    }
                },
                State::BeforeValue => match byte {
                    b'\n' => State::AfterEmpty,
                    b'"' | b'\'' | b'`' => State::Quoted {
                        quote: byte,
                        backslash: false,
                    },
                    // Blank space, and any byte that may begin a multi-byte
                    // Unicode space (U+0085, U+00A0, U+2003, ...): a quote
                    // after one still opens the string for a parser that
                    // reads Unicode. At worst a quote after other non-ASCII
                    // text is taken for an opening one, and the scan reads
                    // less.
                    _ if is_blank(byte) || byte >= 0x80 => State::BeforeValue,
                    _ => State::Discard,
                },
                // A quote right after a backslash never closes the value, in
                // every kind of quote and however many backslashes came
                // before. Parsers disagree (backslash escapes in single quotes
                // for some and not for others, `\\` pairs up for some and
                // not for Python's `\\'` alternative), and every reading that
                // keeps the quote inside the value only hides more names.
                // The price is a value such as `'C:\dir\'` that ends in a
                // backslash: it stays open to the next quote, or to the end of
                // the file, which is reported.
                State::Quoted { quote, backslash } => {
                    if byte == quote && !backslash {
                        State::Discard
                    } else {
                        State::Quoted {
                            quote,
                            backslash: byte == b'\\',
                        }
                    }
                }
                State::Discard => {
                    if byte == b'\n' {
                        State::LineStart
                    } else {
                        State::Discard
                    }
                }
            };
        }
    }
    scan.unterminated = matches!(state, State::Quoted { .. });
    scan
}

/// `NAME`, `export NAME` and `NAME ` (before the `=`) yield `NAME`; anything
/// that is not a variable name yields nothing.
fn parse_name(key: &[u8]) -> Option<String> {
    fn trim(bytes: &[u8]) -> &[u8] {
        let blank = |b: &u8| is_blank(*b) || *b == b'\n';
        let start = bytes.iter().position(|b| !blank(b)).unwrap_or(bytes.len());
        let end = bytes
            .iter()
            .rposition(|b| !blank(b))
            .map_or(start, |i| i + 1);
        &bytes[start..end]
    }
    let mut key = trim(key);
    if let Some(rest) = key.strip_prefix(b"export")
        && rest.first().is_some_and(|b| is_blank(*b))
    {
        key = trim(rest);
    }
    let valid = !key.is_empty()
        && key.len() <= MAX_NAME
        && !key[0].is_ascii_digit()
        && key
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.');
    valid.then(|| String::from_utf8_lossy(key).into_owned())
}

/// What a variable name looks like.
pub(super) fn classify(name: &str) -> EnvCategory {
    let n = name.to_ascii_uppercase();
    let n = n.as_str();
    const URLS: [&str; 17] = [
        "DATABASE_URL",
        "DIRECT_URL",
        "SHADOW_DATABASE_URL",
        "DB_URL",
        "DATABASE_URI",
        "DB_URI",
        "DB_CONNECTION_STRING",
        "DATABASE_CONNECTION_STRING",
        "POSTGRES_URL",
        "POSTGRES_URI",
        "POSTGRES_PRISMA_URL",
        "POSTGRES_URL_NON_POOLING",
        "POSTGRESQL_URL",
        "MYSQL_URL",
        "SQLALCHEMY_DATABASE_URI",
        "SQLALCHEMY_DATABASE_URL",
        "DATABASE_PRIVATE_URL",
    ];
    const LIBPQ: [&str; 12] = [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGDATABASE",
        "PGUSER",
        "PGPASSWORD",
        "PGPASSFILE",
        "PGSERVICE",
        "PGSSLMODE",
        "PGOPTIONS",
        "PGCONNECT_TIMEOUT",
        "PGURL",
    ];
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| n.starts_with(p));
    if URLS.contains(&n)
        || n.ends_with("_DATABASE_URL")
        || n.ends_with("_DB_URL")
        || n.ends_with("_DATABASE_URI")
    {
        EnvCategory::DatabaseUrl
    } else if LIBPQ.contains(&n) || starts(&["POSTGRES_", "POSTGRESQL_", "PG_"]) {
        EnvCategory::Postgres
    } else if starts(&["MYSQL_", "MARIADB_"]) {
        EnvCategory::Mysql
    } else if starts(&["DB_", "DATABASE_", "SPRING_DATASOURCE_"]) {
        EnvCategory::Database
    } else if starts(&["REDIS_", "VALKEY_"]) || n == "CACHE_URL" || n == "REDIS" {
        EnvCategory::Redis
    } else if starts(&["MONGODB_", "MONGO_"]) {
        EnvCategory::Mongo
    } else if starts(&["CLICKHOUSE_"]) {
        EnvCategory::Clickhouse
    } else if starts(&["S3_", "AWS_S3_", "MINIO_"]) {
        EnvCategory::ObjectStorage
    } else if starts(&["RABBITMQ_", "AMQP_", "KAFKA_", "CLOUDAMQP_"]) {
        EnvCategory::Broker
    } else if starts(&["ELASTICSEARCH_", "OPENSEARCH_", "MEILI", "TYPESENSE_"]) {
        EnvCategory::Search
    } else if starts(&["SMTP_", "MAIL_", "MAILER_"]) {
        EnvCategory::Mail
    } else if n == "PORT" || n.ends_with("_PORT") {
        EnvCategory::Port
    } else {
        EnvCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        read_names(text.as_bytes()).names
    }

    #[test]
    fn only_names_are_extracted() {
        let text = "# comment\nDATABASE_URL=VALUE_MUST_NOT_LEAK\nexport REDIS_URL = 'x'\n\nnot an assignment\nPORT=3000\nDATABASE_URL=again\n1BAD=x\nMULTI=\"line1\nline2\"\nAFTER=1\n";
        assert_eq!(
            names(text),
            vec!["DATABASE_URL", "REDIS_URL", "PORT", "MULTI", "AFTER"]
        );
    }

    #[test]
    fn overlong_lines_do_not_leak_into_the_next_name() {
        let long = format!("A={}\nB=1\n", "x".repeat(10_000));
        assert_eq!(names(&long), vec!["A", "B"]);
        let long_key = format!("{}=1\nB=1\n", "K".repeat(10_000));
        assert_eq!(names(&long_key), vec!["B"]);
    }

    #[test]
    fn quoted_values_hide_the_lines_they_span() {
        for quote in ['"', '\'', '`'] {
            let text = format!(
                "A={quote}line1\nDB_SECRET_ABC=rest\n  export TOKEN=x\nline3{quote}\nB=1\n"
            );
            assert_eq!(names(&text), vec!["A", "B"], "quote {quote}");
        }
    }

    #[test]
    fn escapes_and_quote_kinds() {
        // An escaped quote does not close a double-quoted value.
        assert_eq!(names("A=\"x\\\"\nHIDDEN=1\"\nB=1\n"), vec!["A", "B"]);
        // Parsers disagree on whether single quotes have escapes, so a quote
        // after a backslash never closes one: this value stays open.
        let scan = read_names("A='C:\\'\nB=1\n".as_bytes());
        assert_eq!(scan.names, vec!["A"]);
        assert!(scan.unterminated);
        // The other quote kind and `#` inside a value are plain bytes.
        assert_eq!(
            names("A=\"it's\"\nB='say \"hi\"'\nC=1\n"),
            vec!["A", "B", "C"]
        );
        assert_eq!(names("A=x # B=1\nC=1\n"), vec!["A", "C"]);
        // A value that merely contains a quote is not quoted.
        assert_eq!(names("A=it's\nB=1\n"), vec!["A", "B"]);
        // Text after the closing quote is dropped with the line.
        assert_eq!(names("A=\"x\" D=1\nB=1\n"), vec!["A", "B"]);
        // Windows line endings.
        assert_eq!(names("A=\"x\r\ny\"\r\nB=1\r\n"), vec!["A", "B"]);
    }

    #[test]
    fn an_unterminated_quote_swallows_the_rest() {
        let scan = read_names("A=\"never closed\nB=1\nC=2\n".as_bytes());
        assert_eq!(scan.names, vec!["A"]);
        assert!(scan.unterminated);
        assert!(!read_names("A=\"closed\"\n".as_bytes()).unterminated);
    }

    #[test]
    fn many_names_are_capped_without_quadratic_work() {
        let text: String = (0..60_000).map(|i| format!("NAME_{i}=1\n")).collect();
        let started = std::time::Instant::now();
        let scan = read_names(text.as_bytes());
        assert_eq!(scan.names.len(), MAX_NAMES_PER_FILE);
        assert!(scan.truncated);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // Repeats never count against the cap.
        let repeats: String = (0..60_000).map(|_| "SAME=1\n").collect();
        assert_eq!(names(&repeats), vec!["SAME"]);
    }

    #[test]
    fn r2_finding_5_a_rejected_name_still_opens_its_quoted_value() {
        let long = "A".repeat(MAX_NAME + 1);
        let huge = "B".repeat(100_000);
        for (label, text) in [
            ("bom", "\u{feff}X=\"a\nHIDDEN=1\n\"\nAFTER=1\n".to_string()),
            (
                "bom, bad name",
                "\u{feff}X Y=\"a\nHIDDEN=1\"\nAFTER=1\n".into(),
            ),
            (
                "name with a bom",
                "OK=1\nX\u{feff}Y=\"a\nHIDDEN=1\"\nAFTER=1\n".into(),
            ),
            ("257", format!("{long}='a\nHIDDEN=1'\nAFTER=1\n")),
            ("huge", format!("{huge}=`a\nHIDDEN=1`\nAFTER=1\n")),
            ("digit", "1X=\"a\nHIDDEN=1\"\nAFTER=1\n".to_string()),
            ("empty", "=\"a\nHIDDEN=1\"\nAFTER=1\n".to_string()),
        ] {
            let scan = read_names(text.as_bytes());
            assert!(!scan.names.contains(&"HIDDEN".to_string()), "{label}");
            assert!(scan.names.contains(&"AFTER".to_string()), "{label}");
            assert!(!scan.unterminated, "{label}");
        }
        // The longest legal name is kept.
        let longest = "N".repeat(MAX_NAME);
        assert_eq!(names(&format!("{longest}=1\n")), vec![longest]);
    }

    #[test]
    fn r2_finding_5_only_a_leading_byte_order_mark_is_skipped() {
        assert_eq!(names("\u{feff}A=1\nB=2\n"), vec!["A", "B"]);
        // Nothing but the mark, or less than the mark, is not an error.
        assert!(names("\u{feff}").is_empty());
        assert!(read_names(&[0xef, 0xbb][..]).names.is_empty());
        // A mark that is not first belongs to the name, which is then invalid.
        assert_eq!(names("\n\u{feff}A=1\nB=2\n"), vec!["B"]);
        // The first bytes are replayed when they are not a mark.
        assert_eq!(names("A=1"), vec!["A"]);
        assert_eq!(names("ab"), Vec::<String>::new());
        // A reader that hands out one byte at a time sees the same thing.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                buf[0] = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        assert_eq!(
            read_names(Trickle("\u{feff}A=\"x\nHIDDEN=1\"\nB=1\n".as_bytes())).names,
            vec!["A", "B"]
        );
        assert_eq!(read_names(Trickle(b"A=1\nB=1\n")).names, vec!["A", "B"]);
    }

    // -- review round 3, finding 1: quoting variants -----------------------------------

    /// The reviewer's fixture: python-dotenv reads `\'` inside single quotes as
    /// an escaped quote, so the next lines are the value of `SECRET`.
    #[test]
    fn r3_finding_1_an_escaped_quote_does_not_close_a_single_quoted_value() {
        let scan =
            read_names("SECRET='first\\'part\nDB_SECRET_CANARY=tail\n'\nPORT=1\n".as_bytes());
        assert_eq!(scan.names, vec!["SECRET", "PORT"]);
        assert!(!scan.unterminated);
    }

    #[test]
    fn r3_finding_1_a_quote_after_a_backslash_never_closes_in_any_quote_kind() {
        for quote in ['"', '\'', '`'] {
            // An escaped quote.
            let text = format!("S={quote}a\\{quote}b\nDB_CANARY=x\n{quote}\nPORT=1\n");
            assert_eq!(names(&text), vec!["S", "PORT"], "escaped, quote {quote}");
            // Backslashes do not pair up: Python's `\\'` alternative reads the
            // quote after two of them as escaped as well.
            let text = format!("S={quote}a\\\\{quote}\nDB_CANARY=x\n{quote}\nPORT=1\n");
            assert_eq!(names(&text), vec!["S", "PORT"], "doubled, quote {quote}");
            // A value that ends in a backslash stays open, and says so.
            let scan = read_names(format!("S={quote}a\\{quote}\nDB_CANARY=x\n").as_bytes());
            assert_eq!(scan.names, vec!["S"], "open, quote {quote}");
            assert!(scan.unterminated, "open, quote {quote}");
            // The first quote with no backslash before it closes the value.
            let text = format!("S={quote}a\\{quote}b{quote}\nPORT=1\n");
            assert_eq!(names(&text), vec!["S", "PORT"], "closed, quote {quote}");
        }
    }

    #[test]
    fn r3_finding_1_any_blank_space_before_a_quote_still_opens_the_string() {
        for blank in [
            ' ', '\t', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}',
            // Unicode spaces (UTF-8 lead bytes from 0x80 up).
            '\u{85}', '\u{a0}', '\u{1680}', '\u{2003}', '\u{2028}', '\u{3000}',
        ] {
            for quote in ['"', '\'', '`'] {
                let text = format!("A={blank}{quote}x\nDB_CANARY=1\n{quote}\nPORT=1\n");
                let scan = read_names(text.as_bytes());
                assert_eq!(scan.names, vec!["A", "PORT"], "{blank:?} {quote}");
                assert!(!scan.unterminated, "{blank:?} {quote}");
            }
            // Mixed, and after more than one.
            let text = format!("A= {blank}\t{blank}\"x\nDB_CANARY=1\n\"\nPORT=1\n");
            assert_eq!(names(&text), vec!["A", "PORT"], "{blank:?} run");
        }
        // Blank space is not a reason to take a plain value for a string.
        assert_eq!(names("A=\u{b}x\"y\nB=1\n"), vec!["A", "B"]);
        // The same blank space around names.
        assert_eq!(
            names("\u{b}A=1\n\u{c}export\u{b}B=2\nC\u{b}=3\n"),
            vec!["A", "B", "C"]
        );
    }

    /// Node's `dotenv` reads `\s*` before a quote, line breaks included, so a
    /// quote that starts the line after `NAME=` is NAME's value.
    #[test]
    fn r3_finding_1_a_quote_on_the_line_after_an_empty_assignment_opens_the_string() {
        for quote in ['"', '\'', '`'] {
            let text = format!("A=\n  {quote}x\nDB_CANARY=1\n{quote}\nPORT=1\n");
            assert_eq!(names(&text), vec!["A", "PORT"], "quote {quote}");
            let text = format!("A=\r\n\r\n\u{b}\n{quote}x\nDB_CANARY=1\n{quote}\nPORT=1\n");
            assert_eq!(names(&text), vec!["A", "PORT"], "crlf, quote {quote}");
        }
        // Without a quote nothing changes, and a comment ends the carry-over.
        assert_eq!(names("A=\n\nB=1\nC=\n# c\nD=1\n"), vec!["A", "B", "C", "D"]);
        // A quote after a value with text in it is not carried over.
        assert_eq!(names("A=x\n\"y\nB=1\n"), vec!["A", "B"]);
    }

    #[test]
    fn names_with_odd_spacing() {
        assert_eq!(
            names("export\tA=1\n  B =2\nexportC=3\n"),
            vec!["A", "B", "exportC"]
        );
        assert_eq!(names("=novalue\nA.B=1\n"), vec!["A.B"]);
    }

    #[test]
    fn classification() {
        assert_eq!(classify("DATABASE_URL"), EnvCategory::DatabaseUrl);
        assert_eq!(classify("DIRECT_URL"), EnvCategory::DatabaseUrl);
        assert_eq!(classify("TEST_DATABASE_URL"), EnvCategory::DatabaseUrl);
        assert_eq!(classify("POSTGRES_PASSWORD"), EnvCategory::Postgres);
        assert_eq!(classify("PGHOST"), EnvCategory::Postgres);
        assert_eq!(classify("PGP_PRIVATE_KEY"), EnvCategory::Other);
        assert_eq!(classify("DB_HOST"), EnvCategory::Database);
        assert_eq!(classify("MYSQL_ROOT_PASSWORD"), EnvCategory::Mysql);
        assert_eq!(classify("REDIS_URL"), EnvCategory::Redis);
        assert_eq!(classify("MONGODB_URI"), EnvCategory::Mongo);
        assert_eq!(classify("MONGO_URL"), EnvCategory::Mongo);
        assert_eq!(classify("CLICKHOUSE_HOST"), EnvCategory::Clickhouse);
        assert_eq!(classify("S3_BUCKET"), EnvCategory::ObjectStorage);
        assert_eq!(classify("PORT"), EnvCategory::Port);
        assert_eq!(classify("SECRET_KEY_BASE"), EnvCategory::Other);
    }

    #[test]
    fn env_file_names() {
        assert!(is_env_file_name(".env"));
        assert!(is_env_file_name(".env.local"));
        assert!(is_env_file_name(".env.example"));
        assert!(!is_env_file_name(".envrc"));
        assert!(!is_env_file_name(".env.bak"));
        assert!(!is_env_file_name(".environment"));
    }
}
