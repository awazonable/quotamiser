# QuotaMiser 設計

要件は [`../requirements/requirements.md`](../requirements/requirements.md)。ADR は [`../adr/`](../adr/)。本書はそれらを実装可能な形に落とす。

要件に書かれた制約を繰り返さない。本書が定めるのは、要件が「定める」とだけ述べた具体値と、要件が示した性質を成立させる機構である。

## 出発点

既存コードは無い。`openintelligence-labs/tokenmiser`（Rust、MIT）から**選択移植**する。fork しない。理由は [ADR-0001](../adr/0001-selective-port-from-tokenmiser.md)。

調査の結果、移植できる範囲は当初の想定より狭い（ADR-0001 改訂）。

| 移植する | 新規に書く |
| --- | --- |
| SSE イベントパーサとそのテスト | HTTP サーバ（axum、[ADR-0004](../adr/0004-axum-http-server.md)） |
| エラー応答の整形、本文サイズ上限 | OpenAI 互換の受け口（v1 は Responses API 形。Chat Completions は後から変換アダプタ） |
| CSRF ガード、loopback 既定のバインド | provider アダプタと Dispatcher（[ADR-0005](../adr/0005-upstream-client-without-resend.md)） |
| | ルータ、admission control 一式、予約台帳、受理範囲の allowlist、安全入力と安全ラッチ |

上流の budget 機構は移植しない。**事後記録・USD 建て・プロセス内メモリ**であり、本製品の**送信前・トークン建て・永続**な予約とは設計が別物である。移植時に上流の著作権表示と vendoring 元の commit SHA を `LICENSE` と `README.md` に記録する。

## 中核となる不変条件

**すべての `(pool_id, epoch)` について**

```text
consumed[p,e] + Σ( 活性な予約行の liability )[p,e]  <=  granted[p,e]
```

活性とは `RESERVED` / `DISPATCHING` / `DISPATCHED_WITH_ID` / `DISPATCHED_ID_UNKNOWN` を指す。

**Pool とエポックで添字を付けることが必須である。** 添字の無い書き方では、エポックを跨いだ予約の負債がどちらのエポックに属するか曖昧になり、引き継ぎが在庫を増やす経路を見逃す。

不変条件は**台帳に記録された予約量ではなく、上流に到達しうる最大消費量**について述べている。この 2 つが食い違う設計は、台帳の上では健全なまま Pool を超える。したがって**予約量は、そのリクエストが消費しうる最大量そのもの**でなければならない。以下これを**負債（liability）**と呼ぶ。理由は [ADR-0003](../adr/0003-reserve-the-liability.md)。

## アーキテクチャ

```text
        client (OpenAI 互換)
              │
    ┌─────────▼──────────┐
    │ Ingress            │  正規化・allowlist・機能適合判定
    └─────────┬──────────┘
              │ 正規化済みリクエスト（不変）
    ┌─────────▼──────────┐      ┌──────────────────┐
    │ Admission          │◄────►│ Ledger           │  単一書き込み
    │  負債算定・予約      │      │  SQLite (WAL)    │
    └─────────┬──────────┘      └────────┬─────────┘
              │ 予約 capability            ▲
              │ （正規形リクエストを所有）    │
    ┌─────────▼──────────┐               │      ┌──────────────┐
    │ Dispatcher         │───────────────┼─────►│ 外部 HWM     │
    │  取り消し耐性の監督下  │               │      │ 別ボリューム  │
    └──┬───────┬───────┬─┘               │      └──────────────┘
       │       │       │                 │
    ┌──▼──┐ ┌──▼───┐ ┌─▼────┐            │
    │OpenAI│ │Router│ │Local │            │
    └──┬──┘ └──────┘ └──────┘            │
       │                                 │
    ┌──▼─────────────────┐               │
    │ Settlement         │───────────────┘
    └────────────────────┘

    ┌──────────────┐ ┌──────────┐ ┌─────────────────┐
    │ Safety inputs│ │ Latches  │ │ Trusted clock   │
    │ 期限＋負債予算 │ │ 永続      │ │ 無課金の時刻取得 │
    └──────────────┘ └──────────┘ └─────────────────┘
```

### 予約 capability

**予約 capability は、その負債が対応する正規形リクエストそのものを所有する。**

