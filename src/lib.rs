//! shikigami — Jujutsu (jj) の change graph を読み、その場で
//! new / edit / describe / squash / rebase / abandon / undo まで回す TUI。
//!
//! # 構成
//!
//! - [`jj`]: `jj` バイナリとの境界。template 出力のパースとコマンド組み立て。
//! - [`app`]: 状態機械。キー入力から jj 呼び出しまで。端末に依存しない。
//! - [`ui`]: ratatui の描画。状態を持たない。
//! - [`tui`]: raw mode / alternate screen とイベントループ。
//! - [`cli`]: clap の入口。
//!
//! この分割の目的は、端末を開かずに操作をテストできるようにすること。
//! `app` のテストは本物の jj repo を temp dir に作り、キー列を流して
//! repo の状態を検証している。

pub mod ai;
pub mod app;
pub mod cache;
pub mod cli;
pub mod decode;
pub mod diff_filter;
pub mod editor;
pub mod jj;
pub mod tui;
pub mod ui;
pub mod update;

pub use crate::cli::run;

#[cfg(test)]
mod testutil;
