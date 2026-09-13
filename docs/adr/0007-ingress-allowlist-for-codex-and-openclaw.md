# ADR-0007: 受け口の allowlist を Codex と OpenClaw の実リクエストに合わせて定める

- Status: Accepted
- Date: 2026-09-11

## Context

要件は受理範囲を deny-by-default の allowlist とし、hosted tool・サーバ側文脈の参照・リモート URL を拒否し、`store` と service tier を固定して送ることを定めている。しかし具体的な列挙は、想定する利用元のリクエスト形を見るまで決められなかった。

利用元は Codex CLI と、OpenClaw をフォークして作るエージェントである。ソースを読むと、要件をそのまま当てはめれば両者とも動かない。

- Codex（0.154.0）は `client_metadata` を送り、そこに git remote URL などワークスペースの情報を入れる。`store: false` と `service_tier` を送る。履歴の各項目に `id` を付ける。
- Codex は GPT-5.6 系に対して、tool を上位の `tools` ではなく `additional_tools` という入力項目で宣言し、`function` と `custom` を `namespace` に包む。`apply_patch` は `custom` tool である。既定では hosted の `web_search` tool も送る。
- OpenClaw（2026.9.4）は `prompt_cache_options`、`metadata` を送り、履歴の項目に `id` と `status` を付ける。

拒否の応答形にも判断が要る。Codex は 400 の本文を表示して止まるが、403 と 422 は想定外のステータスとして再試行する。429 は本文の `type` が `usage_limit_reached` なら再試行しない。

## Decision

**allowlist の正本は [`crates/protocol/src/request.rs`](../../crates/protocol/src/request.rs) とし、版を `responses-ingress/1` とする。** 受理するものを変えるときは版を上げる。

- **tool は `function`、`custom`、`namespace`（中身は `function` と `custom` のみ、入れ子不可）を受理する。** `defer_loading` は `false` のみ。
- **hosted tool と `tool_search` は 400 で拒否し、直し方を本文に書く。** `web_search` には Codex の `web_search = "disabled"` を、`tool_search` には MCP server や app を外すことを示す。
- **入力項目は `message`、`reasoning`（`encrypted_content` 必須）、`function_call`、`function_call_output`、`custom_tool_call`、`custom_tool_call_output`、`additional_tools` を受理する。** それ以外（`web_search_call`、`local_shell_call`、`tool_search_call`、`compaction`、`item_reference` など）は拒否する。
- **各入力項目の `id` と `status` を除去する。** 履歴は内容で送り、保存済み項目への参照にしない。
- **`client_metadata` は受理して除去する。** 上流へ送らない。
- **`store` と `service_tier` は拒否せず上書きする。** 上流には `background: true`、`store: true`、`stream: true`、`service_tier: "default"` を送る。`stream` はクライアントが `true` を送ることを要求する。
- `prompt_cache_options` は唯一の値であり既定値でもある `{"ttl": "30m"}` のみ受理して除去する。
- 画像は `data:` URL のインラインのみ受理する。`reasoning.effort` は `none` から `xhigh` までに限る。
- Codex の組み込み `openai` provider だけが送るフィールド（`stream_options`、`internal_chat_message_metadata_passthrough`、`encrypted_function_args`）は拒否し、Codex では QuotaMiser を custom model provider として設定するよう本文で示す。
- **受理範囲による拒否は 400 `invalid_request_error` で返し、`param` に JSON パス、`code` に種別を入れる。**
- **どの Provider も応答できないとき（枠・レート制限・後述の冷却）は 429 で `type: "usage_limit_reached"` と `resets_at` を返す。**
- **曖昧な失敗が続いた Provider を一時的に止める。** 5xx などの未列挙ステータス、送信後の transport エラー、終端イベント前に終わったストリームを、完了した応答を挟まずに 3 回数えたら 30 秒送信しない。再開後も完了を挟まずに失敗すれば、倍の時間止める（上限 600 秒）。完了した応答で元に戻る。永続化しない。止めている間の予約は未送信として解放する。

**この決定は、`function`・`custom`・`namespace` を伴うリクエストが無料枠で賄われることを主張しない。** 受理するかどうかと、どう課金されるかは別の問いである。後者は実測の観察として要件に記録し、その範囲を超えて一般化しない。

## Rationale

- 拒否ではなく除去・上書きを選んだフィールドは、課金にも負債の算定にも影響せず、拒否すれば Codex が動かないものである。`client_metadata` を上流へ送らないのは、Data Sharing を有効にした組織へワークスペースの情報を渡す理由が無いためである。
- `id` を除去すると、上流が保存済みの項目を参照して、`input_tokens` が数えた以外の内容を展開する余地が無くなる。`store: true` のまま `id` を除いた履歴が受理されることは実測で確認した。
- `custom` と `namespace` を拒否すれば、Codex の `apply_patch` と GPT-5.6 系の tool 宣言がすべて使えなくなる。
- 403 や 422 で拒否すると、Codex は同じリクエストを再送し、同じ拒否を受け続ける。
- 曖昧な失敗は予約を保持する（ADR-0006）。失敗のたびに新しいリクエストで再試行するクライアントと組み合わさると、壊れた Provider が Pool を保持中の予約で埋める。回数と時間で止めればその量が有限になる。429 `usage_limit_reached` を返すのは、Codex に再試行をやめさせる応答がこれであるため。

## Alternatives considered

**要件の文言どおり `store: false` や未知の `client_metadata` を拒否する。** Codex がすべてのリクエストで送るため、Codex が使えない。

**`additional_tools` を上位の `tools` へ移して送る。** `input_tokens` の数え方の問題（ADR-0008）は消えるが、モデルに見せる形を変える。Codex はモデルごとに形を選んでおり、変えたときの品質への影響を測っていない。

**`web_search` を受理し、`tool_usage` を監視して止める。** 検知は課金の後であり、要件の「課金ゼロを担保するのは admission control と allowlist」に反する。

**曖昧な失敗の遮断を永続化する。** 一時的な障害で再起動をまたいで Provider を閉じることになる。401・403・402 のような設定の失敗とは性質が違う。

## Consequences

- Codex を QuotaMiser 経由で使うには、custom model provider として設定し、`web_search = "disabled"` とし、deferred tool を持つ MCP server と app を外す必要がある。
- OpenClaw のフォークは `previous_response_id` による継続を使わない構成にする必要がある（要件でサーバ側文脈の参照を拒否しているため）。
- 冷却は正常な Provider を最大で冷却時間だけ止めうる。完了した応答が 1 件でも挟まれば止まらない。
- クライアントの新しい版が別のフィールドを送り始めると、400 で止まる。deny-by-default の意図した帰結であり、確認したうえで allowlist の版を上げる。

## References

- [`../requirements/requirements.md`](../requirements/requirements.md) — 受理するリクエストの範囲、Fallback の遷移条件、実測で決着した事項
- [`../design/design.md`](../design/design.md)
- [ADR-0002](0002-background-dispatch-for-settlement.md)、[ADR-0006](0006-release-on-synchronous-refusal.md)、[ADR-0008](0008-count-additional-tools-twice.md)
- Codex CLI `rust-v0.154.0`（`6b9826e`）: `codex-rs/core/src/client.rs`、`codex-rs/tools/src/tool_spec.rs`、`codex-rs/codex-api/src/api_bridge.rs`
- OpenClaw `v2026.9.4`（`3a9d69d`）: `packages/ai/src/transports/openai-responses-params-internal.ts`