複製不可能なトークンが「予約が存在すること」しか証明しないなら、負債 1,000 の予約と最大消費 128,000 のリクエストが取り違えられても検出できない。したがって capability は正規形リクエスト・Pool・モデルスナップショットを不変に保持し、Dispatcher は**送信直前にダイジェストを照合する**。

**credential を持つ汎用の送信関数を公開しない。** `input_tokens` と `/v1/models` は予約を伴わないが、これらは**個別に型付けされた狭い操作**として提供する。汎用の例外口を作れば、予約を伴わない推論経路が再び生まれる。

HTTP クライアントと credential は Dispatcher モジュールの私有とする。予約 capability は**送信を開始する時点で消費する**（await した時点ではない）。

## 負債の算定と受理

### 負債の定義

```text
liability = input_bound + output_bound
```

`output_bound` はクライアントが `max_output_tokens` を指定していればその値、なければモデルスナップショットの最大出力トークン数。reasoning token を含む。

`input_bound` は次のいずれか。

**高速経路（構造的上界）** — リクエストが後述の平坦なスカラ形である場合に限る。

```text
input_bound = Σ( 正規化後の対象文字列すべての UTF-8 バイト長 ) + 書式トークン許容量 × 構造ノード数
```

**「本文」は `input` と `instructions` の両方を含む、正規化後の全対象文字列の合計である。** 片方だけを数えれば上界にならない。実装は合計対象を明示的に列挙し、列挙漏れを型で検出できる形にする。

バイト単位 BPE は基本語彙に 256 バイト値をすべて含み、マージは token 数を減らす方向にしか働かないため、`本文の tokens <= 本文の UTF-8 バイト長` が encoding によらず成立する。

**確定経路** — 上記以外はすべて `POST /v1/responses/input_tokens` で数え、返った値の合計を `input_bound` とする。この endpoint は実測で無課金・無枠消費である。

数える本文は `CreateRequest::input_count_bodies` が決める。リクエストそのものに加え、`additional_tools` 項目ごとに、その tool を上位の `tools` として宣言した本文を 1 つ加える。この endpoint は `additional_tools` の tool を生成時より 1 回少なく数えることを実測したためである（[ADR-0008](../adr/0008-count-additional-tools-twice.md)）。他の受理する形では、数えた値と生成時の値が一致した。

補正は観測した描画に基づくため、**精算時に実際の `usage.input_tokens` が `input_bound` を超えたら、そのモデルをラッチする。** 出力の上界に余裕がある限り負債全体の超過には現れないずれを、最初の 1 件で検知するためである。予約ごとに `input_bound` を記録する。

### 高速経路の適用条件

- `input` が単一の文字列であること（配列でも構造化された content part の集合でもない）
- `instructions` が存在するなら単一の文字列であること
- `tools` と `tool_choice` が無いこと
- `text.format` が無いか `text` であること（JSON Schema は入力として数えられる）
- 添付・画像・ファイル・音声が無いこと
- モデルの encoding が明示表にあること

判定は `CreateRequest::flat_text` が行う。

**満たさないものはすべて確定経路へ送る。** 「plain text」という括りでは、1 個の最上位メッセージが多数の content part を含む形を通してしまい、上流の書式トークンは part ごとに増えて上界が破れる。

書式トークン許容量は `input_tokens` との突き合わせで測定し、観測値に余裕を取った定数とする。平坦形ではノード数が固定されるためこの定数の影響は小さい。

### 受理

```text
liability が残枠に収まるなら、liability を予約して送信する
収まらず、かつ高速経路だったなら、確定経路で input_bound を取り直して再判定する
確定値でも収まらないなら送信しない
```

**予約する量は liability そのものである。** 軽量な推定値は反実仮想の記録と TPM の先読みに使ってよいが、**在庫を左右してはならない**。

## 予約の状態遷移

