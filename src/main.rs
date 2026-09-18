//! バイナリ本体は薄い: 実装は全部 lib 側 (テストから触れるように)。

fn main() -> anyhow::Result<()> {
    shikigami::run()
}
