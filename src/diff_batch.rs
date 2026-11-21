//! `SHIKIGAMI_DIFF_BATCH=delta-v1` のバッチ整形と厳格なファイル境界検証。
//!
//! 責務:
//! 1. 不変コミットの `jj diff --color never --git` 出力を、Git diff 構文と
//!    hunk 行数に従ってセクション分割し、C クォートを復号して `DiffFile` と
//!    一対一に照合する (整形前に実パスと変更種別で確定させる)。
//! 2. 入力本文と衝突しない合成 `commit` 境界行で各セクションを囲み、
//!    `delta --paging=never --side-by-side --width <W> --commit-regex ...
//!    --commit-style=raw --commit-decoration-style=none` をリポジトリ直下で 1 回起動する。
//! 3. 出力の全境界ペアの一意性・順序・区間外に非空白出力が無いことを検証し、
//!    曖昧または失敗ならエラーを返して呼び出し元を非同期のファイル単位経路へ
//!    フォールバックさせる。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use crate::jj::DiffFile;

/// バッチ整形を有効化する環境変数名。`delta-v1` のみ受け付ける。
pub const DIFF_BATCH_ENV: &str = "SHIKIGAMI_DIFF_BATCH";

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffBatchMode {
    DeltaV1,
}

impl DiffBatchMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "delta-v1" => Some(Self::DeltaV1),
            _ => None,
        }
    }

    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(DIFF_BATCH_ENV).ok()?;
        Self::parse(&raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaBatchConfig {
    pub mode: DiffBatchMode,
    pub delta_bin: PathBuf,
}

impl DeltaBatchConfig {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            mode: DiffBatchMode::from_env()?,
            delta_bin: PathBuf::from("delta"),
        })
    }

    pub fn delta_v1() -> Self {
        Self {
            mode: DiffBatchMode::DeltaV1,
            delta_bin: PathBuf::from("delta"),
        }
    }
}

/// `DiffFile` と 1 対 1 に対応づいた生 Git パッチ区間。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedSection<'a> {
    pub file_index: usize,
    pub raw: &'a str,
    /// `delta` 0.18 系では `old mode` / `new mode` を含み `@@` ハンクを持たない
    /// セクション直後の `commit` 行で内部状態がクリアされず、次セクション先頭へ
    /// 旧ファイルヘッダが漏れ出す。直後に独立したフェンス付きドレイン区間を
    /// 挟んで状態をリセットする必要があるかを示す。
    pub needs_drain: bool,
}

/// `delta` 1 回分の入力と、その出力検証に必要な境界情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramedBatch {
    pub input: String,
    pub commit_regex: String,
    pub token: String,
    pub drained: Vec<bool>,
}

/// 生の `jj diff --git` 出力を厳格に分割し、`files` と 1 対 1 照合する。
pub fn parse_and_match_git_patch<'a>(
    raw_patch: &'a str,
    files: &[DiffFile],
) -> Result<Vec<MatchedSection<'a>>> {
    if files.is_empty() {
        if raw_patch.trim().is_empty() {
            return Ok(Vec::new());
        }
        bail!("non-empty git patch for empty file list");
    }

    let raw_sections = split_git_sections(raw_patch)?;
    if raw_sections.len() != files.len() {
        bail!(
            "git patch section count ({}) does not match diff summary file count ({})",
            raw_sections.len(),
            files.len()
        );
    }

    let mut file_lookup: HashMap<(char, &str, &str), usize> = HashMap::with_capacity(files.len());
    for (idx, file) in files.iter().enumerate() {
        let key = (
            file.status,
            file.source_path.as_str(),
            file.destination_path.as_str(),
        );
        if file_lookup.insert(key, idx).is_some() {
            bail!("duplicate DiffFile identity in summary: {:?}", key);
        }
    }

    let mut matched = Vec::with_capacity(raw_sections.len());
    let mut used = vec![false; files.len()];

    for raw in raw_sections {
        let id = parse_section_identity(raw)?;
        let key = (
            id.status,
            id.source_path.as_str(),
            id.destination_path.as_str(),
        );
        let Some(&file_index) = file_lookup.get(&key) else {
            bail!(
                "git patch section {:?} did not match any DiffFile in summary",
                key
            );
        };
        if used[file_index] {
            bail!("duplicate git patch section for DiffFile {:?}", key);
        }
        used[file_index] = true;
        matched.push(MatchedSection {
            file_index,
            raw,
            needs_drain: id.needs_drain,
        });
    }

    if used.iter().any(|&seen| !seen) {
        bail!("some DiffFile entries had no matching git patch section");
    }

    Ok(matched)
}

