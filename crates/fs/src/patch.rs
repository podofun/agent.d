//! Unified diffs: create, parse, format and apply, for text files.
//!
//! `diffy` creates diffs. Parsing and applying are local so a failed hunk can
//! say which line did not match, CRLF files keep their line endings, and a
//! patch may touch several files.

use serde::{Deserialize, Serialize};

const NO_NEWLINE: &str = "\\ No newline at end of file";

/// One file's part of a unified diff. A path of `None` is `/dev/null`: the
/// file is created (`old_path`) or removed (`new_path`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilePatch {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// One `@@` block. Each line keeps its leading `' '`, `'-'` or `'+'`, and a
/// `\ No newline at end of file` marker stays as its own line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PatchError {
    #[error("the patch could not be read at line {line}, {reason}")]
    Parse { line: usize, reason: String },
    #[error("hunk {hunk} of `{path}` does not apply, {reason}")]
    Hunk {
        path: String,
        hunk: usize,
        reason: String,
    },
}

impl FilePatch {
    /// The path this patch is about, for messages: the new path, or the old
    /// one when the file is removed.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }
}

/// A unified diff from `old` to `new`, with `a/` and `b/` path prefixes.
pub fn create_diff(old: &str, new: &str, old_path: &str, new_path: &str, context: usize) -> String {
    let mut options = diffy::DiffOptions::new();
    options
        .set_context_len(context)
        .set_original_filename(format!("a/{old_path}"))
        .set_modified_filename(format!("b/{new_path}"));
    options.create_patch(old, new).to_string()
}

/// Every file patch in `text`, in order.
pub fn parse_patch(text: &str) -> Result<Vec<FilePatch>, PatchError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut patches = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        if !line.starts_with("--- ") {
            at += 1;
            continue;
        }
        let parse_error = |line: usize, reason: &str| PatchError::Parse {
            line: line + 1,
            reason: reason.to_owned(),
        };
        let old_path = header_path(&line[4..]);
        let Some(next) = lines.get(at + 1).filter(|next| next.starts_with("+++ ")) else {
            return Err(parse_error(
                at + 1,
                "a `---` line must be followed by a `+++` line",
            ));
        };
        let new_path = header_path(&next[4..]);
        at += 2;
        let mut hunks = Vec::new();
        while at < lines.len() && lines[at].starts_with("@@") {
            let (old_start, old_lines, new_start, new_lines) = hunk_header(lines[at])
                .ok_or_else(|| parse_error(at, "the `@@` header is malformed"))?;
            at += 1;
            let mut hunk = Hunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            };
            let (mut old_seen, mut new_seen) = (0, 0);
            while at < lines.len()
                && (old_seen < old_lines || new_seen < new_lines || lines[at] == NO_NEWLINE)
            {
                let line = lines[at];
                match line.chars().next() {
                    Some(' ') => {
                        old_seen += 1;
                        new_seen += 1;
                    }
                    // Some tools drop the space on empty context lines.
                    None => {
                        old_seen += 1;
                        new_seen += 1;
                        hunk.lines.push(" ".to_owned());
                        at += 1;
                        continue;
                    }
                    Some('-') => old_seen += 1,
                    Some('+') => new_seen += 1,
                    Some('\\') => {}
                    _ => {
                        return Err(parse_error(
                            at,
                            "a hunk line must start with a space, `-` or `+`",
                        ));
                    }
                }
                hunk.lines.push(line.to_owned());
                at += 1;
            }
            if old_seen != old_lines || new_seen != new_lines {
                return Err(parse_error(
                    at.min(lines.len()),
                    "a hunk is shorter than its `@@` header says",
                ));
            }
            hunks.push(hunk);
        }
        if hunks.is_empty() {
            return Err(parse_error(at, "a file in the patch has no `@@` hunks"));
        }
        patches.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }
    if patches.is_empty() {
        return Err(PatchError::Parse {
            line: 1,
            reason: "there is no `---`/`+++` file header in it".to_owned(),
        });
    }
    Ok(patches)
}

