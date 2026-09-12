# ADR-0009: OpenRouter は回数建ての別経路として足す

- Status: Accepted
- Date: 2026-09-12

## Context

OpenAI の無料枠を使い切ったとき、v1 は 429 を返して終わる。要件は OpenRouter の無料モデルへ退避することを定めているが、どう足すかは決まっていなかった。

2026-09-12 に実測して、前提が 3 つ確まった（詳細は[要件の実測表](../requirements/requirements.md)）。

- **OpenRouter は Responses API を持つ。** stream・reasoning・function tool・function だけの namespace と `additional_tools` を受理する。変換アダプタは要らない。
- **`custom` tool は通らない。** 背後の provider が Chat Completions の function 形として解釈し、`missing field 'parameters'` や `unknown variant 'custom'` で 400 になる。Codex の `apply_patch` は `custom` である。
- **`store: true` は拒否される**（`expected false`）。OpenAI 経路が精算に使う retrieve の足場がここには無い。
- 無料枠は**回数**で、1 日 50・1 分 20。**残量を上流から読む手段は無い**（14 回送っても `/key` の `usage_daily` は 0、`limit_remaining` は `null`）。成功応答にレート制限ヘッダも付かない。

## Decision

**共通 ingress → 正規形 → Provider 別 egress policy とする。** OpenAI 向けに組み立てた最終 payload をそのまま OpenRouter へ送らない。

- OpenAI egress は `background: true`・`store: true`・`service_tier: "default"` を付ける（ADR-0002）。
- **OpenRouter egress はそれらを付けず、`store: false`・`stream: true` と、実測で通った形だけを送る。** 測っていない knob（`prompt_cache_key`、`include`、`reasoning.summary` と `reasoning.context`、`text.verbosity`）は落とす。推測で送って 400 を踏めば、貴重な 1 回を捨てることになる。

**OpenRouter の admission は、予約台帳とは別の資源モデル・別の状態機械とする。** 同じ SQLite ファイルの別テーブル（`request_counter` と `request_dispatch`）に置き、接続とトランザクションの補助コードだけを共有する。

- **送信前に 1 回ぶんを原子的に確保して耐久化し、原則として戻さない。** OpenRouter では失敗したリクエストも 1 日 50 回を消費するため、「予約 → 失敗なら解放 → usage で精算」という OpenAI の状態機械は意味が違う。戻す API を用意しない。
- **1 分 20 回の窓は、送信前に、同じトランザクションで判定する。** 直近の送信時刻を永続化するので、再起動直後に窓を忘れて 429 を踏むことがない。既存の `RateLimitGate`（429 と `Retry-After` に反応する事後型）は二次防御として残す。

**送れない形は送らずに判定する。** モデルごとの対応可否を**実測値として設定に持ち**、リクエストの tool 形（`function` / `custom` / `namespace` / `additional_tools`）がどのモデルにも収まらなければ、OpenRouter へは送らない。`supported_parameters` は provider 実装の違いまでは保証しない。

**経路を開く条件は「有料利用が成立しないこと」とする。** free tier であること、購入 credits が 0 であること、（management key がある場合に）BYOK endpoint が無いこと。TTL 付きの安全入力として扱い、確認できなければ閉じる。

**auto top-up は直接確認しない。** ON/OFF を返す endpoint が公開文書にも実測にも見つからないため、これを TTL 付きの自動確認の対象にすると経路が永久に開かない。**代わりに credits が 0 であることを見る。** top-up が起きれば credits が 0 を超え、次の確認で閉じる。

## Rationale

- 回数建ての資源に予約・解放・精算を持ち込むと、失敗が枠を戻す錯覚を生む。実際には戻らない。状態機械を分ければ、この取り違えが型の上で起きない。
- 送信前に確保して戻さないのは、並列でも 50 を超えず、クラッシュしても超えない最も単純な形である。
- 窓を永続化する案と「起動後 60 秒だけ閉じる」案を比べ、前者を採った。カウンタの加算と同じトランザクションに 1 行足すだけで済み、再起動のたびに 60 秒使えなくなることもない。
- 事前判定は要件の「機能適合性による事前判定」そのものである。OpenRouter では 400 を踏むこと自体が枠を減らすので、ここでの意味はより強い。

## Alternatives considered

**Chat Completions へ変換して送る。** OpenRouter が Responses を持つ以上、変換を挟む理由が無い。SSE の解析も正規形も二重になる。

**既存の token 台帳を単位可変にして再利用する。** 資源の次元と状態遷移が違うものを同じ表に載せることになり、要件が禁じている共通抽象化そのものになる。単位の取り違えは静かに枠を超える。

**`custom` tool を含むリクエストも送ってみる。** 1 回 400 を踏むごとに 50 分の 1 を失う。事前に分かることを実測で分かっているのに試す理由が無い。

**auto top-up を TTL 付きの必須確認にする。** 取得手段が無いので、経路が永久に閉じる。

## Consequences

- **Codex の全 tool 構成は OpenRouter へ退避できない。** `apply_patch`（`custom`）を含むリクエストは、OpenAI の枠が尽きたら 429 になる。plain な会話と `function` tool だけの利用は退避できる。
- 1 日 50 回は少ない。退避は「切らさないための最後の手段」であって、常用の経路ではない。
- 実測値を設定に持つため、**モデルを足すときは測ってから足す**ことになる。測っていない能力は既定で「無い」。
- credits が 0 を超えた瞬間から次の確認までの間は、検知できない。allowlist が有料モデルを塞いでいることが、その間の防壁である。

## References

- [`../requirements/requirements.md`](../requirements/requirements.md) — OpenRouter 経路、Fallback の遷移条件、実測表（2026-09-12）
- [ADR-0002](0002-background-dispatch-for-settlement.md)、[ADR-0003](0003-reserve-the-liability.md)、[ADR-0007](0007-ingress-allowlist-for-codex-and-openrouter.md)
