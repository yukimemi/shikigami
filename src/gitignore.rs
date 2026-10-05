//! リポジトリ直下の `.gitignore` への追記。
//!
//! `jj file untrack` は ignore 済みでないパスを拒否するので、untrack の前に
//! 対象を `.gitignore` に入れておく必要がある。ここは純関数
//! ([`plan_append`]) と薄い I/O ([`ignore_paths`]) に分けてあり、
//! 追記ロジックは jj 無しでテストできる。

use std::path::Path;

use anyhow::{Context, Result, bail};

/// repo ルート固定の ignore 行 (`/path`) にする。`\` は `/` に正規化し、
/// 行頭の `#`/`!` と glob 文字は `\` でエスケープする。
fn entry_line(path: &str) -> Result<String> {
    if path.contains('\n') || path.contains('\r') {
        bail!("path contains a line break and cannot be written to .gitignore: {path:?}");
    }
    let norm = path.replace('\\', "/");
    let norm = norm.trim_start_matches('/');
    if norm.is_empty() {
        bail!("empty path");
    }
    let mut out = String::from("/");
    for (i, c) in norm.chars().enumerate() {
        if matches!(c, '*' | '?' | '[') || (i == 0 && matches!(c, '#' | '!')) {
            out.push('\\');
        }
        out.push(c);
    }
    Ok(out)
}

/// 既存行を比較用に正規化する (前後空白と先頭・末尾の `/` を除く)。
fn normalize(line: &str) -> &str {
    line.trim().trim_start_matches('/').trim_end_matches('/')
}

/// `existing` に `paths` を追記した新しい内容を返す。変更不要なら `None`。
/// 既に等価な行があるパスは足さない。常に末尾改行で終える。
pub fn plan_append(existing: Option<&str>, paths: &[String]) -> Result<Option<String>> {
    let existing = existing.unwrap_or("");
    let mut lines: Vec<String> = existing.lines().map(|l| normalize(l).to_string()).collect();
    let mut added: Vec<String> = Vec::new();
    for p in paths {
        let entry = entry_line(p)?;
        let key = normalize(&entry).to_string();
        if lines.contains(&key) {
            continue;
        }
        added.push(entry);
        lines.push(key);
    }
    if added.is_empty() {
        return Ok(None);
    }
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in added {
        out.push_str(&line);
        out.push('\n');
    }
    Ok(Some(out))
}

/// `root/.gitignore` に `paths` を追記する (無ければ作る)。実際に
/// 追加した行数を返す。
pub fn ignore_paths(root: &Path, paths: &[String]) -> Result<usize> {
    let file = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&file) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let Some(new) = plan_append(existing.as_deref(), paths)? else {
        return Ok(0);
    };
    let added = new.lines().count() - existing.as_deref().map_or(0, |s| s.lines().count());
    std::fs::write(&file, new).with_context(|| format!("write {}", file.display()))?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn appends_to_existing_file() {
        let out = plan_append(Some("target\n"), &v(&["a/b.rs"])).unwrap();
        assert_eq!(out.as_deref(), Some("target\n/a/b.rs\n"));
    }

    #[test]
    fn creates_content_when_missing() {
        let out = plan_append(None, &v(&["x", "y"])).unwrap();
        assert_eq!(out.as_deref(), Some("/x\n/y\n"));
    }

    #[test]
    fn adds_newline_when_file_lacks_one() {
        let out = plan_append(Some("target"), &v(&["x"])).unwrap();
        assert_eq!(out.as_deref(), Some("target\n/x\n"));
    }

    #[test]
    fn dedupes_with_or_without_slashes() {
        assert_eq!(plan_append(Some("/x\n"), &v(&["x"])).unwrap(), None);
        assert_eq!(plan_append(Some("x\n"), &v(&["x"])).unwrap(), None);
        assert_eq!(plan_append(Some("x/\n"), &v(&["x"])).unwrap(), None);
        assert_eq!(plan_append(Some("  /x  \n"), &v(&["x"])).unwrap(), None);
        let out = plan_append(Some("/x\n"), &v(&["x", "z", "z"])).unwrap();
        assert_eq!(out.as_deref(), Some("/x\n/z\n"));
    }

    #[test]
    fn escapes_special_characters_and_normalizes_separators() {
        let out = plan_append(None, &v(&["#a", "!b", "c*d", "e\\f.rs"])).unwrap();
        assert_eq!(out.as_deref(), Some("/\\#a\n/\\!b\n/c\\*d\n/e/f.rs\n"));
    }

    #[test]
    fn rejects_line_breaks() {
        assert!(plan_append(None, &v(&["a\nb"])).is_err());
    }

    #[test]
    fn ignore_paths_creates_and_dedupes_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(ignore_paths(tmp.path(), &v(&["a.txt"])).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "/a.txt\n"
        );
        assert_eq!(ignore_paths(tmp.path(), &v(&["a.txt"])).unwrap(), 0);
        std::fs::write(tmp.path().join(".gitignore"), "keep").unwrap();
        assert_eq!(ignore_paths(tmp.path(), &v(&["b"])).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "keep\n/b\n"
        );
    }
}
