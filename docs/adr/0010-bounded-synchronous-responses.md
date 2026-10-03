# ADR-0010: 短い上限付き応答は同期実行にする

- Status: Accepted
- Date: 2026-09-14

## Context

`background: true` は、クライアント切断後も応答を生成し続け、`response.id` を使って cancel・retrieve できる。これは OpenAI のトークン枠を正確に精算するために必要だが、短い会話でも数秒のレイテンシを追加する。音声チャットではこの遅延を受け入れられない。

同期 stream を切断した応答は retrieve できないことも実測済みである。そのため、短い応答だけ同期化する場合は、切断時の正確な usage 回収を諦める必要がある。

## Decision

`[safety] synchronous_max_output_tokens`（既定値 `512`）を設ける。

- `max_output_tokens` が未指定、または上限が閾値を超えるリクエストは `background: true`、`store: true` で送信し、既存の cancel・retrieve 回収経路を使う。
- 明示された上限が閾値以下のリクエストは `background: false`、`store: true` で送信する。通常完了時は終端 usage で精算する。
- 同期経路のうち、平坦なテキスト入力が `synchronous_max_input_bytes`（既定 `4096`）と `synchronous_max_input_characters`（既定 `1024`）の両方以下なら、`input_tokens` 照会を省き、UTF-8 バイト長と固定 framing allowance を入力上界として使う。構造化入力または長い入力は、同期経路でも厳密カウントする。文字数は Unicode scalar values の数であり、512 token を保証する値ではない。
- 同期経路でクライアント切断、ストリーム異常終了、または終端 usage 欠落が起きた場合は retrieve を待たず、予約全額を `CONSUMED_UNRECOVERABLE` として write-off する。予約を解放して無料扱いにはしない。

閾値は設定値として変更可能にし、実測した遅延と write-off の割合を見て運用で調整する。

## Consequences

- 明示的に小さな上限を指定する音声・短文リクエストは、background の数秒の遅延を避けられる。
- 同期経路では切断 1 回につき予約全額が失われうる。閾値で損失の上限を抑えるが、入力分と reasoning token を含む出力上限分を write-off する可能性がある。
- 未確認の encoding に対する UTF-8 バイト上界は、byte-fallback BPE という前提に依存する。前提が破れた場合は最初の usage overrun でモデルをラッチするが、その1件の超過は防げない。文字数制限はこのリスクを説明可能な短い窓に抑えるためのもので、トークン数の証明ではない。
- 上限未指定または大きな出力を要求するリクエストは、従来どおり遅延しても正確な回収を優先する。
- `store: true` は両経路で維持する。同期経路の通常完了応答を扱えることと、OpenAI 側の応答保持仕様との整合性を保つためである。