| 遷移元 | 契機となる証拠 | 遷移先 | 群の範囲 | 在庫の変化 | HWM | クラッシュ時 |
| --- | --- | --- | --- | --- | --- | --- |
| （なし） | 受理判定に成功 | `RESERVED` | 当該エポック | `reserved += liability` | — | 行が無ければ受理されていない |
| `RESERVED` | 送信開始の直前 | `DISPATCHING` | 当該エポック | 変化なし | **先に累積負債を進める** | `DISPATCHING` として復帰 |
| `RESERVED` | **1 バイトも渡していないことが確実** | `RELEASED_UNSENT` | 群全体 | `reserved -= liability` | 変化なし | 遷移前なら `RESERVED` のまま |
| `DISPATCHING` | `response.id` を取得 | `DISPATCHED_WITH_ID` | 当該エポック | 変化なし | — | `DISPATCHING` として復帰 |
| `DISPATCHING` | 送信の成否が不明のまま終了 | `DISPATCHED_ID_UNKNOWN` | 当該エポック | 変化なし | — | 同上 |
| `DISPATCHING` | 作成リクエストへの同期的な 400・401・402・403・404・422・429（[ADR-0006](../adr/0006-release-on-synchronous-refusal.md)） | `REJECTED_BEFORE_PROCESSING` | 群全体 | `reserved -= liability` | 変化なし | 未コミットなら `DISPATCHING` として復帰（保守側） |
| `DISPATCHED_*` | 検証を通った usage | `SETTLED` | **群全体** | `reserved -= liability`, `consumed += actual` | — | 未コミットなら再実行 |
| `DISPATCHED_*` / `DISPATCHING` | 回収不能の確定 | `CONSUMED_UNRECOVERABLE` | **群全体** | `reserved -= liability`, `consumed += liability` | — | 同上 |

**再起動時、`DISPATCHING` は保守側に倒して `DISPATCHED_ID_UNKNOWN` として扱う。**

`RELEASED_UNSENT` は、**1 バイトも HTTP スタックへ渡していないことが証明できる場合にのみ**使う。接続確立前の失敗やクライアントの事前キャンセルがこれにあたる。曖昧な送信に対してこの経路を使えば金銭的損失になるため、証拠の条件を実装で明示的に判定する。

### DISPATCHING の永続化と送信の順序

`DISPATCHING` は、**リクエストを HTTP スタックへ渡しうる最初の操作よりも前に**永続化されていなければならない。

async Rust では「await の順序」で守れる境界ではない。HTTP クライアントによっては、返り値の future を**構成する時点で**リクエストが送信キューに積まれる。実装は使用するクライアントについて「バイトを送出しうる最初の操作」を特定し、その前に永続化を完了させる。

送信は**取り消し耐性のある監督下**で実行する。タスクが drop されても状態遷移だけは完了させる。

検証で確認した挙動と、それに基づく規則（[ADR-0004](../adr/0004-axum-http-server.md)、[ADR-0005](../adr/0005-upstream-client-without-resend.md)）:

- axum はクライアント切断から 1 ms 未満でハンドラの future を drop する。**送信・cancel・状態遷移は、ハンドラから spawn した監督タスクが担う。**
- 受け口はリクエスト本文を上限付きで最初に読み切る。未読の大きな本文を保持していると切断が検知されない。
- reqwest の `.send()` は最初の poll まで何も送らない。`DISPATCHING` は最初の poll の前に永続化する。
- 送信後の接続エラーは、上流が要求全体を受け取っていても起こりうる。結果不明として扱い、予約を解放しない。
- **`response.created` を受け取る前に下流が切断した場合、上流との接続をすぐには閉じない。** background で発行した応答は接続を閉じても生成が続く（2026-09-09 実測）ため、id を得る前に閉じると cancel も回収もできず、上流の生成が最後まで枠を消費する。上限付きの時間だけ `response.created` を待ち、id を得たら cancel する。待ちきれなければ接続を閉じて `DISPATCHED_ID_UNKNOWN` とする。

## 永続モデル

SQLite、WAL、`FULL` 相当の同期、**`STRICT` テーブル**。

**設定の成否を検査する。** `journal_mode` の設定は失敗しても以前のモードを返しうるため、戻り値を読まずに成功と見なすと、耐久性が保証されない状態で動きうる。起動時と各書き込み接続で、WAL と `FULL` 相当が実際に有効であることを**照会して確認**し、一致しなければ起動しない。ローカルの対応 VFS 上にあることも確認する。

