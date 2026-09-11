# QuotaMiser

**無料 LLM 自動切替プロキシ。** OpenAI 互換の endpoint を1つ立て、無料 Quota を使い切るまでは無料枠で捌き、**使い切る前に止める**。

TokenMiser 系のツールが「安く済ませる」ことを目指すのに対し、QuotaMiser が守るのは **Quota そのもの**である。安いモデルへ逃がすのではなく、**無料枠を超えそうなリクエストは送らない**。v1 には有料 API を使う経路そのものが無い。

## 中核となる仕組み

課金を防ぐのに事後の集計では足りない。使ったと分かった時点で、もう請求は発生している。QuotaMiser は送信前に決める。

```text
リクエスト受信
  → input token + max output token から最大消費量（負債）を保守的に見積もる
  → 残 Quota を超える可能性があれば OpenAI へ送らない        (fail-closed)
  → 送る場合は、送信前に負債そのものを予約する              (並列でも超過しない)
  → 応答後、実際の使用量と予約量の差分を精算する
```

Fallback は無料の範囲でのみ連鎖する:

```text
OpenAI 無料 Quota  →  OpenRouter 無料モデル  →  Local LLM (FreeToken Desktop)
```

無料で処理できないとき、勝手に有料へ逃げることはしない。

## 状態

**実装フェーズ。** 予約台帳（[`crates/ledger`](crates/ledger)）を実装済み。OpenAI 互換表面・provider アダプタ・ルータは未実装で、プロキシとしてはまだ動作しない。

要件は [`docs/requirements/requirements.md`](docs/requirements/requirements.md)、設計は [`docs/design/design.md`](docs/design/design.md)、判断の記録は [`docs/adr/`](docs/adr/) を参照。

## Attribution

[`openintelligence-labs/tokenmiser`](https://github.com/openintelligence-labs/tokenmiser)（MIT、Copyright (c) 2026 Open Intelligence Labs contributors）から、commit `5fe22e826a0fde09b6910b273dc45bed24316f9f` 時点の次の部品を移植している。上流の許諾表示は [`LICENSE`](LICENSE) に収録した。

- SSE のイベント境界検出、行終端の扱い、バッファ上限とそのテスト（[`crates/protocol/src/sse.rs`](crates/protocol/src/sse.rs)）
- OpenAI 形式のエラー応答の整形（[`crates/protocol/src/error_body.rs`](crates/protocol/src/error_body.rs)）

本文サイズ上限や CSRF ガードなどの小さな部品も、必要になった時点で同じく移植し、ここに記録する。サーバ基盤、provider アダプタ、ルータは本製品の制約に合わないため新規に書く（[ADR-0001](docs/adr/0001-selective-port-from-tokenmiser.md) 改訂）。

fork ではなく選択的な移植を選んだのは、本製品の中核である予約型 admission control が上流の事後 USD budget と設計上別物であり、上流追従の利益がコストを下回るためである。

無料 tier を共有 pool key で束ねるカタログ構造と、Quota のリセット時刻に基づくルーティング戦略については [`diegosouzapw/OmniRoute`](https://github.com/diegosouzapw/OmniRoute)（MIT）を設計上の参考実装として読む。コードの移植は行わない。

## License

MIT。[`LICENSE`](LICENSE) を参照。
