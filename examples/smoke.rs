//! `examples/smoke.rs` — release-time smoke target.
//!
//! `release.yml` runs `cargo run --release --target <T> --example smoke`
//! for every matrix entry. shikigami の場合、lib テストが通っていても
//! release バイナリが死ぬ経路は「jj を起動して template 出力を読む」
//! ところに集中している (template 文法の非互換、graph マーカーの扱い、
//! Windows の改行/コードページ)。そこを実際に 1 回通す。
//!
//! `jj` が無い runner では skip して exit 0 する。ここで落として
//! release を止める価値はない (jj の有無は shikigami のバグではない)。

use std::path::Path;
use std::process::Command;

fn jj(dir: &Path, args: &[&str]) -> bool {
    Command::new("jj")
        .current_dir(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn main() -> anyhow::Result<()> {
    if Command::new("jj").arg("--version").output().is_err() {
        eprintln!("smoke: `jj` not on PATH; skipping (exit 0)");
        return Ok(());
    }
    let tmp = std::env::temp_dir().join(format!("shikigami-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let _guard = scopeguard(&tmp);

    // repo 内に user を書く: runner の global jj config は空なので、
    // これが無いと describe が "author not configured" で落ちる。
    assert!(jj(&tmp, &["git", "init"]), "jj git init failed");
    assert!(jj(&tmp, &["config", "set", "--repo", "user.name", "smoke"]));
    assert!(jj(
        &tmp,
        &["config", "set", "--repo", "user.email", "smoke@example.com"]
    ));
    std::fs::write(tmp.join("a.txt"), "a\n")?;
    assert!(jj(&tmp, &["describe", "-m", "smoke change"]));

    let repo = shikigami::jj::Jj::discover(&tmp)?;
    let rows = repo.log(Some("all()"))?;
    let descs: Vec<String> = rows
        .iter()
        .filter_map(|row| match row {
            shikigami::jj::Row::Change { change, .. } => Some(change.description.clone()),
            shikigami::jj::Row::Connector(_) => None,
        })
        .collect();
    assert!(
        descs.iter().any(|d| d == "smoke change"),
        "jj log did not round-trip through the template parser: {descs:?}"
    );
    // diff / show も ANSI 付きで読めることまで見る (ansi-to-tui の
    // feature 抜けはここでしか出ない)。
    let head = rows
        .iter()
        .find_map(|row| match row {
            shikigami::jj::Row::Change { change, .. } if change.is_working_copy => {
                Some(change.id.clone())
            }
            _ => None,
        })
        .expect("no working-copy change in the log");
    let diff = repo.diff(&head, None)?;
    assert!(
        diff.contains("a.txt"),
        "diff missing the changed file: {diff}"
    );

    eprintln!("smoke: ok ({} rows, diff {} bytes)", rows.len(), diff.len());
    Ok(())
}

/// temp repo の後始末。panic しても消えるように Drop で消す。
fn scopeguard(path: &Path) -> impl Drop {
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    Cleanup(path.to_path_buf())
}