```sql
CREATE TABLE pool_epoch (
    pool_id   TEXT    NOT NULL,
    epoch     INTEGER NOT NULL,
    granted   INTEGER NOT NULL CHECK (granted  >= 0),
    consumed  INTEGER NOT NULL CHECK (consumed >= 0),
    reserved  INTEGER NOT NULL CHECK (reserved >= 0),
    PRIMARY KEY (pool_id, epoch)
) STRICT;

CREATE TABLE reservation (
    id          INTEGER PRIMARY KEY,
    root_id     INTEGER NOT NULL,
    pool_id     TEXT    NOT NULL,
    epoch       INTEGER NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN
                  ('RESERVED','DISPATCHING','DISPATCHED_WITH_ID',
                   'DISPATCHED_ID_UNKNOWN','SETTLED',
                   'CONSUMED_UNRECOVERABLE','RELEASED_UNSENT')),
    liability   INTEGER NOT NULL CHECK (liability >= 0),
    settled     INTEGER CHECK (settled IS NULL OR settled >= 0),
    response_id TEXT,
    request_digest TEXT NOT NULL,
    -- 予約時点で固定する会計の前提。再起動や設定変更をまたいで検証できるようにする。
    model_snapshot   TEXT NOT NULL,
    service_tier     TEXT NOT NULL,
    accounting_rev   INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (root_id, epoch),
    -- settled は SETTLED のときだけ、かつ必ず存在する。
    CHECK ((state = 'SETTLED') = (settled IS NOT NULL)),
    -- response_id は DISPATCHED_WITH_ID で必須、そこから至る終端状態では残ってよく、
    -- 送信前の状態では存在しない。
    CHECK (CASE
             WHEN state = 'DISPATCHED_WITH_ID' THEN response_id IS NOT NULL
             WHEN state IN ('SETTLED','CONSUMED_UNRECOVERABLE') THEN 1
             ELSE response_id IS NULL
           END)
) STRICT;

CREATE TABLE ledger_meta (
    id                     INTEGER PRIMARY KEY CHECK (id = 1),
    generation             INTEGER NOT NULL,   -- 単調増加
    cumulative_liability   INTEGER NOT NULL,   -- 送信した負債の累計、単調増加
    current_epoch          INTEGER NOT NULL,   -- 単調増加、巻き戻さない
    clean_shutdown         INTEGER NOT NULL    -- 0/1
) STRICT;
```

`safety_input` と `latch` も `STRICT` で定義する。算術は**検査付き整数演算**で行う。

**起動時の整合性検査では、`reserved` を活性状態の行だけから再計算する。** 終端行も `liability` を保持しているため、状態で絞らずに合計すると必ず不一致になるか、終端した予約を復活させる。

### compare-and-reserve

```text
BEGIN IMMEDIATE
  ラッチを確認 → 立っていれば中止
  境界の不確かさ窓に入っていれば中止
  各安全入力について、期限内であり、かつ負債予算が liability 以上残っていることを確認
  SELECT granted, consumed, reserved FROM pool_epoch WHERE pool_id=? AND epoch=current_epoch
  行が無ければ中止（暗黙に作らない）
  granted - consumed - reserved >= liability でなければ中止
  UPDATE pool_epoch SET reserved = reserved + liability
  各安全入力の負債予算から liability を減算する          -- 検査だけでなく引き落とす
  INSERT INTO reservation (..., state='RESERVED', liability=?, request_digest=?, ...)
COMMIT
```

**安全入力の負債予算は、検査するだけでなく同じトランザクションで引き落とす。** 検査だけでは、予算 10,000 に対して 9,000 の負債が 10 本すべて通り、90,000 を受理してしまう。予算は永続化し、単調に消費され、**成功した再検証によってのみ回復する**。精算で戻すことはしない（露出は既に発生しているため）。

`BEGIN IMMEDIATE` で書き込みロックを即座に取り、読んでから書くまでに別の受理が入り込めない。**コミットの成功を確認してから次へ進む。結果が不確実なコミットは失敗として扱い、送信しない。**

### 終端遷移

精算（群単位、冪等）:

```text
BEGIN IMMEDIATE
  root_id の全エポック行を SELECT
  いずれも DISPATCHED_* / DISPATCHING でなければ何もせず COMMIT   -- 二重精算の防止
  各エポック行について:
    UPDATE pool_epoch SET reserved = reserved - liability,
                          consumed = consumed + actual
    UPDATE reservation SET state='SETTLED', settled=actual
COMMIT
```

回収不能の確定も同じ群単位で行い、`consumed += liability`（全額）とする。**ラッチは、それを引き起こした精算と同一トランザクションで立てる。**

### 精算の検証

**次をすべて満たさなければ精算しない。**