fn header_path(raw: &str) -> Option<String> {
    let path = raw.split('\t').next().unwrap_or(raw).trim_end();
    if path == "/dev/null" {
        return None;
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    Some(path.to_owned())
}

fn hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let inner = line.strip_prefix("@@ ")?;
    let end = inner.find(" @@")?;
    let mut parts = inner[..end].split(' ');
    let range = |part: Option<&str>, sign: char| -> Option<(usize, usize)> {
        let part = part?.strip_prefix(sign)?;
        match part.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((part.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(parts.next(), '-')?;
    let (new_start, new_lines) = range(parts.next(), '+')?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// The patches as unified diff text, with `a/` and `b/` path prefixes.
pub fn format_patch(files: &[FilePatch]) -> String {
    let mut out = String::new();
    for file in files {
        let path = |path: &Option<String>, prefix: &str| match path {
            Some(path) => format!("{prefix}{path}"),
            None => "/dev/null".to_owned(),
        };
        out.push_str(&format!(
            "--- {}\n+++ {}\n",
            path(&file.old_path, "a/"),
            path(&file.new_path, "b/")
        ));
        for hunk in &file.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
            ));
            for line in &hunk.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// A line of a file: its text and whether a line ending follows it.
struct Line {
    text: String,
    ends: bool,
}

/// Apply one file's patch to `text`. Each hunk must match exactly, either
/// where its header says or, when the file has shifted, at the one place its
/// old lines appear.
pub fn apply_patch(text: &str, patch: &FilePatch) -> Result<String, PatchError> {
    let crlf = text.contains("\r\n");
    let mut lines: Vec<Line> = split_lines(text);
    let mut shift: isize = 0;
    for (number, hunk) in patch.hunks.iter().enumerate() {
        let fail = |reason: String| PatchError::Hunk {
            path: patch.path().to_owned(),
            hunk: number + 1,
            reason,
        };
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|line| line.starts_with(' ') || line.starts_with('-'))
            .map(|line| &line[1..])
            .collect();
        let expected = if hunk.old_lines == 0 {
            hunk.old_start as isize + shift
        } else {
            hunk.old_start as isize - 1 + shift
        };
        let expected = expected.max(0) as usize;
        let matches_at = |at: usize| {
            at + old.len() <= lines.len()
                && old
                    .iter()
                    .zip(&lines[at..])
                    .all(|(want, have)| *want == have.text)
        };
        let at = if matches_at(expected) {
            expected
        } else {
            let found: Vec<usize> = (0..=lines.len().saturating_sub(old.len()))
                .filter(|at| matches_at(*at))
                .collect();
            match found.as_slice() {
                [only] if !old.is_empty() => *only,
                [] | [_] => return Err(fail(mismatch(&lines, &old, expected))),
                _ => {
                    return Err(fail(format!(
                        "its lines appear {} times and not where the header says",
                        found.len()
                    )));
                }
            }
        };
        let mut replacement = Vec::new();
        let mut original = lines[at..at + old.len()].iter();
        for (index, line) in hunk.lines.iter().enumerate() {
            let no_newline = hunk
                .lines
                .get(index + 1)
                .is_some_and(|next| next == NO_NEWLINE);
            match line.chars().next() {
                Some(' ') => {
                    let kept = original.next().expect("matched above");
                    replacement.push(Line {
                        text: kept.text.clone(),
                        ends: kept.ends && !no_newline,
                    });
                }
                Some('-') => {
                    original.next();
                }
                Some('+') => replacement.push(Line {
                    text: line[1..].to_owned(),
                    ends: !no_newline,
                }),
                _ => {}
            }
        }
        let removed = old.len();
        let added = replacement.len();
        lines.splice(at..at + removed, replacement);
        shift += added as isize - removed as isize;
    }
    let ending = if crlf { "\r\n" } else { "\n" };
    let mut out = String::new();
    for line in &lines {
        out.push_str(&line.text);
        if line.ends {
            out.push_str(ending);
        }
    }
    Ok(out)
}

fn split_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(end) => {
                let text = rest[..end].strip_suffix('\r').unwrap_or(&rest[..end]);
                lines.push(Line {
                    text: text.to_owned(),
                    ends: true,
                });
                rest = &rest[end + 1..];
            }
            None => {
                lines.push(Line {
                    text: rest.to_owned(),
                    ends: false,
                });
                rest = "";
            }
        }
    }
    lines
}

