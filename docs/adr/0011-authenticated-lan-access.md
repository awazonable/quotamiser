# ADR-0011: 認証と接続元制限を伴う LAN 接続を許可する

- Status: Accepted
- Date: 2026-10-03

## Context

従来の v1 は単一テナント、認証なし、loopback のみを前提にしていた。同じ利用者の別端末から LAN 経由で利用できるよう、この前提を変更する。非 loopback の bind を許可するだけでは、到達できる端末が同じ無料枠を消費でき、入力 token 照会や大きな本文でも資源を占有できる。

変更前に origin を取得して確認した。リモート `impl/ledger-core` とローカル HEAD は `da15422` で一致し、リモート `main` は PR #1 の merge commit `9b72f46` だった。両者の tree に差分はない。ローカル `main` は `db281ce` のままであり、リモートに対し 22 commit 遅れている。

作業ツリーには ADR-0010 の短い同期応答経路に関する未コミット変更がある。本決定はその変更を保持し、同期・background の精算方針を変えない。実装の `Config::resolve` は非 loopback を拒否し、HTTP router は認証を持たない。ローカル設定の bind は `127.0.0.1:8787` と `127.0.0.1:8788` で、確認時に QuotaMiser プロセスと 8787 番の listener は無かった。

## Decision

- 既定は従来の認証不要の loopback 接続とし、`allow_lan = true` で LAN 接続を明示的に有効化する。
- LAN モードでは上流 key と別の共有 Bearer token と private CIDR の接続元制限を必須にする。全 API route で、本文読込・token 照会・予約・fallback より前に検証する。同じ listener の loopback 接続にも認証を要求する。
- 接続元は TCP peer で判断し、forwarded ヘッダを信用しない。wildcard bind でも CIDR 制限を維持する。ブラウザ Origin を拒否し、CORS と cookie 認証は提供しない。
- 平文 HTTP は信頼する隔離 LAN に限定する。HTTPS は同じホストの reverse proxy で終端し、その場合の LAN 接続元制限は proxy と firewall が担当する。QuotaMiser 側でも Bearer token を検証する。
- 同一利用者の複数端末は単一の台帳・無料枠を共有する。マルチテナント、ユーザー別 quota、公開インターネット向け運用は追加しない。

設定検証、middleware、資源上限、移行と検証手順の具体化は[設計の LAN 接続節](../design/design.md#lan-接続の境界adr-0011)に置く。

## Rationale

loopback の既定値は既存のローカル利用を維持する。Bearer token は OpenAI 互換クライアントが送れるヘッダであり、上流 credential を別端末に配布せずに利用できる。接続元制限と明示的な有効化を組み合わせ、bind の変更だけで無認証の公開が成立することを防ぐ。

認証を admission より前に置くことで、拒否した接続は入力照会も予約も発生させない。既存の quota 保護と LAN のアクセス制御はそれぞれの境界で成立させる。

## Alternatives considered

**無認証のまま private address / wildcard bind を許可する。** LAN 上の全端末に無料枠の利用を許すことになり、採用しない。

**loopback のまま reverse proxy のみで LAN 接続する。** TLS 終端には採用するが、直接 LAN 接続の選択肢とアプリ側の認証を備える方針にする。

**ユーザーごとの key、権限、台帳を導入する。** 単一利用者の複数端末という目的を超えるため採用しない。

## Consequences

- LAN の利用者には専用 token の生成・配布・更新と firewall の設定が必要になる。token が漏れれば、許可された接続元から同じ quota を使われうる。
- Bearer 認証は通信を暗号化しない。隔離 LAN の前提が成立しない場合は HTTPS を使う。TLS reverse proxy を使う場合はその接続元制限と SSE 設定も運用上の境界になる。
- 認証成功した複数端末に公平性は保証しない。接続数と本文受信期限を制限しても、意図的な LAN 内のサービス妨害を完全には防がない。
- 本 ADR のアクセス制御を実装し、設定例を更新する。既存の実行設定は loopback のままとし、運用者が token・接続元範囲・firewall を設定して LAN を有効にする。
- 実装時に既存の axum 受け口が HTTP/1.1 の構成であることを確認した。LAN でも HTTP/1.1 を維持し、HTTP/2 は受け付けない。TLS reverse proxy の backend 接続も HTTP/1.1 にする。

## References

- [要件](../requirements/requirements.md#lan-からの接続)
- [設計](../design/design.md#lan-接続の境界adr-0011)
- [ADR-0004: axum HTTP サーバ](0004-axum-http-server.md)
- [ADR-0010: 短い同期応答](0010-bounded-synchronous-responses.md)
