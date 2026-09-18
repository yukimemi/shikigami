//! テスト用の本物の jj repo。
//!
//! shikigami の中身はほぼ全部「jj の出力をどう読むか」なので、jj を
//! mock すると意味のあるものが何も残らない。temp dir に本物の repo を
//! 作って、本物の `jj` を叩く。

use std::path::Path;
use std::process::Command;

use crate::app::App;
use crate::jj::Jj;

/// `jj` を repo 内の設定だけで動かす。実行環境の
/// `~/.config/jj/config.toml` (revsets.log や template の上書き、
/// ui.editor) に結果を左右されないようにする。
pub fn jj_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.current_dir(dir)
        .env("JJ_CONFIG", dir.join("config.toml"));
    cmd
}

pub fn run(dir: &Path, args: &[&str]) {
    let out = jj_cmd(dir).args(args).output().expect("failed to spawn jj");
    assert!(
        out.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `first change` → `second change` → `third change`(@) の 3 世代 repo。
///
/// 返す [`App`] は revset `all()` (root commit まで見える) で開いている。
/// 既定 revset だと `immutable_heads()` の設定次第で行数が変わり、
/// テストが実行環境に依存してしまう。
pub fn repo() -> (tempfile::TempDir, App) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("config.toml"),
        "[user]\nname = \"test\"\nemail = \"test@example.com\"\n",
    )
    .unwrap();
    run(dir, &["git", "init"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    run(dir, &["describe", "-m", "first change"]);
    run(dir, &["new", "-m", "second change"]);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    run(dir, &["new", "-m", "third change"]);

    // App 側は JJ_CONFIG を引き継がない (実運用と同じ素の env で jj を
    // 起動する) ので、user を repo config にも書いておく。
    run(dir, &["config", "set", "--repo", "user.name", "test"]);
    run(
        dir,
        &["config", "set", "--repo", "user.email", "test@example.com"],
    );

    let jj = Jj::discover(dir).unwrap();
    let app = App::new(jj, Some("all()".to_string())).unwrap();
    (tmp, app)
}