/// 衝突しない合成 `commit` 境界行でセクション群をフレーミングする。
pub fn build_framed_batch(
    sections: &[MatchedSection<'_>],
    raw_patch: &str,
    commit_id: &str,
) -> FramedBatch {
    let prefix: String = commit_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let token = loop {
        let seq = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("SHIKIGAMI_BATCH_{prefix}_{seq}");
        if !raw_patch.contains(&candidate) {
            break candidate;
        }
    };

    let mut input = String::with_capacity(raw_patch.len() + sections.len() * 128);
    let mut drained = Vec::with_capacity(sections.len());
    let mut any_drain = false;

    for (i, section) in sections.iter().enumerate() {
        input.push_str("commit ");
        input.push_str(&token);
        input.push_str("_BEGIN_");
        input.push_str(&i.to_string());
        input.push('\n');

        input.push_str(section.raw);
        if !section.raw.ends_with('\n') {
            input.push('\n');
        }

        input.push_str("commit ");
        input.push_str(&token);
        input.push_str("_END_");
        input.push_str(&i.to_string());
        input.push('\n');

        let emit_drain = section.needs_drain && i + 1 < sections.len();
        drained.push(emit_drain);
        if emit_drain {
            any_drain = true;
            input.push_str("commit ");
            input.push_str(&token);
            input.push_str("_DRAIN_");
            input.push_str(&i.to_string());
            input.push('\n');
            input.push_str("diff --git a/. b/.\n--- a/.\n+++ b/.\n");
        }
    }

    let commit_regex = if any_drain {
        format!("^commit {token}_(?:BEGIN|END|DRAIN)_[0-9]+$")
    } else {
        format!("^commit {token}_(?:BEGIN|END)_[0-9]+$")
    };

    FramedBatch {
        input,
        commit_regex,
        token,
        drained,
    }
}

/// `delta` を 1 回だけ起動し、バッチ入力全体を整形する。
pub fn run_delta_batch(
    config: &DeltaBatchConfig,
    root: &Path,
    framed: &FramedBatch,
    width: u16,
) -> Result<String> {
    let width_str = width.to_string();
    let mut command = Command::new(&config.delta_bin);
    command
        .current_dir(root)
        .args([
            "--paging=never",
            "--side-by-side",
            "--width",
            &width_str,
            "--commit-regex",
            &framed.commit_regex,
            "--commit-style=raw",
            "--commit-decoration-style=none",
        ])
        .env("COLUMNS", &width_str);
    let out = crate::diff_filter::capture(&mut command, Some(&framed.input))
        .with_context(|| format!("failed to run `{}`", config.delta_bin.display()))?;

    if !out.status.success() {
        let stderr = crate::decode::decode(&out.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!(
                "`{}` failed with {}",
                config.delta_bin.display(),
                out.status
            );
        }
        bail!("{msg}");
    }

    Ok(crate::decode::decode(&out.stdout))
}

/// `delta` 出力から各セクションを切り出し、全境界ペアの一意性・順序・
/// 区間外出力の不在を厳格に検証する。
pub fn extract_and_validate_delta_output(
    output: &str,
    framed: &FramedBatch,
    count: usize,
) -> Result<Vec<String>> {
    if framed.drained.len() != count {
        bail!("framed batch metadata length mismatch");
    }
    if count == 0 {
        if output.trim().is_empty() {
            return Ok(Vec::new());
        }
        bail!("unexpected non-whitespace output for empty batch");
    }

    enum State {
        ExpectBegin(usize),
        InSection { index: usize, buf: String },
        ExpectDrain(usize),
        InDrain(usize),
        ExpectEnd,
    }

    let marker_prefix = format!("commit {}_", framed.token);
    let mut state = State::ExpectBegin(0);
    let mut results = Vec::with_capacity(count);

    for raw_line in output.split_inclusive('\n') {
        let stripped = raw_line
            .strip_suffix('\n')
            .unwrap_or(raw_line)
            .strip_suffix('\r')
            .unwrap_or_else(|| raw_line.strip_suffix('\n').unwrap_or(raw_line));

        if let Some(rest) = stripped.strip_prefix(&marker_prefix) {
            if let Some(idx_str) = rest.strip_prefix("BEGIN_") {
                let idx: usize = idx_str
                    .parse()
                    .with_context(|| format!("invalid BEGIN marker index: {stripped}"))?;
                match state {
                    State::ExpectBegin(expected) | State::InDrain(expected) if idx == expected => {
                        state = State::InSection {
                            index: idx,
                            buf: String::new(),
                        };
                    }
                    _ => bail!("unexpected BEGIN marker: {stripped}"),
                }
            } else if let Some(idx_str) = rest.strip_prefix("END_") {
                let idx: usize = idx_str
                    .parse()
                    .with_context(|| format!("invalid END marker index: {stripped}"))?;
                match std::mem::replace(&mut state, State::ExpectEnd) {
                    State::InSection { index, buf } if index == idx => {
                        results.push(buf);
                        if idx + 1 == count {
                            state = State::ExpectEnd;
                        } else if framed.drained[idx] {
                            state = State::ExpectDrain(idx);
                        } else {
                            state = State::ExpectBegin(idx + 1);
                        }
                    }
                    _ => bail!("unexpected END marker: {stripped}"),
                }
            } else if let Some(idx_str) = rest.strip_prefix("DRAIN_") {
                let idx: usize = idx_str
                    .parse()
                    .with_context(|| format!("invalid DRAIN marker index: {stripped}"))?;
                match state {
                    State::ExpectDrain(expected) if idx == expected => {
                        state = State::InDrain(idx + 1);
                    }
                    _ => bail!("unexpected DRAIN marker: {stripped}"),
                }
            } else {
                bail!("unknown batch marker line: {stripped}");
            }
            continue;
        }

        match &mut state {
            State::InSection { buf, .. } => {
                buf.push_str(raw_line);
            }
            State::InDrain(_) => {
                // Intentionally discarded: fenced drain section resets delta state.
            }
            State::ExpectBegin(_) | State::ExpectDrain(_) | State::ExpectEnd => {
                if !stripped.trim().is_empty() {
                    bail!("unexpected non-whitespace output outside batch markers: {stripped:?}");
                }
            }
        }
    }

    if !matches!(state, State::ExpectEnd) || results.len() != count {
        bail!(
            "incomplete batch output: extracted {} of {} sections",
            results.len(),
            count
        );
    }

    Ok(results)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionIdentity {
    status: char,
    source_path: String,
    destination_path: String,
    needs_drain: bool,
}

fn split_git_sections(raw_patch: &str) -> Result<Vec<&str>> {
    let mut starts = Vec::new();
    let mut old_rem: usize = 0;
    let mut new_rem: usize = 0;
    let mut just_ended_hunk = false;
    let mut offset = 0usize;

    for raw_line in raw_patch.split_inclusive('\n') {
        let line_start = offset;
        offset += raw_line.len();
        let line = trim_line_end(raw_line);

        if old_rem > 0 || new_rem > 0 {
            let Some(first) = line.as_bytes().first().copied() else {
                bail!("unexpected empty line inside git hunk");
            };
            match first {
                b' ' => {
                    if old_rem == 0 || new_rem == 0 {
                        bail!("hunk context line exceeded remaining counts");
                    }
                    old_rem -= 1;
                    new_rem -= 1;
                }
                b'-' => {
                    if old_rem == 0 {
                        bail!("hunk deletion line exceeded remaining old count");
                    }
                    old_rem -= 1;
                }
                b'+' => {
                    if new_rem == 0 {
                        bail!("hunk addition line exceeded remaining new count");
                    }
                    new_rem -= 1;
                }
                b'\\' => {}
                _ => bail!("unexpected line inside git hunk: {line:?}"),
            }
            if old_rem == 0 && new_rem == 0 {
                just_ended_hunk = true;
            }
            continue;
        }

        if line.starts_with("diff --git ") {
            starts.push(line_start);
            just_ended_hunk = false;
            continue;
        }

        if starts.is_empty() {
            if !line.trim().is_empty() {
                bail!("unexpected content before first `diff --git` header: {line:?}");
            }
            continue;
        }

        if line.starts_with("@@ ") {
            let Some((o, n)) = parse_git_hunk_header(line) else {
                bail!("malformed git hunk header: {line:?}");
            };
            old_rem = o;
            new_rem = n;
            just_ended_hunk = old_rem == 0 && new_rem == 0;
            continue;
        }

        if line.starts_with("\\ ") {
            if just_ended_hunk {
                continue;
            }
            bail!("unexpected no-newline marker outside hunk: {line:?}");
        }

        if line == "GIT binary patch" {
            bail!("GIT binary patch is not supported in batch mode");
        }

        if is_git_header_line(line) {
            just_ended_hunk = false;
            continue;
        }

        if line.is_empty() && offset == raw_patch.len() {
            continue;
        }

        bail!("unexpected line outside hunk in git diff: {line:?}");
    }

    if old_rem != 0 || new_rem != 0 {
        bail!("truncated git hunk at end of patch (remaining -{old_rem}/+{new_rem})");
    }

    let mut sections = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let start = starts[i];
        let end = starts.get(i + 1).copied().unwrap_or(raw_patch.len());
        sections.push(&raw_patch[start..end]);
    }
    Ok(sections)
}

fn trim_line_end(raw_line: &str) -> &str {
    let no_lf = raw_line.strip_suffix('\n').unwrap_or(raw_line);
    no_lf.strip_suffix('\r').unwrap_or(no_lf)
}

fn is_git_header_line(line: &str) -> bool {
    [
        "old mode ",
        "new mode ",
        "deleted file mode ",
        "new file mode ",
        "copy from ",
        "copy to ",
        "rename from ",
        "rename to ",
        "similarity index ",
        "dissimilarity index ",
        "index ",
        "--- ",
        "+++ ",
        "Binary files ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn parse_git_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_spec, rest) = rest.split_once(" +")?;
    let (new_spec, _) = rest.split_once(" @@")?;
    let old_count = parse_hunk_range_count(old_spec)?;
    let new_count = parse_hunk_range_count(new_spec)?;
    Some((old_count, new_count))
}

fn parse_hunk_range_count(spec: &str) -> Option<usize> {
    if let Some((start, count)) = spec.split_once(',') {
        let _: usize = start.parse().ok()?;
        count.parse().ok()
    } else {
        let _: usize = spec.parse().ok()?;
        Some(1)
    }
}

fn parse_section_identity(section: &str) -> Result<SectionIdentity> {
    let mut lines = section.split_inclusive('\n').map(trim_line_end);
    let first = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty git diff section"))?;
    let diff_git_rest = first
        .strip_prefix("diff --git ")
        .ok_or_else(|| anyhow::anyhow!("section does not start with `diff --git `: {first:?}"))?;

    let mut rename_from: Option<&str> = None;
    let mut rename_to: Option<&str> = None;
    let mut copy_from: Option<&str> = None;
    let mut copy_to: Option<&str> = None;
    let mut minus_path: Option<&str> = None;
    let mut plus_path: Option<&str> = None;
    let mut has_new_file_mode = false;
    let mut has_deleted_file_mode = false;
    let mut has_old_mode = false;
    let mut has_new_mode = false;
    let mut binary_line: Option<&str> = None;
    let mut has_hunk = false;

    for line in lines {
        if line.starts_with("@@ ") {
            has_hunk = true;
            break;
        }
        if let Some(p) = line.strip_prefix("rename from ") {
            rename_from = Some(p);
        } else if let Some(p) = line.strip_prefix("rename to ") {
            rename_to = Some(p);
        } else if let Some(p) = line.strip_prefix("copy from ") {
            copy_from = Some(p);
        } else if let Some(p) = line.strip_prefix("copy to ") {
            copy_to = Some(p);
        } else if let Some(p) = line.strip_prefix("--- ") {
            minus_path = Some(p);
        } else if let Some(p) = line.strip_prefix("+++ ") {
            plus_path = Some(p);
        } else if line.starts_with("new file mode ") {
            has_new_file_mode = true;
        } else if line.starts_with("deleted file mode ") {
            has_deleted_file_mode = true;
        } else if line.starts_with("old mode ") {
            has_old_mode = true;
        } else if line.starts_with("new mode ") {
            has_new_mode = true;
        } else if line.starts_with("Binary files ") {
            binary_line = Some(line);
        }
    }

    let (status, source_path, destination_path) = match (rename_from, rename_to, copy_from, copy_to)
    {
        (Some(from), Some(to), None, None) => ('R', decode_git_path(from)?, decode_git_path(to)?),
        (None, None, Some(from), Some(to)) => ('C', decode_git_path(from)?, decode_git_path(to)?),
        (None, None, None, None) => match (minus_path, plus_path) {
            (Some("/dev/null"), Some(plus)) if plus != "/dev/null" => {
                let p = decode_prefixed_path(plus, "b/")?;
                ('A', p.clone(), p)
            }
            (Some(minus), Some("/dev/null")) if minus != "/dev/null" => {
                let p = decode_prefixed_path(minus, "a/")?;
                ('D', p.clone(), p)
            }
            (Some(minus), Some(plus)) => {
                let old = decode_prefixed_path(minus, "a/")?;
                let new = decode_prefixed_path(plus, "b/")?;
                if old != new {
                    bail!("mismatched ---/+++ paths without rename/copy: {old:?} vs {new:?}");
                }
                ('M', old, new)
            }
            (None, None) => {
                let p = parse_same_path_diff_git(diff_git_rest)?;
                let status = if has_new_file_mode
                    || binary_line.is_some_and(|b| b.starts_with("Binary files /dev/null and "))
                {
                    'A'
                } else if has_deleted_file_mode
                    || binary_line.is_some_and(|b| b.ends_with(" and /dev/null differ"))
                {
                    'D'
                } else if (has_old_mode && has_new_mode) || binary_line.is_some() {
                    'M'
                } else {
                    bail!("cannot classify hunk-less diff section: {first:?}");
                };
                (status, p.clone(), p)
            }
            _ => bail!("incomplete ---/+++ header in section: {first:?}"),
        },
        _ => bail!("inconsistent rename/copy headers in section: {first:?}"),
    };

    if !diff_git_matches(diff_git_rest, &source_path, &destination_path)? {
        bail!(
            "`diff --git` header {diff_git_rest:?} disagrees with parsed paths ({source_path:?} -> {destination_path:?})"
        );
    }

    Ok(SectionIdentity {
        status,
        source_path,
        destination_path,
        needs_drain: has_old_mode && has_new_mode && !has_hunk,
    })
}

fn parse_same_path_diff_git(rest: &str) -> Result<String> {
    if rest.starts_with('"') {
        let (left, right) = split_quoted_first_pair(rest)?;
        let old = decode_prefixed_path(left, "a/")?;
        let new = decode_prefixed_path(right, "b/")?;
        if old != new {
            bail!("expected identical paths in hunk-less diff --git header: {old:?} vs {new:?}");
        }
        return Ok(old);
    }

    // Both tokens unquoted and representing the same path `a/<p> b/<p>`.
    // Length is 2 ("a/") + p.len() + 1 (" ") + 2 ("b/") + p.len() = 5 + 2 * p.len().
    if rest.len() < 5 || !(rest.len() - 5).is_multiple_of(2) {
        bail!("cannot split unquoted diff --git header: {rest:?}");
    }
    let path_len = (rest.len() - 5) / 2;
    let sep = 2 + path_len;
    if !rest.is_char_boundary(sep) || rest.as_bytes()[sep] != b' ' {
        bail!("cannot split unquoted diff --git header at midpoint: {rest:?}");
    }
    let left = &rest[..sep];
    let right = &rest[sep + 1..];
    let old = left
        .strip_prefix("a/")
        .ok_or_else(|| anyhow::anyhow!("missing `a/` prefix in diff --git header: {left:?}"))?;
    let new = right
        .strip_prefix("b/")
        .ok_or_else(|| anyhow::anyhow!("missing `b/` prefix in diff --git header: {right:?}"))?;
    if old != new {
        bail!("expected identical paths in hunk-less diff --git header: {old:?} vs {new:?}");
    }
    Ok(old.to_string())
}

fn diff_git_matches(rest: &str, source: &str, destination: &str) -> Result<bool> {
    if rest.starts_with('"') {
        let (left, right) = split_quoted_first_pair(rest)?;
        let old = decode_prefixed_path(left, "a/")?;
        let new = decode_prefixed_path(right, "b/")?;
        return Ok(old == source && new == destination);
    }
    let expected_left = format!("a/{source} ");
    let Some(right) = rest.strip_prefix(&expected_left) else {
        return Ok(false);
    };
    let new = decode_prefixed_path(right, "b/")?;
    Ok(new == destination)
}

fn split_quoted_first_pair(rest: &str) -> Result<(&str, &str)> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'"') {
        bail!("expected opening quote in {rest:?}");
    }
    let mut i = 1usize;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            let end_first = i + 1;
            if bytes.get(end_first) != Some(&b' ') {
                bail!("expected space after quoted first path in {rest:?}");
            }
            return Ok((&rest[..end_first], &rest[end_first + 1..]));
        }
        i += 1;
    }
    bail!("unterminated C-quoted path in {rest:?}")
}

