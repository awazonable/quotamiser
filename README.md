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

**OpenAI 経路が動く。** 予約台帳（[`crates/ledger`](crates/ledger)）、日付境界と負債の算定（[`crates/admission`](crates/admission)）、受け口の allowlist と正規化（[`crates/protocol`](crates/protocol)）、admission・送信・精算・HTTP の受け口（[`crates/proxy`](crates/proxy)）を実装済み。

**OpenRouter への退避も動く。** OpenAI の枠が足りないとき、要求の形を受けられる無料モデルがあれば OpenRouter へ回す。

**未実装:** Local LLM への退避、Chat Completions の受け口。どちらの経路も使えないときは 429 を返す。

2026-09-12 にローカルで起動し、1 件を実際に通して確認した。入力 13 トークンを上流の counter で数え、出力上限 64 と合わせて **77 を予約**してから送信し、終端イベントの usage（in 13 / out 5）で **18 を精算**、残りは解放された。

## 使い方

### 1. 用意するもの

- Rust ツールチェイン（[`rust-toolchain.toml`](rust-toolchain.toml) が指定する版を rustup が自動で入れる）
- Data Sharing に opt-in 済みで、complimentary daily tokens の対象である OpenAI organization
- **2 種類の key。** 推論用の API key と、Usage API 照会用の Admin key。Admin key は usage の読み取りにしか使わず、推論経路からは到達できない

### 2. ビルド

```bash
cargo build --release -p quotamiser-proxy --bin quotamiser
```

### 3. 設定

[`quotamiser.example.toml`](quotamiser.example.toml) を `quotamiser.toml` にコピーして編集する。設定ファイルに key は書かない。**key を持つ環境変数の名前**を書く。設定ファイルの隣に `.env` があれば、未設定の変数だけそこから読む。

最低限、次を自分の環境に合わせる。

- `[ledger] organization` — 対象の organization ID
- `[ledger] external_record_path` — **台帳とは別ボリューム**に置く。台帳を失ったことを検知する外部記録であり、同じボリュームに置くと両方まとめて失われる。単一ボリュームの試用機では `allow_same_volume_external_record = true` を使うが、これはその検知を捨てる設定である
- `[[pool]] granted_per_day` — 自分の tier の付与量。既定値は tier 1（Large 250,000 / Small 2,500,000）
- `[[model]] max_output_tokens` — **小さく書くと保証が壊れる。** 出力上限を指定しないリクエストはこの値を予約する

### 4. 起動

```bash
./target/release/quotamiser quotamiser.toml
```

起動時に次を順に行う。どれかが通らなければ、受理せずに終了するか、受理を閉じたまま動き続ける。

1. 上流の `Date` ヘッダから**信頼できる時刻**を取る（無課金）。ローカルの時計は信用しない
2. その時刻が指すエポックへ台帳をロールオーバする
3. 台帳を信頼できない場合（初回起動、異常終了の後）だけ、その日の消費を Usage API から復元する。**復元を経た台帳に fail-closed は主張しない**（反映遅延を吸収するため）。ログにもそう出る
4. Data Sharing を確認する。確認できなければ受理は閉じたままになる

停止は **Ctrl-C**。台帳に clean shutdown の印が付き、次の起動は `Trusted` として始まる（復元を行わない）。プロセスを強制終了した場合、次の起動は `UncleanShutdown` として扱い、上の 3 を実行する。どちらも確認済み（前者はテスト、後者は実測）。

### 5. 受け口

- `POST /v1/responses` — Responses API 形。**`stream: true` が必須**
- `GET /v1/models` — 設定したモデル一覧（上流には問い合わせない）

```bash
curl -N http://127.0.0.1:8787/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-5.6-terra","stream":true,"max_output_tokens":64,
       "input":[{"type":"message","role":"user",
                 "content":[{"type":"input_text","text":"Reply with the single word ok."}]}]}'
```

応答には、処理した Provider とモデルを `x-quotamiser-provider` / `x-quotamiser-model` で返す。

### 6. 断られたとき

| 状況 | 応答 |
| --- | --- |
| 受理範囲外（hosted tool、未知のフィールド、`previous_response_id` など） | **400** `invalid_request_error`。`param` に JSON パス、`code` に種別、本文に直し方 |
| 設定していないモデル | **400** `model_not_configured` |
| 枠不足・レート制限・曖昧な失敗による冷却・日付境界 | **429** `usage_limit_reached`（分かれば `resets_at` 付き）。**送信していないので枠は減っていない** |
| 入力を数えられない、台帳に触れない | **503**。上界が立たないので送らない |

### 7. クライアント側の設定

**Codex CLI。** 組み込みの `openai` provider を上書きせず、custom provider として登録する。組み込みのままだと、QuotaMiser が受理しないフィールドを送る。

```toml
# ~/.codex/config.toml
model = "gpt-5.6-terra"
model_provider = "quotamiser"
web_search = "disabled"          # hosted の web search は無料枠の対象外

[model_providers.quotamiser]
name = "QuotaMiser"
base_url = "http://127.0.0.1:8787/v1"
wire_api = "responses"
```

あわせて、deferred tool を持つ MCP server と app をこのプロファイルから外す（入っていると Codex が `tool_search` を送り、拒否される）。この設定は Codex 0.154.0 のソースから導いたもので、**Codex を実際に通した確認はまだ行っていない**。

**OpenClaw。** base URL を `http://127.0.0.1:8787/v1` に向け、`previous_response_id` による継続を使わない構成にする（サーバ側に保存された文脈は、送信前に入力量を確定できないため受理しない）。

### 8. OpenRouter への退避

OpenAI の枠が足りないとき、**要求の形を受けられる無料モデルがあれば** OpenRouter へ回す。設定は [`quotamiser.example.toml`](quotamiser.example.toml) の `[openrouter]` 節で、節ごと消せば OpenAI だけで動く。

- **回数で数える。** OpenRouter の無料枠はトークンではなく**リクエスト回数**（無入金の口座で 1 日 50 回、1 分 20 回）。**失敗したリクエストも 1 回を消費する**ため、送信前に 1 回ぶんを確保し、結果によらず戻さない。上流に残量を照会する手段が無いので、回数は自前で数える。
- **送る前に形を見る。** `custom` tool（Codex の `apply_patch`）は、OpenRouter の背後の provider が受理しないことを実測した。こうしたリクエストは**送らずに**断る。送って 400 を踏めば、それだけで 1 回を失うため。モデルごとの対応可否は設定に**実測値**として書く。測っていない能力は既定で「無い」。
- **経路が開く条件。** free tier であること、購入 credits が 0 であること、（management key を設定した場合に）BYOK endpoint が無いこと。TTL で再確認し、確認できなければ閉じる。auto top-up の設定を返す API は見つからなかったので直接は見ない。top-up が起きれば credits が 0 を超え、次の確認で閉じる。
- **退避しないもの。** 受理範囲による 400 や未設定モデルは、別の Provider でも同じく誤りなので退避しない。`custom` tool を含むリクエストも退避できないため、**Codex の全 tool 構成は OpenRouter では動かない**。

応答の `x-quotamiser-provider` が `openrouter`、`x-quotamiser-model` が実際に使われた `:free` モデル ID になる。

2026-09-12 に実測で確認した。日次枠を 10 トークンだけにした設定で平文のリクエストを送ると、OpenAI 側が枠不足で断り、`nex-agi/nex-n2.5-pro:free` が応答を返し（`x-quotamiser-provider: openrouter`）、台帳の回数は 1 増えた。`custom` tool を含むリクエストは 429 で断られ、**回数は 1 のまま**だった。

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
