//! `shikigami self-update` — kaishin への薄いラッパー。
//!
//! TUI 本体 (`cli::launch`) は同期 `std::process::Command` だけで足りる設計
//! (`Cargo.toml` 冒頭のコメント参照) で、tokio ランタイムは持たない。この
//! サブコマンドを叩いたときだけ、その場で current_thread ランタイムを組み
//! 立てて `kaishin::run_self_update` を `block_on` し、戻り値と一緒に畳む。
//!
//! renri の `Checker` のようなバックグラウンド自動更新チェックや起動時
//! banner は意図的に持ち込まない: shikigami は alternate screen 上の TUI
//! なので、更新通知を印字できる隙間がない。更新の可否は `--check`、実行は
//! 素の呼び出し (確認プロンプト付き) か `--yes` で明示する。

use anyhow::Result;
use kaishin::{KaishinOptions, UpdateOptions};

/// GitHub リポジトリの owner。crates.io のパッケージ名・バイナリ名は
/// どちらも `shikigami` (`Cargo.toml` の `[package] name`) と一致するので
/// `KaishinOptions::crate_name` は不要。
const OWNER: &str = "yukimemi";

/// `shikigami self-update [--yes] [--check]` の実装。
pub fn run(yes: bool, check: bool) -> Result<()> {
    let opts = KaishinOptions::new(
        OWNER,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
    let upd_opts = UpdateOptions::new().yes(yes).check_only(check);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(kaishin::run_self_update(&opts, upd_opts))
}