- `usage` が存在し、対象フィールドが非負整数である。
- 応答の `model` が予約時に固定した `model_snapshot` と一致する。
- `service_tier` が予約時に固定した値と一致する。途中のイベントと終端のイベントで報告値が変わりうる（`"auto"` で送ると終端は `"default"`、2026-09-11 実測）ため、**照合は終端の値に対して行い、受け口は `"default"` を固定して送る**。
- （未実装）応答オブジェクトの `tool_usage` が hosted tool の利用を示していれば、受理範囲をすり抜けた利用の証拠として Pool をラッチする。hosted tool の利用は無料枠の対象外であり、受理範囲の検査に対する二重防壁になる。
- `accounting_rev` が現行の会計規則と一致する（一致しなければ、その規則で解釈できない）。

Pool 消費量は

```text
actual = usage.input_tokens + usage.output_tokens
```

`output_tokens` は reasoning token を含む。`input_tokens_details.cached_tokens` は `input_tokens` の内訳であり別途加算しない。

**検証を通らない場合、負債を解放してはならない。** 使えない値をゼロとみなして `reserved` から引き、`consumed` に何も足さなければ、不明な在庫がそのまま再受理される。検証を通らない応答は `CONSUMED_UNRECOVERABLE`（全額消費）とし、ラッチを同一トランザクションで立てる。

実消費が予約を超えていた場合は、**超えた実測値で精算する**（予約量に丸めない）。同時に上限超過ラッチを立てる。

## エポックとロールオーバ

### 境界の不確かさ窓

上流が日付境界を越えたかどうかが不確かな区間では、**OpenAI の受理を停止する**。

「信頼できる時刻が無ければロールオーバしない」だけでは逆向きの穴が開く。上流が既に翌日に入っているのに手元が前日のままなら、**前日の残枠を使って上流の今日に課金され**、後で今日を満額で開けるので同じ枠が二度使われる。

したがって:

- **境界が起きえた最も早い時刻から受理を閉じる**（ローカル時計の不確かさを考慮した下限）。
- 信頼できる新鮮な時刻を取得し、ロールオーバをコミットしてから受理を再開する。
- 不確かな区間に送信してしまったリクエストがあれば、**確定した新エポックへ計上する**。

### ロールオーバ

**単一の冪等なトランザクション**で次をすべて行う。

```text
BEGIN IMMEDIATE
  新エポックの pool_epoch 行を作成する（granted を設定）
  未終端の各予約について、新エポック行を root_id 付きで挿入する（既にあれば何もしない）
  新エポックの reserved を、いま新たに挿入した行の liability の合計だけ増やす
  ledger_meta.current_epoch を進める（単調、巻き戻さない）
COMMIT
```

**予約行を挿入するだけでは在庫を予約したことにならない。** `pool_epoch.reserved` を上げなければ、次の受理は `reserved = 0` を読んで満額を配ってしまう。「いま新たに挿入した行の分だけ」と限定するのは、再実行時に二重に加算しないためである。

境界を跨いだ応答は、実消費を両エポックに計上する。

### 時刻源

**課金も枠消費もしない時刻源を、推論の可否から独立に持つ。** `GET /v1/models` は無料で応答に `Date` ヘッダを含むため、推論経路が閉じていても取得できる。

境界は 00:00 UTC。これより早い設定を許さず、設定可能範囲に安全な下限を強制する。正方向スキューガードは 5 分、時刻の不確かさがこれを超える場合はその分だけ広げる。

## 巻き戻しの検出

**等値は信頼の証明にならない。** スナップショット後に送信し、その消費が反映される前に復元すれば、台帳も局所記録も Usage API も整合したまま在庫だけが復活する。

### 順序の定めた手順

外部記録は**台帳とは別のボリュームに置く**（推奨ではなく要件）。同一ボリュームに置けば一緒に巻き戻り、比較対象にならない。

送信の前に、次の順序で進める。

1. `compare-and-reserve` をコミットする。
2. **外部記録に `generation` と `cumulative_liability` を書き、`fsync` する。**
3. `DISPATCHING` を台帳にコミットする（`cumulative_liability` を同じ値へ進める）。
4. 送信する。

外部記録が先に進むため、**常に `外部.cumulative_liability >= 台帳.cumulative_liability`** が成り立つ。

### 起動時の判定

```text
外部記録が無い、または読めない                     → STATUS_UNKNOWN
外部.generation < 台帳.generation                → 外部が巻き戻った → STATUS_UNKNOWN
外部.cumulative_liability < 台帳.cumulative_liability → ありえない → STATUS_UNKNOWN
差分が、活性な DISPATCHING 行で説明できる量を超える  → 台帳が巻き戻った → STATUS_UNKNOWN
clean_shutdown が立っていない                     → STATUS_UNKNOWN
```

