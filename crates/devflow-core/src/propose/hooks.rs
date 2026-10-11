//! What a hook command is for, so that a proposal does not add a second
//! `bun install` next to the `npm ci` a project already runs.
use crate::config::HookEntry;

/// The job a hook command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Purpose {
    /// Installing dependencies of one ecosystem (`js`, `python`, `ruby`, …).
    Install(String),
    MiseInstall,
    Codegen,
    Migrate,
    ComposeUp,
    ComposeDown,
}

/// The purposes of a shell command, one per `&&`/`;`/`|`-separated step.
/// Anything not recognized has no purpose.
pub(super) fn purposes_of(run: &str) -> Vec<Purpose> {
    let mut found = Vec::new();
    for segment in run.split(['\n', ';', '|', '&']) {
        let words = significant_words(segment);
        if words.is_empty() {
            continue;
        }
        let word = |i: usize| words.get(i).map(String::as_str);
        let install = |ecosystem: &str| Some(Purpose::Install(ecosystem.to_string()));
        let purpose = match (word(0), word(1)) {
            (Some("bun"), Some("install" | "i" | "ci")) => install("js"),
            (Some("pnpm"), Some("install" | "i")) => install("js"),
            (Some("yarn"), None | Some("install")) => install("js"),
            (Some("npm"), Some("ci" | "install" | "i" | "clean-install")) => install("js"),
            (Some("uv"), Some("sync")) => install("python"),
            (Some("poetry"), Some("install")) => install("python"),
            (Some("pip" | "pip3"), Some("install")) => install("python"),
            (Some("bundle"), None | Some("install")) => install("ruby"),
            (Some("composer"), Some("install")) => install("php"),
            (Some("mix"), Some("deps.get")) => install("elixir"),
            (Some("mise"), Some("install")) => Some(Purpose::MiseInstall),
            (Some("docker"), Some("compose")) if words.iter().any(|w| w == "up") => {
                Some(Purpose::ComposeUp)
            }
            (Some("docker-compose"), _) if words.iter().any(|w| w == "up") => {
                Some(Purpose::ComposeUp)
            }
            (Some("docker"), Some("compose")) if words.iter().any(|w| w == "down") => {
                Some(Purpose::ComposeDown)
            }
            (Some("docker-compose"), _) if words.iter().any(|w| w == "down") => {
                Some(Purpose::ComposeDown)
            }
            _ if words.iter().any(|w| is_migration_word(w)) => Some(Purpose::Migrate),
            _ if words.iter().any(|w| w == "generate" || w == "codegen") => Some(Purpose::Codegen),
            _ => None,
        };
        found.extend(purpose);
    }
    found
}

fn is_migration_word(word: &str) -> bool {
    word.contains("migrat")
        || matches!(
            word,
            "alembic" | "dbmate" | "goose" | "db:push" | "db:prepare" | "db:setup"
        )
}

/// The words of one command step with the wrappers removed: `cd dir`,
/// leading `VAR=value` assignments, `sudo`, `mise exec --`, `devflow exec --`,
/// `time`.
fn significant_words(segment: &str) -> Vec<String> {
    let mut words: Vec<String> = segment
        .split_whitespace()
        .map(|word| word.trim_matches(['"', '\'']).to_ascii_lowercase())
        .collect();
    loop {
        match words.first().map(String::as_str) {
            Some("cd") => {
                words.clear();
            }
            Some("sudo" | "time" | "exec" | "command") => {
                words.remove(0);
            }
            Some(first) if is_assignment(first) => {
                words.remove(0);
            }
            Some("mise" | "devflow")
                if words.get(1).map(String::as_str) == Some("exec")
                    || (words[0] == "mise" && words.get(1).map(String::as_str) == Some("x")) =>
            {
                let skip = words
                    .iter()
                    .position(|word| word == "--")
                    .map_or(words.len(), |at| at + 1);
                words.drain(..skip);
            }
            _ => break,
        }
    }
    words
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether the entries of a phase already do what `purpose` (or the exact
/// command `run`) does.
pub(super) fn covered(entries: &[HookEntry], purpose: &Purpose, run: &str) -> bool {
    entries
        .iter()
        .any(|entry| entry.run.trim() == run.trim() || purposes_of(&entry.run).contains(purpose))
}

/// A hook name made of the characters a person would type: lowercase
/// letters, digits and `-`, 64 characters at most.
pub(super) fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    out.chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(run: &str) -> Vec<Purpose> {
        purposes_of(run)
    }

    #[test]
    fn install_commands_are_recognized_through_wrappers() {
        let js = Purpose::Install("js".into());
        for run in [
            "bun install",
            "npm ci",
            "mise exec -- pnpm install --frozen-lockfile",
            "mise exec -- devflow exec -- pnpm install --frozen-lockfile",
            "cd apps/web && mise x -- devflow exec -- yarn",
            "cd apps/web && yarn",
            "NODE_ENV=development npm install",
        ] {
            assert!(one(run).contains(&js), "{run}");
        }
        assert!(one("uv sync --frozen").contains(&Purpose::Install("python".into())));
        assert!(
            one("mise trust").is_empty(),
            "devflow no longer proposes it"
        );
        assert!(one("mise install").contains(&Purpose::MiseInstall));
        assert!(one("bundle exec rake").is_empty());
    }

    #[test]
    fn migration_and_compose_steps() {
        for run in [
            "bunx prisma migrate deploy",
            "mise run db:migrate",
            "bin/rails db:migrate",
            "uv run python manage.py migrate --noinput",
            "mix ecto.migrate",
            "alembic upgrade head",
            "make migrate",
        ] {
            assert!(one(run).contains(&Purpose::Migrate), "{run}");
        }
        assert!(one("docker compose up -d --wait redis").contains(&Purpose::ComposeUp));
        assert!(one("docker compose down -v").contains(&Purpose::ComposeDown));
        assert!(one("bunx prisma generate").contains(&Purpose::Codegen));
    }

    #[test]
    fn slugs_are_short_and_plain() {
        assert_eq!(slug("apps/web"), "apps-web");
        assert_eq!(slug("  Hello, World!  "), "hello-world");
    }
}
