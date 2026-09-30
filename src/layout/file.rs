use std::collections::BTreeMap;

use anyhow::Context as _;

use crate::layout::merge;
use crate::layout::model;


// ==============
// === FORMAT ===
// ==============

const FORMAT: i64 = 1;


// ===============
// === Parsing ===
// ===============

const MARKERS: [&str; 4] = ["<<<<<<<", "|||||||", "=======", ">>>>>>>"];

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    format: Option<i64>,
    repos: Option<BTreeMap<String, RawEntry>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    url: String,
}

/// Parses `repos.toml`. Any valid TOML is accepted, not just the canonical form; unresolved conflict markers are
/// refused before parsing.
pub(crate) fn parse(text: &str) -> anyhow::Result<model::Layout> {
    reject_conflict_markers(text)?;
    let raw = toml::from_str::<RawLayout>(text).context("repos.toml is not a valid layout file")?;
    check_format(raw.format)?;
    let repos = raw
        .repos
        .unwrap_or_default()
        .into_iter()
        .map(|(key, entry)| {
            let path = key.parse().with_context(|| format!("repos.toml has an invalid entry {key:?}"))?;
            let url = entry.url.parse().with_context(|| format!("repos.toml entry {key:?} has an invalid url"))?;
            Ok(model::LayoutRepo { path, url })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    model::Layout::from_repos(repos).context("repos.toml has overlapping entries")
}

/// Like `parse`, but empty or whitespace-only text is an empty layout: git passes an empty file to a merge driver when
/// the file didn't exist at the merge base.
pub(crate) fn parse_merge_input(text: &str) -> anyhow::Result<model::Layout> {
    match text.trim().is_empty() {
        true => Ok(model::Layout::default()),
        false => parse(text),
    }
}

fn reject_conflict_markers(text: &str) -> anyhow::Result<()> {
    let lines = text
        .lines()
        .zip(1..)
        .filter(|(line, _)| MARKERS.iter().any(|marker| line.starts_with(marker)))
        .map(|(_, number)| number.to_string())
        .collect::<Vec<_>>();
    match lines.as_slice() {
        [] => Ok(()),
        [line] => Err(anyhow::anyhow!(
            "repos.toml has an unresolved conflict marker on line {line}; resolve it, then run `dev_sync pull \
             --continue`"
        )),
        _ => Err(anyhow::anyhow!(
            "repos.toml has unresolved conflict markers on lines {}; resolve them, then run `dev_sync pull \
             --continue`",
            lines.join(", ")
        )),
    }
}

fn check_format(format: Option<i64>) -> anyhow::Result<()> {
    match format {
        None => Err(anyhow::anyhow!("repos.toml has no `format` key")),
        Some(FORMAT) => Ok(()),
        Some(newer) if newer > FORMAT => Err(anyhow::anyhow!(
            "repos.toml uses format {newer}, but this dev_sync only understands format {FORMAT} — install a newer \
             dev_sync"
        )),
        Some(other) => Err(anyhow::anyhow!("repos.toml has an invalid format {other}; expected {FORMAT}")),
    }
}


// =================
// === Rendering ===
// =================

const HEADER: &str = "# dev_sync workspace layout: repository path -> origin URL.\n\
                      # Written by dev_sync. Edit it by hand only to resolve a merge conflict.\n";

/// The canonical form: a header, then one line per repository sorted by path. Byte-stable, and `parse` reads it back
/// unchanged.
pub(crate) fn render(layout: &model::Layout) -> String {
    format!("{}{}", preamble(), layout.repos().map(|repo| line(&repo)).collect::<String>())
}

/// The canonical form of everything that merged, then one block per conflict with git-style markers around each side.
pub(crate) fn render_conflicted(merged: &merge::ConflictedMerge) -> String {
    let resolved = merged.resolved.repos().map(|repo| line(&repo)).collect::<String>();
    let blocks = merged.conflicts.iter().map(|conflict| {
        let local = conflict.local_repos.iter().map(line).collect::<String>();
        let incoming = conflict.incoming_repos.iter().map(line).collect::<String>();
        format!(
            "\n# CONFLICT: {conflict}\n\
             # Keep the lines you want, delete the rest and the marker lines, then run dev_sync pull --continue\n\
             <<<<<<< local\n{local}=======\n{incoming}>>>>>>> incoming\n"
        )
    });
    format!("{}{resolved}{}", preamble(), blocks.collect::<String>())
}

fn preamble() -> String {
    format!("{HEADER}format = {FORMAT}\n\n[repos]\n")
}

fn line(repo: &model::LayoutRepo) -> String {
    format!("{}\n", render_entry(repo))
}

/// One repository's line in the canonical form, without the line break.
pub(crate) fn render_entry(repo: &model::LayoutRepo) -> String {
    format!("{} = {{ url = {} }}", quote(repo.path.as_str()), quote(repo.url.as_str()))
}

/// A TOML basic string. Paths and URLs can't hold control characters, so only `\` and `"` need escaping.
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use crate::layout::merge::ConflictedMerge;
    use crate::layout::merge::MergeOutcome;
    use crate::layout::merge::merge;
    use crate::layout::model::Layout;
    use crate::layout::model::LayoutRepo;
    use super::parse;
    use super::parse_merge_input;
    use super::render;
    use super::render_conflicted;

    const HEADER: &str = "# dev_sync workspace layout: repository path -> origin URL.\n\
                          # Written by dev_sync. Edit it by hand only to resolve a merge conflict.\n\
                          format = 1\n\n[repos]\n";

    fn error_of(text: &str) -> String {
        parse(text).map_or_else(|error| format!("{error:#}"), |layout| format!("parsed: {layout:?}"))
    }

    fn conflicted(base: &str, local: &str, incoming: &str) -> anyhow::Result<ConflictedMerge> {
        match merge(&fixtures::layout(base)?, &fixtures::layout(local)?, &fixtures::layout(incoming)?) {
            MergeOutcome::Conflicted(conflicted) => Ok(conflicted),
            MergeOutcome::Clean(layout) => anyhow::bail!("expected a conflict, got {layout:?}"),
        }
    }

    #[test]
    fn renders_the_canonical_form() -> anyhow::Result<()> {
        let layout = fixtures::layout(
            "claude-status=git@github.com:ferrisoft/am.git ferrisoft/setup=git@github.com:ferrisoft/handbook.git \
             account_manager=git@github.com:ferrisoft/am.git",
        )?;
        let expected = format!(
            "{HEADER}\"account_manager\" = {{ url = \"git@github.com:ferrisoft/am.git\" }}\n\
             \"claude-status\" = {{ url = \"git@github.com:ferrisoft/am.git\" }}\n\
             \"ferrisoft/setup\" = {{ url = \"git@github.com:ferrisoft/handbook.git\" }}\n"
        );
        assert_eq!(render(&layout), expected);
        assert_eq!(render(&parse(&expected)?), expected);
        Ok(())
    }

    #[test]
    fn renders_an_empty_layout_ending_after_the_table_header() {
        assert_eq!(render(&Layout::default()), HEADER);
    }

    #[test]
    fn round_trips_unusual_paths_and_urls() -> anyhow::Result<()> {
        let repos = [
            ("zażółć gęślą/my repo", "/tmp/it's a \"world\"/r.git"),
            ("back\\slash", "https://example.com/a b.git"),
            ("a\"quote/x", "C:\\weird\\path"),
        ]
        .into_iter()
        .map(|(path, url)| Ok(LayoutRepo { path: fixtures::path(path)?, url: fixtures::url(url)? }))
        .collect::<anyhow::Result<Vec<_>>>()?;
        let layout = Layout::from_repos(repos)?;
        let text = render(&layout);
        assert_eq!(parse(&text)?, layout);
        assert_eq!(render(&parse(&text)?), text);
        assert!(text.ends_with(" }\n") && !text.ends_with("\n\n"));
        Ok(())
    }

    #[test]
    fn accepts_any_valid_toml() -> anyhow::Result<()> {
        let text = "format = 1\n[repos.\"a\"]\nurl = \"u1\"\n[repos.b]\nurl = 'u2' # comment\n";
        assert_eq!(parse(text)?, fixtures::layout("a=u1 b=u2")?);
        assert_eq!(parse("format = 1\n")?, Layout::default());
        Ok(())
    }

    #[test]
    fn rejects_bad_files_with_helpful_messages() {
        assert!(error_of("[repos]\n").contains("format"));
        assert!(error_of("format = 2\n").contains(
            "repos.toml uses format 2, but this dev_sync only understands format 1 — install a newer dev_sync"
        ));
        assert!(error_of("format = 0\n").contains("invalid"));
        assert!(error_of("format = \"1\"\n").contains("format"));
        assert!(error_of("format = 1\nextra = true\n").contains("extra"));
        assert!(error_of("format = 1\n[repos]\n\"a\" = { url = \"u\", branch = \"main\" }\n").contains("branch"));
        assert!(error_of("format = 1\n[repos]\n\"a/../b\" = { url = \"u\" }\n").contains("a/../b"));
        assert!(error_of("format = 1\n[repos]\n\"a\" = { url = \"-x\" }\n").contains("\"a\""));
        let nested = "format = 1\n[repos]\n\"a\" = { url = \"u\" }\n\"a/b\" = { url = \"v\" }\n";
        assert!(error_of(nested).contains("inside"));
    }

    #[test]
    fn reports_every_conflict_marker_line() {
        let text = "format = 1\n[repos]\n<<<<<<< local\n\"a\" = { url = \"u1\" }\n=======\n\
                    \"a\" = { url = \"u2\" }\n>>>>>>> incoming\n";
        let message = error_of(text);
        assert!(message.contains("lines 3, 5, 7"), "{message}");
        assert!(message.contains("dev_sync pull --continue"), "{message}");
        assert!(error_of("format = 1\n||||||| base\n").contains("line 2"));
    }

    #[test]
    fn empty_merge_input_is_an_empty_layout() -> anyhow::Result<()> {
        assert_eq!(parse_merge_input("")?, Layout::default());
        assert_eq!(parse_merge_input(" \n\t\n")?, Layout::default());
        assert_eq!(parse_merge_input(&render(&fixtures::layout("a=u1")?))?, fixtures::layout("a=u1")?);
        assert!(parse("").is_err());
        Ok(())
    }

    #[test]
    fn conflicted_render_has_markers_and_is_refused() -> anyhow::Result<()> {
        let merged = conflicted("", "a=u1 b=u3", "a=u2 b=u3")?;
        let text = render_conflicted(&merged);
        assert!(text.starts_with(HEADER));
        assert!(text.contains("\"b\" = { url = \"u3\" }\n"));
        assert!(text.contains(
            "# CONFLICT: \"a\": added locally as u1, incoming as u2\n\
             # Keep the lines you want, delete the rest and the marker lines, then run dev_sync pull --continue\n\
             <<<<<<< local\n\"a\" = { url = \"u1\" }\n=======\n\"a\" = { url = \"u2\" }\n>>>>>>> incoming\n"
        ));
        assert!(parse(&text).is_err());
        Ok(())
    }

    #[test]
    fn keeping_one_side_resolves_the_conflict() -> anyhow::Result<()> {
        let text = render_conflicted(&conflicted("", "a=u1 b=u3", "a=u2 b=u3")?);
        let keep_local = drop_between(&text, "=======", ">>>>>>>").replace("<<<<<<< local\n", "");
        assert_eq!(parse(&keep_local)?, fixtures::layout("a=u1 b=u3")?);
        let keep_incoming = drop_between(&text, "<<<<<<<", "=======").replace(">>>>>>> incoming\n", "");
        assert_eq!(parse(&keep_incoming)?, fixtures::layout("a=u2 b=u3")?);
        Ok(())
    }

    #[test]
    fn an_empty_side_renders_as_nothing_between_markers() -> anyhow::Result<()> {
        let text = render_conflicted(&conflicted("a=u1", "x=u1", "")?);
        assert!(text.contains("<<<<<<< local\n\"x\" = { url = \"u1\" }\n=======\n>>>>>>> incoming\n"), "{text}");
        Ok(())
    }

    /// Removes the lines from one starting with `start` to one starting with `end`, both included.
    fn drop_between(text: &str, start: &str, end: &str) -> String {
        text.lines()
            .scan(false, |dropping, line| {
                *dropping = *dropping || line.starts_with(start);
                let kept = (!*dropping).then_some(line);
                *dropping = *dropping && !line.starts_with(end);
                Some(kept)
            })
            .flatten()
            .map(|line| format!("{line}\n"))
            .collect()
    }
}