**いずれかに該当すれば `STATUS_UNKNOWN`。** 部分的な記録や比較不能な記録を「たぶん大丈夫」と扱わない。

`clean_shutdown` は起動後の最初の書き込みで落とし、正常終了時にのみ立てる。

Usage API は**ドリフトの検出にのみ**用いる。`台帳.consumed` が Usage API の報告値を下回っていれば巻き戻しである。ただし**下回っていないことは信頼の証明ではない**。照会は対象モデル群・エポック境界・ページングの完了を明示して実装し、**遅延窓の内側は判定に使わない**。

**ホスト全体を巻き戻された場合、ローカルのどの手段でも検出できない。** 要件の「保証の対象外」に含まれる残存リスクである。

## ロック

**保護対象は台帳ファイルでも credential でもなく、付与量を共有する資源である。**

- ロックの identity は**組織識別子と Pool 識別子から導出する**。同じ組織の別 credential は同じ Pool を消費するため、credential を鍵にすると 2 プロセスが別々のロックを取り、それぞれ満額の Pool を見る。
- **カーネルが保持する排他ロック**を、ホスト全体で正規な位置に取る。ファイルの存在や PID ではなく**カーネルの所有権を権威とする**。プロセス死亡時に自動解放されるため、古いロックで復旧不能にならない。
- PID や起動時刻は診断用メタデータとしてのみ書く。

## 具体値

すべて設定可能とし、安全側の下限・上限を強制する。

| 項目 | 既定値 | 根拠 |
| --- | --- | --- |
| retrieve 再試行間隔 | 5s から指数バックオフ、上限 60s | cancel の確定は実測で約 90 秒 |
| 回収の escalation | 15 分 | 運用者への通知。**確定はしない** |
| 回収の打ち切り | 当該エポックが照合から利益を得られなくなるまで、または応答保持期限まで | 15 分で確定すると、実消費 1k を 128k の永久損失に変える。回収の継続は枠を減らさない |
| `DISPATCHED_ID_UNKNOWN` の確定 | 5 分 | 回収手段が無いので保持する意味がない |
| 未終端 background の上限 | 32（回収ワーカーの処理能力に基づく） | 枠の安全は負債が担う |
| 安全入力の TTL | data sharing / project 15 分、カタログ 24 時間 + 起動時、OpenRouter 15 分 | 負債予算と併用 |
| 安全入力の負債予算 | Pool 付与量の 1/4 | 検証が古いまま失われうる額を有限にする |
| スキューガード | 5 分（時刻の不確かさが大きければその分広げる） | |
| `STATUS_UNKNOWN` の再取得間隔 | 30 分 | 要件どおり |
| 曖昧な失敗による冷却 | 完了を挟まない 3 回で 30 秒。再開後も失敗が続けば倍、上限 600 秒 | 失敗のたびに再試行するクライアントが、壊れた Provider に予約を積み上げる量を有限にする（ADR-0007） |
| OpenRouter の 1 日の回数 | 50（生涯で $10 以上購入した口座では 1,000） | 実測。上流に残量を照会する手段が無い |
| OpenRouter の短い窓 | 60 秒に 20 回。送信前に判定し、送信時刻を永続化する | 成功応答にレート制限ヘッダが返らないため、事後では抑えられない。再起動で窓を忘れない（ADR-0009） |
| OpenRouter アカウントの再確認 | TTL の 1/3、最短 60 秒（既定 300 秒） | 確認できなければ経路を閉じる |

可変なモデル別名は避け、**不変のモデルスナップショット**を優先する。別名を使う場合は Pool 所属・最大出力・context 上限・tokenizer 同一性を版に束ね、受理前に一括で検証する。検証から送信までの競合は原理的に消せないため残存リスクとして記録する。

## Provider アダプタ

共通に必要な能力は「このリクエストを受けられるかの判定」と「予約 capability を伴う送信」である。**トレイトの形は移植元を見てから決める。** 台帳の抽象化はしない（要件の非目標）。

