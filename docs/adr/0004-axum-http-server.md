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

## 検証（2026-09-11）

ローカルで実測した（axum 0.8.9、hyper 1.11.1、Windows 11）。

- streaming 応答の途中でクライアントが切断すると、応答本文のストリームは約 0.1〜0.2 ms 後に drop された。次の書き込みを待たず、読み取り側で切断を検知している。
- 非 streaming のハンドラが await している間に切断すると、keep-alive の有無や切断の仕方によらず、ハンドラの future は約 0.2〜0.3 ms 後に drop された。**await を挟んだハンドラ内のクリーンアップは実行されない。**
- 例外として、ハンドラが大きなリクエスト本文（64 KiB）を読まずに保持していると切断が検知されず、ハンドラは最後まで走った。
- ハンドラから `tokio::spawn` した監督タスクは切断後も完了まで走り、下流チャネルの閉鎖を検知してから上流へ cancel を送るまで約 1 ms だった。上流の応答ヘッダを待っている間の切断でも同様だった。

これにより次を設計に取り込む。

- 受け口はリクエスト本文を上限付きで**最初にすべて読み切る**。読まずに保持すると切断検知が止まる。
- 送信・cancel・状態遷移は監督タスクが担い、ハンドラには置かない。

未検証: 下流との HTTP/2、TLS 越しの挙動、FIN も RST も送らずに消えるクライアント（半開の接続。タイムアウトで扱う必要がある）、Linux での挙動。

## References

- [`../design/design.md`](../design/design.md) — アーキテクチャ、DISPATCHING の永続化と送信の順序
- [ADR-0001](0001-selective-port-from-tokenmiser.md)（改訂）、[ADR-0002](0002-background-dispatch-for-settlement.md)
