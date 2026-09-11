# ADR-0004: HTTP サーバは axum で新規に書く

- Status: Accepted
- Date: 2026-09-11

## Context

移植元の tokenmiser は Pingora 上に構築され、すべてのリクエストを `request_filter` の中で自前で応答している。本製品は OpenAI 互換の受け口を Responses API の形で提供し、クライアント切断時に上流の background 応答を cancel する必要がある（[ADR-0002](0002-background-dispatch-for-settlement.md)）。

移植元のサーバ基盤を引き継ぐか、別の基盤で書くかは、要件や設計から一意に導けない。

## Decision

**HTTP サーバは axum（hyper 上）で新規に書く。** Pingora は持ち込まない。

## Rationale

- 移植元のハンドラはキャッシュ・予算・cascade・single-flight と絡み合っており、どのみち書き直しが必要である。移植できるのはハンドラの外側の小さな部品に限られる（[ADR-0001](0001-selective-port-from-tokenmiser.md) 改訂）。
- 調査時点の Pingora の非 streaming 経路は、`request_filter` を await する間クライアント切断を監視せず、上流呼び出しが完了するまで走り続ける。
- 移植元は Pingora のプロキシ機能（上流への転送）を使っておらず、すべての応答を自前で組み立てている。中核機能を使わないまま依存だけを抱えることになる。

## Alternatives considered

**Pingora を維持する。** 移植元との差分が小さく見えるが、ハンドラを書き直す以上、実際の差分は小さくならない。

**hyper を直接使う。** 依存は最小になるが、ルーティング・本文抽出・エラー整形を自前で書く量が増える。axum は hyper の上の薄い層であり、hyper の挙動を大きく隠さない。

## Consequences

- クライアント切断時にハンドラの future がいつ drop されるかが、cancel と状態遷移の設計に直結する。送信と状態遷移は取り消し耐性のある監督タスクで行う設計を前提とし、axum での実際の挙動を小さな検証で確認する。
- ハンドラ内のクリーンアップ処理には依存しない。切断後に必要な処理は監督タスクが担う。

## References

- [`../design/design.md`](../design/design.md) — アーキテクチャ、DISPATCHING の永続化と送信の順序
- [ADR-0001](0001-selective-port-from-tokenmiser.md)（改訂）、[ADR-0002](0002-background-dispatch-for-settlement.md)