- **OpenAI**: Responses API、`background: true`、`store: true`。予約・精算・回収の全機構が付く。
- **OpenRouter**: トークン台帳を持たない。**1 日 50 回・1 分 20 回の回数建て**であり、送信前に 1 回ぶんを確保して原則戻さない（失敗したリクエストも枠を消費するため、ADR-0009）。Responses API で送るが、`store: false` とし `background` と `service_tier` は付けない。`:free` と `openrouter/free` の完全一致 allowlist に加え、**モデルごとの実測 capability で送信前に適合を判定する**（`custom` tool は通らない）。402 は provider を閉じて次へ。
- **Local (FreeToken Desktop)**: 枠が無い。`GET /health` で `ok` / `loading` / `error` を判定し、`loading` は down として扱わない。**同時実行を自前で数え**、上限超過は送信前に拒否する。retrieve は試みない。

## 失敗時の挙動

要件の遷移表に加えて 1 つ。**provider を閉じる操作はラッチと同じ永続化を持つ。** 401 / 403 / 402 で閉じた provider は再起動で自動的に開かない。429 や一時的な 5xx による退避は永続化しない。曖昧な失敗が続いたことによる冷却も永続化しない。

Dispatcher は、レート制限のゲートが閉じているか冷却中であれば送信せず、予約を未送信として解放する。曖昧な失敗として数えるのは、送信後の transport エラー、予約を保持するステータス、終端イベント前に終わったストリームである。クライアントが切断したことによる打ち切りは数えない。終端イベントを受け取れば、回数と冷却時間を初期値に戻す。

OpenRouter の経路には、これとは別の門が 2 つ前段にある。**アカウントの確認**（free tier・購入 credits が 0・BYOK 無し）が通らなければ経路ごと閉じる。**要求の形を受けられるモデルが無ければ、そもそも送らない。** どちらも回数を消費しない。回数を消費するのは、確保に成功して実際に送った場合だけである。

**OpenAI が枠不足で断ったときにだけ、次の経路へ回す。** 受理範囲やカタログによる拒否は、別の Provider でも同じく誤りなので回さない。

## テスト戦略

**壊れると金銭的被害が出る性質は、冒頭の不変条件 1 つである。** 台帳に記録された値だけを検査するテストは、初稿の欠陥（受理と予約に別の量を使う構造）をそのまま合格させた。

- **ゴースト変数を持つ property-based test。** 上流に到達しうる各リクエストの最大消費量をモデル側で `(pool, epoch)` ごとに追跡し、**送信の線形化点で**冒頭の不変条件を表明する。台帳の値ではなくゴースト量を検査する。**引き継ぎで生じた複数エポックの負債も、この添字付きで検査する。**
- **生成した履歴を実物の SQLite に対しても流す。**
- **障害注入**を少なくとも次の点で行う。`DISPATCHING` の永続化とバイト送出の間、外部 HWM の `fsync` と台帳コミットの間、送信と `response.id` 取得の間、精算コミットとラッチコミットの間、ロールオーバの途中とその再実行、各状態遷移での kill。
- **巻き戻し。** 台帳・WAL・外部記録の各組み合わせを復元し、Usage API の遅延で隠れた直近消費を含めて検査する。
- **時刻。** 時計の飛び、時刻源の喪失、境界の不確かさ窓に入ったまま送信した場合の計上先。
- **多重起動。** 同一組織・同一 Pool・**異なる credential と異なる DB パス**の 2 プロセスが同時に受理しないこと。
- **SQLite の設定失敗。** WAL / 同期モードの設定が失敗したときに起動しないこと。
- **安全入力の負債予算。** 予算未満の負債を多数通しても総量が予算を超えないこと。
- **受理範囲の allowlist。** 拒否すべきフィールド・tool・サーバ側文脈参照・リモート URL・構造化 content part の各ケース。列挙外のフィールド名が必ず拒否されることを性質テストで確かめる。Codex と OpenClaw の実際の形が受理され、正規化が冪等であることも確かめる。
- **`input_tokens` と生成時の入力の一致。** 受理する形ごとに実 API で突き合わせる契約テストを、モデルやクライアントの版が変わったときに手動で再実行する（ADR-0008）。
- **回数建ての経路。** 1 日の上限で送信が止まること、短い窓が送信前に抑えること、窓が再起動をまたいで残ること、**失敗したリクエストも 1 回を消費すること**、受けられない形が送られないまま終わること。
- **上流クライアントの再送とリダイレクト。** OpenAI・OpenRouter・Local の各 Provider について、モックサーバが 307 / 308 を返したときに追従先へ送られないこと、Provider が永続的に閉じられること、予約が保持されることを確認する（[ADR-0005](../adr/0005-upstream-client-without-resend.md)）。
- 実 API に対する契約テストは CI から分離し、手動で実行する。`quotamiser-probes.ps1` が原型となる。

