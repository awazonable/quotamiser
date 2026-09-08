# QuotaMiser

**無料 LLM 自動切替プロキシ。** OpenAI 互換の endpoint を1つ立て、無料 Quota を使い切るまでは無料枠で捌き、**使い切る前に止める**。

TokenMiser 系のツールが「安く済ませる」ことを目指すのに対し、QuotaMiser が守るのは **Quota そのもの**である。安いモデルへ逃がすのではなく、**無料枠を超えそうなリクエストは送らない**。有料 API は明示的に許可しない限り使わない。

## 中核となる仕組み

課金を防ぐのに事後の集計では足りない。使ったと分かった時点で、もう請求は発生している。QuotaMiser は送信前に決める。

```text
リクエスト受信
  → input token + max output token + 安全マージン から最大消費量を保守的に見積もる
  → 残 Quota を超える可能性があれば OpenAI へ送らない        (fail-closed)
  → 送る場合は、送信前に Quota を予約する                    (並列でも超過しない)
  → 応答後、実際の使用量と予約量の差分を精算する
```

Fallback は無料の範囲でのみ連鎖する:

```text
OpenAI 無料 Quota  →  OpenRouter 等の無料 AI  →  Local LLM (Ollama 等)
```

無料で処理できないとき、勝手に有料へ逃げることはしない。

## 状態

**要件定義フェーズ。実装はまだ無い。**

要件は [`docs/requirements/requirements.md`](docs/requirements/requirements.md) を参照。設計と ADR はこれから。

## Attribution

実装フェーズで [`openintelligence-labs/tokenmiser`](https://github.com/openintelligence-labs/tokenmiser)（MIT）から OpenAI 互換プロキシ表面・SSE・provider アダプタ・ルータの骨格を移植する予定である。移植した時点で、上流の著作権表示と vendoring 元の commit SHA を `LICENSE` および本節に記録する。

fork ではなく選択的な移植を選んだのは、本製品の中核である予約型 admission control が上流の事後 USD budget と設計上別物であり、上流追従の利益がコストを下回るためである。

無料 tier を共有 pool key で束ねるカタログ構造と、Quota のリセット時刻に基づくルーティング戦略については [`diegosouzapw/OmniRoute`](https://github.com/diegosouzapw/OmniRoute)（MIT）を設計上の参考実装として読む。コードの移植は行わない。

## License

MIT。[`LICENSE`](LICENSE) を参照。
