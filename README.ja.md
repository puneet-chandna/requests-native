<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

<p align="center">
  <img src="https://raw.githubusercontent.com/puneet-chandna/requests-native/main/docs/assets/logo.png" alt="Requests Native" width="250">
</p>

[English](https://github.com/puneet-chandna/requests-native/blob/main/README.md) · [Español](https://github.com/puneet-chandna/requests-native/blob/main/README.es.md) · [简体中文](https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md) · [Français](https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md) · [हिन्दी](https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md) · [日本語](https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md)

Requests Native は、使い慣れた Python の `requests` API を、Rust で実装した共通の HTTP コアにつなぎます。
同じコアを、Rust 向けのネイティブな非同期クライアントとブロッキングクライアントとして直接利用することもできます。
両方の Rust クライアントは、接続プール、設定可能なタイムアウト、プロキシと TLS の設定、
リクエストとレスポンスのボディのストリーミングに対応しています。

[1.0.0 リリース](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0) ·
[crates.io](https://crates.io/crates/requests-native) ·
[インストール](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) · [Issue](https://github.com/puneet-chandna/requests-native/issues)

## Rust：安定版 1.0.0

Rust クレートは crates.io で公開されています。**Rust 1.98.1 以降**を使い、次の依存関係を追加してください。

```toml
[dependencies]
requests-native = "1.0.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

クレート名は `requests-native`、ライブラリのインポート名は `requests_native` です。
非同期のサンプルには Tokio が必要です。ブロッキングクライアントはデフォルトで含まれています。

### 非同期

```rust
use requests_native::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send().await?;
    println!("{}", response.text().await?);
    Ok(())
}
```

### ブロッキング

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

接続プールを再利用するには、同じクライアントを使い続けてください。

## Python：改良された 1.0.0 のソースをインストール

PyPI への公開は近日中を予定しており、公開設定を仕上げています。
Python の配布パッケージは**まだ PyPI に公開されていません**。
今は改良された `v1.0.0` のソースを利用してください。このリリースには Python の安定版 wheel は含まれていません。

**CPython 3.10+**、Git、リポジトリで固定された **Rust 1.98.1** ツールチェーン、
ネイティブ C/C++ コンパイラ（Windows では MSVC Build Tools）が必要です。
[rustup](https://rustup.rs/) で Rust をインストールし、新しい環境を作成してください。

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

Linux/macOS では、次のコマンドで有効にします。

```console
source .venv/bin/activate
```

Windows PowerShell では、次のコマンドを使います。

```powershell
.venv\Scripts\Activate.ps1
```

タグ付きのソースをインストールします。

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"
```

使い慣れた API を利用できます。

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

配布パッケージは `requests-native` **1.0.0** です。`import requests` と
`requests.__version__ == "2.34.2"` は、Requests との互換性のために元の識別情報を維持しています。

**元の `requests` とは別の環境を使ってください。** 両方の配布パッケージは
同じインポートパスを使います。`requests` という名前のパッケージへの依存関係によって、
元の Requests が再インストールされることがあります。[インストールの詳細](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst)を参照してください。

現在の Windows 検証は、TLS の失敗を許容せずに通過しました。過去の [Issue #1](https://github.com/puneet-chandna/requests-native/issues/1) は再現されておらず、未解決のままです。

## 性能

公開された 1.0.0 の Rust コアを、各ネイティブクライアントで対応する 16 種類のワークロードを使い、ローカルのループバック比較で五組の測定により評価しました。

| ネイティブクライアント | 元の Requests に対するスループット比の中央値 | 元の Requests に対する最大観測スループット比 | 最大観測スループット（リクエスト/秒） | ベータ版に対するスループット比の中央値 |
| --- | --- | --- | --- | --- |
| 非同期 | 2.65x | 11.92x | 13,479 | 1.005x |
| ブロッキング | 2.23x | 9.96x | 10,513 | 1.055x |

比率の中央値は、五組の測定でワークロードごとに求めた 16 個の比率の中央値を、さらに中央値でまとめたものです。
ピーク値は各レポートの単一測定で観測された最大値であり、通常の性能を表すものではありません。最大比率と最大リクエスト数/秒は、異なる測定から得られる場合があります。
改善幅はワークロードによって異なります。オラクルの対照測定が不安定なため、統計的な不確実性が残っています。
[測定方法](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md)と[正確なソースに紐づく検証記録](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md)を参照してください。

## 互換性と開発

組み込みの `Session`/`HTTPAdapter` の通信には、ネイティブトランスポートが使われます。
カスタムアダプター、サブクラス、モンキーパッチ、未対応の拡張動作では、ネイティブ I/O の前に
Python にフォールバックする場合があります。実際のワークロードで連携をテストしてください。

[API 互換性一覧](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv)、
[移植ガイド](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md)、
[貢献ガイド](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md)を参照してください。
脆弱性は[セキュリティポリシー](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md)に従って報告してください。

## クレジット

Requests Native は、[PSF Requests](https://github.com/psf/requests) を
**非公式かつ独立して**書き直したプロジェクトで、
[Puneet Chandna](https://github.com/puneet-chandna) が保守しています。Python Software Foundation
や元の Requests のメンテナーとは提携しておらず、承認を受けているものでもありません。

この派生プロジェクトは、Apache-2.0 ライセンス、通知、履歴、元の貢献者の記録を保持しています。
[LICENSE](LICENSE)、[NOTICE](NOTICE)、
[AUTHORS.rst](AUTHORS.rst)、[ランタイムの通知](RUST_RUNTIME_NOTICES.html)、
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md)を参照してください。
