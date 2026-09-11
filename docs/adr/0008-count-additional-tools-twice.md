# ADR-0008: `additional_tools` の tool を入力の上界に二重に数える

- Status: Accepted
- Date: 2026-09-11

## Context

設計は、平坦なテキスト以外のリクエストの入力上界を `POST /v1/responses/input_tokens` の返す値としている（確定経路、ADR-0003）。これは、その値が生成時の入力トークン数と一致することを前提にしている。

2026-09-11 に、受理する形ごとに `input_tokens` と生成時の `usage.input_tokens` を比べた（`gpt-5.6-terra`）。

| 形 | `input_tokens` | 生成時 |
| --- | --- | --- |
| tool なし / `instructions` / JSON Schema 出力 / `phase` 付き履歴 / インライン画像 | 一致 | 一致 |
| 上位の `tools` に `function`・`custom`・`namespace` | 一致 | 一致 |
| `function_call` / `custom_tool_call` とその出力を含む履歴 | 一致 | 一致 |
| 暗号化された `reasoning` 項目の履歴（`reasoning.context` の 3 値すべて） | 一致 | 一致 |
| **`additional_tools` 項目で `function` と `custom` を宣言** | **246** | **451** |
| 同、往復後の 2 ターン目 | 288 | 493 |
| 同、`function` のみ | 159 | 277 |

差はどの場合も tool 定義を 1 回ぶん描画した量にほぼ等しい。生成は `additional_tools` の tool を、`input_tokens` より 1 回多く描画している。

Codex は GPT-5.6 系に対して常にこの形で tool を宣言する（ADR-0007）。したがって、何もしなければ Codex のすべてのリクエストで入力上界が実際より小さくなる。出力が上限に達する最悪の場合、消費が負債を超え、不変条件が破れる。

## Decision

- **入力上界は、[`CreateRequest::input_count_bodies`](../../crates/protocol/src/request.rs) が返す本文それぞれの `input_tokens` の合計とする。** 1 つめはリクエストそのもの。加えて `additional_tools` 項目ごとに、すべての `additional_tools` 項目を取り除き、その項目の tool を上位の `tools` として宣言した本文を 1 つ加える。特定の tool を名指しする `tool_choice` はその本文から外す。
- **精算時に、実際の `usage.input_tokens` が記録した入力上界を超えたら、そのモデルをラッチする。** 入力上界を予約ごとに記録する必要があるため、admission の組み立てと同時に実装する。
- 形ごとの一致を確かめる実測を契約テストとして残し、モデルやクライアントの版が変わったときに手動で再実行する。

## Rationale

- 加える本文の `input_tokens` は、上位に宣言した tool の描画を含む。観測では、この描画の量が、生成時に余分に描画される量と等しかった。加える本文は残りの入力ももう一度含むので、その分が余裕になる。
- 3 ターンの往復で、上界 506 / 602 / 652 に対して生成時は 460 / 508 / 533 であり、すべて上界の内側だった。
- リクエストそのものは変えない。Codex がモデルに見せる形はそのまま上流へ届く。

## Alternatives considered

**`additional_tools` を拒否する。** GPT-5.6 系で Codex が使えなくなる。

**tool を上位の `tools` へ移して送る。** 数え方は一致するが、モデルに見せる形を変える（ADR-0007）。

**差分の形（tool を宣言した本文の値から、宣言しない本文の値を引いたもの）を加える。** 観測とは正確に一致し、余分な予約も無い。しかし描画が加法的であることに依存し、引き算は仮定が崩れたときに過小評価の側へ誤る。

**固定の倍率を掛ける。** 根拠が無い。

## Consequences

- `additional_tools` 項目 1 つにつき、tool 以外の入力（Codex では会話履歴の全体）をもう一度予約する。長い会話ほど予約が膨らみ、Large Pool での同時実行を減らす。精算で差分は解放される。
- この補正は証明ではなく、観測した描画に基づく。上流が描画を変えれば破れうる。精算時の検査は、破れたことを最初の 1 件で検知してラッチに変えるが、その 1 件を防ぐものではない。
- リクエストごとの `input_tokens` 呼び出しが `1 + additional_tools 項目数` 回になる。この endpoint は実測で無課金であり、レート制限のヘッダも返さない。

## References

- [ADR-0003](0003-reserve-the-liability.md)、[ADR-0007](0007-ingress-allowlist-for-codex-and-openclaw.md)
- [`../requirements/requirements.md`](../requirements/requirements.md) — 実測で決着した事項
- [`../design/design.md`](../design/design.md) — 負債の算定と受理
