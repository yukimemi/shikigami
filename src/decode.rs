//! 子プロセスの stdout/stderr バイト列を文字列にする。
//!
//! `ai.rs`/`diff_filter.rs` は Windows では `cmd /C` 越しにユーザコマンド
//! を呼ぶ (`shell_command` 参照)。実際のツール出力 (opencode/claude/delta
//! など、今日日の CLI) はほぼ例外なく UTF-8 だが、`cmd /C` がプログラム
//! を見つけられなかった場合などは **cmd.exe 自身がローカライズ済みの
//! 診断メッセージ** (例: 日本語 Windows の
//! 「'foo' は、内部コマンドまたは外部コマンドとして認識されていません。」)
//! を出す。これは UTF-8 ではなく、システムの OEM コードページ (日本語
//! Windows なら CP932) で書かれるため、常に UTF-8 として読むと文字化け
//! する — 特に `SHIKIGAMI_AI_CMD` が指すプログラムが `PATH` に無いとき
//! (typo やインストール漏れ) に踏みやすい、最も実害の大きい経路。
//!
//! 有効な UTF-8 ならそのまま使い、無効なときだけ Windows では OEM
//! コードページとして読み直す。実ツールの正常な UTF-8 出力を誤って化け
//! させないための順序 (先に UTF-8 として通れば、それ以上は何もしない)。

#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};

    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: `bytes`/`bytes.len()` describe a valid, in-bounds slice for
    // the whole call; `MultiByteToWideChar` only reads from it and never
    // writes through `bytes.as_ptr()`. The first call (null output
    // buffer, requested length 0) just asks for the required wide-char
    // count; the second call writes into `wide`, which is sized from
    // that answer.
    unsafe {
        let wide_len = MultiByteToWideChar(
            CP_OEMCP,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if wide_len <= 0 {
            return None;
        }
        let mut wide = vec![0u16; wide_len as usize];
        let written = MultiByteToWideChar(
            CP_OEMCP,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide_len,
        );
        if written <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&wide[..written as usize]))
    }
}

/// `bytes` を文字列にする。有効な UTF-8 ならそのまま返す。無効なときは
/// Windows では OEM コードページとして読み直し (それも失敗したら)、
/// 最後の手段として損失付き UTF-8 変換にフォールバックする。
pub fn decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    if let Some(s) = decode_oem(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_passes_through_unchanged() {
        assert_eq!(decode("hello 世界".as_bytes()), "hello 世界");
    }

    #[test]
    fn empty_input_decodes_to_empty_string() {
        assert_eq!(decode(&[]), "");
    }

    // cmd.exe の日本語ローカライズ診断メッセージを模した CP932 バイト列。
    // 「'foo' は、内部コマンドまたは外部コマンドとして認識されていません。」
    // の一部を CP932 でエンコードしたもの (`chcp` 依存の統合テストは環境
    // のロケール設定に左右されるため避け、既知バイト列で decode() 単体を
    // 検証する)。
    #[cfg(windows)]
    #[test]
    fn invalid_utf8_falls_back_to_oem_codepage_on_windows() {
        // 「は」(CP932: 82 CD) だけを含む、UTF-8 としては不正なバイト列。
        let cp932_ha = [0x82, 0xCD];
        assert_eq!(decode(&cp932_ha), "は");
    }

    #[test]
    fn non_codepage_garbage_still_returns_something_via_lossy_fallback() {
        // 有効な UTF-8 でも (Windows では) 有効な CP932 でもないバイト列
        // でも panic せず、何らかの文字列を返す。
        let garbage = [0xFF, 0xFE, 0xFF, 0xFE];
        assert!(!decode(&garbage).is_empty());
    }
}
