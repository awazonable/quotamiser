# ADR-0005: 上流への HTTP クライアントは再送とリダイレクト追従を無効化して使う

- Status: Accepted
- Date: 2026-09-11

## Context

設計は、上流へのすべての送信を Dispatcher が管理し、送信の前に `DISPATCHING` を永続化することを求める。要件は、同一 Provider への再試行に新しい予約を求める。

移植元が使う reqwest（調査時点 0.12.28）とその依存は、呼び出し側が指定しなくても次の再送を行う。

- **reqwest の既定リトライ層**: HTTP/2 の GOAWAY（NO_ERROR）と REFUSED_STREAM で、元の送信に加えて最大 2 回再送する。
- **リダイレクト追従**: 最大 10 回。307 / 308 では POST 本文を再送する。認証ヘッダを外すのは別ホストへのリダイレクトのときだけである。
- **hyper-util の canceled request retry**: 再利用したプール接続が死んでいた場合に、1 バイトも書いていない要求を別の接続で送り直す。調査時点の reqwest からは無効化できない。

いずれも台帳が関知しない送信である。

## Decision

**reqwest を使い、リトライ層とリダイレクト追従を無効化する。** hyper-util の canceled request retry は受け入れる。

上流が 3xx を返した場合、**Dispatcher は追従しない。** 経路または設定の異常として当該 Provider を閉じ（永続、明示的な是正を要する）、予約は送信済みとして保持する。

## Rationale

- 既定のリトライとリダイレクト追従は、Dispatcher の外で送信を発生させ、「すべての送信を Dispatcher が管理する」という前提を崩す。プロトコル上は相手が未処理と保証される場合に限られるものが多いが、例外を許すと前提そのものを検証できなくなる。
- hyper-util の再送は、1 バイトも書いていない要求を別の接続へ付け替えるものであり、上流から見て 1 リクエストのままである。台帳の不変条件に影響しない。
- リダイレクトは課金制御とは直接関係しない。しかし OpenAI も OpenRouter も API endpoint は通常リダイレクトしないため、3xx は base URL の誤設定か、利用者と上流の間に何かが介在していることを示す。**3xx を返した相手が上流本体である保証は無く**、元の要求が転送されて処理された可能性を否定できない。したがって追従もせず、予約も解放しない。

## Alternatives considered

**hyper を直接使う。** 隠れた再送を完全に排除できるが、コネクションプール・TLS・HTTP/2 を自前で扱う量が増える。残る hyper-util の再送は安全であるため、得られるものが少ない。

**既定のまま使う。** 前提を検証できなくなるため採らない。

**3xx に Dispatcher が追従する。** 追従先に本文を、同一ホストなら credential も送ることになる。利用者の意図しない宛先への送信を、Dispatcher が自ら行うことになる。

## Consequences

- 上流用の HTTP クライアントを構築する箇所は Dispatcher に 1 つだけとし、無効化の設定をテストで固定する。
- **リダイレクトのテストは Provider ごとに持つ。** OpenAI・OpenRouter・Local のそれぞれについて、ローカルのモックサーバが 307 / 308 を返したときに、(1) 追従先へ 1 件も送られないこと、(2) 当該 Provider が永続的に閉じられること、(3) 予約が送信済みとして保持されること、を確認する。どの Provider も通常はリダイレクトしないが、Provider ごとに異なる経路で Dispatcher に到達するため、1 つの Provider での確認で他を代表させない。
- 依存を更新したときに、既定の再送挙動が変わっていないかを確認する必要がある。
- 3xx の扱いを要件の遷移表に追加した。

## 検証（2026-09-11）

ローカルのモックサーバに対して実測した（reqwest 0.13.5、hyper-util 0.1.20）。

- 既定のクライアントは、HTTP/2 サーバが REFUSED_STREAM を返したとき、1 件の論理的な POST を **3 回**送った。`retry(reqwest::retry::never())` を設定すると 1 回だった。
- 既定のクライアントは 307 / 308 に追従し、追従先へ同一の JSON 本文を POST した。`redirect(reqwest::redirect::Policy::none())` を設定すると 3xx がそのまま返り、追従先へは 0 件だった。
- `.send()` の future は、最初に poll されるまで接続もバイト送出も行わない。
- hyper-util の canceled request retry は 0.13.5 でも有効で、reqwest から無効化する設定は無い。書き込み前に接続の死を検知した場合にだけ別の接続で送り直し、サーバが受け取った要求は 1 件だった。
- **要求を書き込んだ後で接続の死が判明した場合、サーバは要求全体を受け取っていたのに、クライアントは `IncompleteMessage` エラーを受け取り、再送しなかった。** 送信後のエラーは、上流が要求を受け取った可能性を含む「結果不明」として扱わなければならない。要件の、送信後の connection failure で予約を解放しない扱いと一致する。
- `pool_max_idle_per_host(0)` で接続の再利用を止めると canceled request retry の経路は生じないが、要求ごとに接続と TLS ハンドシェイクが要る。上記の通りこの再送は重複を生まないため採用しない。

## References

- [`../requirements/requirements.md`](../requirements/requirements.md) — Fallback の遷移条件
- [`../design/design.md`](../design/design.md) — 予約 capability、DISPATCHING の永続化と送信の順序
- [ADR-0001](0001-selective-port-from-tokenmiser.md)（改訂）、[ADR-0004](0004-axum-http-server.md)