## 安全上の制約

- 予約 capability は正規形リクエストを所有し、Dispatcher は送信直前にダイジェストを照合する。credential を持つ汎用の送信関数を公開しない。`input_tokens` と `/v1/models` は個別に型付けされた狭い操作とする。
- 受理範囲は allowlist であり、拡張時に「拒否リストへの追加漏れ」が起きない構造にする。
- credential は 4 種。OpenAI の推論用と Admin、OpenRouter の推論用と（任意の）management。**Admin key は Usage API の照会にのみ、management key は BYOK の照会にのみ使い**、どちらも推論経路から到達不能にする。
- ログに credential とプロンプト本文を残さない。トークン数と識別子のみを記録する。

## 実装の現状

### 台帳（`crates/ledger`）— 実装済み

予約台帳・状態遷移・ロールオーバ・起動時の信頼判定・外部 HWM・ロックを実装した。ネットワークに依存しないクレートとして分離しており、送信経路を構造的に持たない。

**回数建ての資源は、同じ SQLite の別テーブル（`request_counter` と `request_dispatch`）に置く。** 予約台帳とは資源の次元も状態遷移も違うため、解放の口を持たない。共有するのは接続とトランザクションの補助だけである（ADR-0009）。

実装で確定した点:

- **状態遷移はすべて群単位で適用する。** 本書の表で「当該エポック」としていた遷移も含む。引き継ぎ行の状態が食い違う余地を構造的に消すため。
- `reservation.id` と `root_id` は整数とした。
- **スキーマの `response_id` 制約を修正した。** 当初の制約は `DISPATCHED_WITH_ID` から回収不能に至った行（`response_id` を保持している）を拒否していた。
- ラッチのスコープは `global` / Pool id / `model:<snapshot>`。上限超過はモデルを、検証を通らない usage は Pool をラッチする。
- `release_unsent` は安全入力の負債予算を払い戻さない。
- **信頼判定が `Trusted` でも `UncleanShutdown` でもない状態で起動した場合、`RESERVED` を含む非終端の行をすべて `DISPATCHED_ID_UNKNOWN` として扱う。** 外部記録と整合しない台帳では、未送信であることを証明できないため。
- 外部記録が台帳と同じボリュームにある構成は、`open` で拒否する。

検証:

- ゴースト変数による model-based property test を実物の SQLite 上で実行し、決定的な単体テストと合わせて全件通過。
- **テストのオラクルが欠陥を検出できることを、意図的な不具合で確認した。** 予約量の半減、ロールオーバでの予約計上漏れ、不正 usage で消費を計上しない、残枠判定のずれ、負債予算の引き落とし漏れの 5 種すべてを property test 単独で検出した。

### proxy 層 — 一部実装済み

- **`crates/admission`（I/O なし）:** 日付境界の判定と負債の見積り器。性質テストの検出力を、意図的に不具合を入れた複製で確認した。
- **`crates/protocol`（I/O なし）:** SSE のイベント境界検出とエラー応答の整形（tokenmiser から移植）、Responses イベント列の観測器、受け口の allowlist と正規化（`request.rs`、版 `responses-ingress/1`、ADR-0007）、**Provider 別の egress**（OpenAI と OpenRouter）と tool 形の判定。
- **`crates/proxy`:** 上流ステータスの判定、上流 HTTP クライアント（再送・リダイレクト追従・システムプロキシを無効化）、Dispatcher と監督タスク、回収ワーカー、信頼できる時刻源、レート制限ゲート、曖昧な失敗による冷却、**admission の組み立て、起動と各ループ、HTTP の受け口と実行バイナリ、OpenRouter のクライアント・回数建ての送信・経路選択**。

### 未実装

- 予約ごとの `input_bound` の記録と、精算時に実 input がそれを超えたときのラッチ（ADR-0008）。
- 取得した応答オブジェクトの `billing.payer` による停止条件。stream の終端イベントには `billing` が含まれないため、回収または精算後の取得で判定する。
- `STATUS_UNKNOWN` からの復旧手順。
- Local の provider アダプタと、Provider ごとのリダイレクトのテスト。
- HTTP/2 の REFUSED_STREAM で再送しないことを固定するテスト、`tool_usage` による hosted tool 利用の検出。