fn decode_prefixed_path(token: &str, prefix: &str) -> Result<String> {
    let decoded = decode_git_path(token)?;
    let stripped = decoded
        .strip_prefix(prefix)
        .ok_or_else(|| anyhow::anyhow!("path {decoded:?} missing expected prefix {prefix:?}"))?;
    Ok(stripped.to_string())
}

/// Git の C クォートパス (`"..."`) を復号する。クォート無しならそのまま返す。
pub fn decode_git_path(token: &str) -> Result<String> {
    if !token.starts_with('"') {
        return Ok(token.to_string());
    }
    if token.len() < 2 || !token.ends_with('"') {
        bail!("malformed C-quoted git path: {token:?}");
    }
    let inner = &token.as_bytes()[1..token.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0usize;
    while i < inner.len() {
        let b = inner[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        if i >= inner.len() {
            bail!("dangling backslash in C-quoted git path: {token:?}");
        }
        match inner[i] {
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'"' => {
                out.push(b'"');
                i += 1;
            }
            b'a' => {
                out.push(0x07);
                i += 1;
            }
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            d0 @ b'0'..=b'7' => {
                if i + 2 >= inner.len() {
                    bail!("truncated octal escape in C-quoted git path: {token:?}");
                }
                let d1 = inner[i + 1];
                let d2 = inner[i + 2];
                if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                    bail!("invalid octal escape in C-quoted git path: {token:?}");
                }
                let val = ((d0 - b'0') as u16) * 64 + ((d1 - b'0') as u16) * 8 + (d2 - b'0') as u16;
                if val > 255 {
                    bail!("octal escape out of byte range in C-quoted git path: {token:?}");
                }
                out.push(val as u8);
                i += 3;
            }
            other => bail!(
                "unsupported escape \\{} in C-quoted git path: {token:?}",
                other as char
            ),
        }
    }
    String::from_utf8(out).with_context(|| format!("invalid UTF-8 in C-quoted git path: {token:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(status: char, path: &str) -> DiffFile {
        DiffFile {
            status,
            path: path.to_string(),
            source_path: path.to_string(),
            destination_path: path.to_string(),
        }
    }

    fn renamed(status: char, from: &str, to: &str) -> DiffFile {
        DiffFile {
            status,
            path: format!("{{{from} => {to}}}"),
            source_path: from.to_string(),
            destination_path: to.to_string(),
        }
    }

    #[test]
    fn decodes_git_c_quoted_paths_including_octal_utf8_and_escapes() {
        assert_eq!(decode_git_path("plain/path.rs").unwrap(), "plain/path.rs");
        assert_eq!(
            decode_git_path(r#""a/quote\"and\\slash\t.txt""#).unwrap(),
            "a/quote\"and\\slash\t.txt"
        );
        // UTF-8 bytes for "日本" = E6 97 A5 E6 9C AC = \346\227\245\346\234\254
        assert_eq!(
            decode_git_path(r#""a/\346\227\245\346\234\254 file.txt""#).unwrap(),
            "a/日本 file.txt"
        );
        assert!(decode_git_path(r#""unterminated"#).is_err());
        assert!(decode_git_path(r#""\777""#).is_err());
    }

    #[test]
    fn parses_and_matches_diverse_git_sections_including_hunk_body_lookalikes() {
        let patch = concat!(
            "diff --git a/lookalike.txt b/lookalike.txt\n",
            "index 1111111..2222222 100644\n",
            "--- a/lookalike.txt\n",
            "+++ b/lookalike.txt\n",
            "@@ -1,2 +1,3 @@\n",
            " context\n",
            "+diff --git a/fake.txt b/fake.txt\n",
            " tail\n",
            "diff --git a/old space.txt b/new space.txt\n",
            "rename from old space.txt\n",
            "rename to new space.txt\n",
            "diff --git a/a b/c.txt b/a b/c.txt\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "diff --git \"a/quote\\\"and\\\\.txt\" \"b/quote\\\"and\\\\.txt\"\n",
            "new file mode 100644\n",
            "index 0000000..1111111\n",
            "--- /dev/null\n",
            "+++ \"b/quote\\\"and\\\\.txt\"\n",
            "@@ -0,0 +1 @@\n",
            "+hi\n",
            "diff --git \"a/\\346\\227\\245\\346\\234\\254.txt\" \"b/\\346\\227\\245\\346\\234\\254.txt\"\n",
            "new file mode 100644\n",
            "index 0000000..e69de29\n",
            "diff --git a/bin.dat b/bin.dat\n",
            "new file mode 100644\n",
            "index 0000000..3333333\n",
            "Binary files /dev/null and b/bin.dat differ\n",
            "diff --git a/deleted.txt b/deleted.txt\n",
            "deleted file mode 100644\n",
            "index 4444444..0000000\n",
            "--- a/deleted.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-bye\n",
            "\\ No newline at end of file\n",
        );
        let files = vec![
            file('D', "deleted.txt"),
            file('A', "bin.dat"),
            file('A', "日本.txt"),
            file('A', "quote\"and\\.txt"),
            file('M', "a b/c.txt"),
            renamed('R', "old space.txt", "new space.txt"),
            file('M', "lookalike.txt"),
        ];

        let matched = parse_and_match_git_patch(patch, &files).unwrap();
        assert_eq!(matched.len(), files.len());
        // Order follows patch order, each mapped to exact DiffFile index.
        assert_eq!(matched[0].file_index, 6); // lookalike.txt
        assert!(!matched[0].needs_drain);
        assert_eq!(matched[1].file_index, 5); // rename
        assert_eq!(matched[2].file_index, 4); // mode-only "a b/c.txt"
        assert!(matched[2].needs_drain);
        assert_eq!(matched[3].file_index, 3); // quoted
        assert_eq!(matched[4].file_index, 2); // octal UTF-8 empty new file
        assert_eq!(matched[5].file_index, 1); // binary
        assert_eq!(matched[6].file_index, 0); // deleted with no-newline marker
    }

    #[test]
    fn rejects_mismatched_or_ambiguous_git_patches() {
        let patch = concat!(
            "diff --git a/a.txt b/a.txt\n",
            "new file mode 100644\n",
            "index 0000000..e69de29\n",
        );
        // Wrong status ('M' instead of 'A') must not match.
        assert!(parse_and_match_git_patch(patch, &[file('M', "a.txt")]).is_err());
        // Extra file in summary must not match.
        assert!(
            parse_and_match_git_patch(patch, &[file('A', "a.txt"), file('A', "b.txt")]).is_err()
        );
        // Wrong path must not match.
        assert!(parse_and_match_git_patch(patch, &[file('A', "other.txt")]).is_err());
    }

    #[test]
    fn validates_delta_output_markers_strictly() {
        let files = vec![file('M', "a.txt"), file('A', "b.txt")];
        let patch = concat!(
            "diff --git a/a.txt b/a.txt\nold mode 100644\nnew mode 100755\n",
            "diff --git a/b.txt b/b.txt\nnew file mode 100644\nindex 0000000..e69de29\n",
        );
        let matched = parse_and_match_git_patch(patch, &files).unwrap();
        let framed = build_framed_batch(&matched, patch, "abcdef123456");
        assert!(framed.drained[0]);
        assert!(!framed.drained[1]);

        let t = &framed.token;
        let valid = format!(
            "commit {t}_BEGIN_0\nSEC0\ncommit {t}_END_0\ncommit {t}_DRAIN_0\nDRIP\ncommit {t}_BEGIN_1\nSEC1\ncommit {t}_END_1\n"
        );
        let out = extract_and_validate_delta_output(&valid, &framed, 2).unwrap();
        assert_eq!(out, vec!["SEC0\n", "SEC1\n"]);

        // Non-whitespace outside markers is rejected.
        let stray = format!("STRAY\n{valid}");
        assert!(extract_and_validate_delta_output(&stray, &framed, 2).is_err());

        // Missing END marker is rejected.
        let truncated = format!("commit {t}_BEGIN_0\nSEC0\n");
        assert!(extract_and_validate_delta_output(&truncated, &framed, 2).is_err());

        // Out-of-order / duplicate BEGIN marker is rejected.
        let dup = format!(
            "commit {t}_BEGIN_0\ncommit {t}_BEGIN_0\nSEC0\ncommit {t}_END_0\ncommit {t}_DRAIN_0\ncommit {t}_BEGIN_1\nSEC1\ncommit {t}_END_1\n"
        );
        assert!(extract_and_validate_delta_output(&dup, &framed, 2).is_err());
    }

    #[test]
    fn real_delta_batch_round_trips_edge_cases_without_bleed() {
        let ok = Command::new("delta")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: `delta` is not on PATH");
            return;
        }
        let patch = concat!(
            "diff --git a/mode_only.txt b/mode_only.txt\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "diff --git a/old space.txt b/new space.txt\n",
            "rename from old space.txt\n",
            "rename to new space.txt\n",
            "diff --git a/bin.dat b/bin.dat\n",
            "new file mode 100644\n",
            "index 0000000000..1111111111\n",
            "Binary files /dev/null and b/bin.dat differ\n",
            "diff --git a/日本語.txt b/日本語.txt\n",
            "new file mode 100644\n",
            "index 0000000000..7898192261\n",
            "--- /dev/null\n",
            "+++ b/日本語.txt\n",
            "@@ -0,0 +1,2 @@\n",
            "+こんにちは\n",
            "+世界\n",
            "diff --git a/split.txt b/split.txt\n",
            "--- a/split.txt\n",
            "+++ b/split.txt\n",
            "@@ -1 +1 @@\n",
            "-old_left\n",
            "+new_right\n",
        );
        let files = vec![
            file('M', "mode_only.txt"),
            renamed('R', "old space.txt", "new space.txt"),
            file('A', "bin.dat"),
            file('A', "日本語.txt"),
            file('M', "split.txt"),
        ];
        let matched = parse_and_match_git_patch(patch, &files).unwrap();
        let framed = build_framed_batch(&matched, patch, "12345678");
        let root = tempfile::tempdir().unwrap();
        let raw_out =
            run_delta_batch(&DeltaBatchConfig::delta_v1(), root.path(), &framed, 90).unwrap();
        let sections = extract_and_validate_delta_output(&raw_out, &framed, files.len()).unwrap();
        assert_eq!(sections.len(), 5);
        assert!(sections[0].contains("mode_only.txt"), "{}", sections[0]);
        assert!(
            !sections[1].contains("mode_only.txt"),
            "bleed into rename: {}",
            sections[1]
        );
        assert!(sections[1].contains("old space.txt"), "{}", sections[1]);
        assert!(sections[1].contains("new space.txt"), "{}", sections[1]);
        assert!(sections[2].contains("bin.dat"), "{}", sections[2]);
        assert!(sections[3].contains("日本語.txt"), "{}", sections[3]);
        assert!(sections[3].contains("こんにちは"), "{}", sections[3]);
        use ansi_to_tui::IntoText;
        let text = sections[4].as_str().into_text().unwrap();
        let mut left = None;
        let mut right = None;
        for line in &text.lines {
            let plain: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            left = left.or_else(|| plain.find("old_left"));
            right = right.or_else(|| plain.find("new_right"));
        }
        assert!(left.is_some_and(|column| column < 45), "{text:?}");
        assert!(right.is_some_and(|column| column >= 45), "{text:?}");
    }
}