/// Why the old lines do not match at `at`, naming the first differing line.
fn mismatch(lines: &[Line], old: &[&str], at: usize) -> String {
    for (offset, want) in old.iter().enumerate() {
        match lines.get(at + offset) {
            Some(have) if have.text == *want => continue,
            Some(have) => {
                return format!(
                    "line {} reads `{}` but the patch expects `{want}`",
                    at + offset + 1,
                    have.text
                );
            }
            None => {
                return format!(
                    "the file ends before line {}, which the patch expects to read `{want}`",
                    at + offset + 1
                );
            }
        }
    }
    "its lines do not match the file".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_apply_round_trip() {
        let (old, new) = ("a\nb\nc\n", "a\nB\nc\nd\n");
        let diff = create_diff(old, new, "x", "x", 3);
        let patches = parse_patch(&diff).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(apply_patch(old, &patches[0]).unwrap(), new);
    }

    #[test]
    fn multi_file_patches_split() {
        let text = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-c\n+d\n";
        let patches = parse_patch(text).unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].new_path.as_deref(), Some("src/a.rs"));
        assert_eq!(patches[1].old_path.as_deref(), Some("src/b.rs"));
    }

    #[test]
    fn created_and_removed_files_use_dev_null() {
        let created = parse_patch("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n").unwrap();
        assert_eq!(created[0].old_path, None);
        assert_eq!(apply_patch("", &created[0]).unwrap(), "hi\n");
        let removed = parse_patch("--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n").unwrap();
        assert_eq!(removed[0].new_path, None);
        assert_eq!(apply_patch("bye\n", &removed[0]).unwrap(), "");
    }

    #[test]
    fn format_round_trips() {
        let text = create_diff("a\nb\nc\n", "a\nc\nd\n", "f.txt", "f.txt", 1);
        let parsed = parse_patch(&text).unwrap();
        assert_eq!(parse_patch(&format_patch(&parsed)).unwrap(), parsed);
    }

    #[test]
    fn missing_final_newline_is_kept() {
        let diff = create_diff("a", "b", "x", "x", 3);
        assert!(diff.contains("\\ No newline at end of file"), "{diff}");
        let patches = parse_patch(&diff).unwrap();
        assert_eq!(apply_patch("a", &patches[0]).unwrap(), "b");
    }

    #[test]
    fn crlf_lines_apply() {
        let patch = parse_patch("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n").unwrap();
        assert_eq!(apply_patch("a\r\nb\r\n", &patch[0]).unwrap(), "a\r\nc\r\n");
    }

    #[test]
    fn hunks_apply_at_an_offset() {
        let patch = parse_patch("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n").unwrap();
        assert_eq!(
            apply_patch("top\na\nb\n", &patch[0]).unwrap(),
            "top\na\nB\n"
        );
    }

    #[test]
    fn failed_hunks_name_the_file_and_hunk() {
        let patch =
            parse_patch("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n").unwrap();
        let error = apply_patch("a\nq\n", &patch[0]).unwrap_err();
        assert!(
            matches!(error, PatchError::Hunk { hunk: 1, .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("hunk 1 of `src/a.rs` does not apply"),
            "{message}"
        );
        assert!(message.contains("line 2 reads `q`"), "{message}");
    }

    #[test]
    fn malformed_patches_say_where() {
        let error = parse_patch("--- a/x\n+++ b/x\n@@ nonsense @@\n").unwrap_err();
        assert!(
            matches!(error, PatchError::Parse { line: 3, .. }),
            "{error:?}"
        );
        assert!(parse_patch("just text\n").is_err());
    }
}
