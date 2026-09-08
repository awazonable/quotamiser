# QuotaMiser 要件

## 目的

LLM API を OpenAI 互換 Proxy 経由で利用し、無料枠を最大限活用しつつ、**意図しない課金を確実に防止する**。

## 機能要件

### 無料 Quota の管理

- OpenAI API の Data Sharing 等による無料 Quota を最優先で利用する
- 無料 Quota は**共有 Pool 単位**で管理する
- 日次 Quota の**残量とリセット時刻**を管理する

### Admission control（本製品の中核）

- リクエスト送信前に、input token・max output token・安全マージンから**最大消費量を保守的に見積もる**
- 最大消費量が残 Quota を超える可能性がある場合、そのリクエストを OpenAI へ**送信しない**
- 並列リクエストによる Quota 超過を防ぐため、**送信前に Quota を予約する**
- 実際の token 使用量取得後、**予約量との差分を精算する**
- 無料 Quota 超過による課金は **fail-closed** で防止する

### Fallback

- OpenAI 無料枠を安全に利用できない場合は、他の無料 Provider へ自動 Fallback する
- 主な Fallback 先として OpenRouter 等の無料モデルを利用可能にする
- 外部の無料 Provider が利用不能・制限到達の場合は、自宅の Local LLM へ Fallback する
- Local LLM は Ollama 等の一般的な API を利用可能にする

基本 Fallback 連鎖:

```text
OpenAI 無料 Quota  →  OpenRouter 等の無料 AI  →  Local LLM
```

### インタフェース

- OpenAI 互換 API として既存アプリから透過的に利用できる

## 制約

- **有料 API の利用は明示的に許可しない限り禁止する**
- 原則として、無料で処理できない場合でも勝手に有料 API へ Fallback しない

## モデル指定時のルーティング（補助要件）

モデル指定アクセスは、可能な範囲で指定モデルを優先する。

| 指定されたモデル | ルーティング |
| --- | --- |
| Data Sharing 対象の OpenAI モデル | 当日の無料 Quota に余裕があれば指定モデルへ送信 → 利用できなければ無料 Fallback へ |
| Data Sharing 対象外の OpenAI モデル | Data Sharing 対象の低レベル最高モデル（Luna） → その他の無料 API → Local LLM |
| 無料 API 系モデル | 指定された無料 API/モデル → 利用不能時は Local LLM |

このルーティングは**補助要件**とし、**無料枠超過防止を最優先**する。

## 非目標

- 有料 API のコスト最適化（安い有料モデルへの誘導）は目的ではない。有料は既定で禁止であり、最適化の対象ですらない
- 汎用的なマルチプロバイダ・ゲートウェイを目指さない。無料枠を守り切ることに範囲を絞る

## 未解決の質問

- 「Data Sharing 対象の低レベル最高モデル（Luna）」の具体的なモデル ID
- OpenAI 側の無料 Quota の実際の粒度 — 日次リセットの基準時刻（UTC か否か）、共有 Pool の単位（組織単位か、モデル群単位か）
- 予約 ledger の永続化要否。プロセス再起動をまたいで残量を保つ必要があるか（在庫を見失うと fail-closed が破れる）
- 見積りに使う tokenizer と安全マージンの決め方。tokenizer 差異による過小見積りは課金に直結する
- streaming 応答での精算タイミング（切断・中断時の予約解放）
