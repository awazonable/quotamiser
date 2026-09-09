# ADR-0001: tokenmiser を fork せず選択移植する

- Status: Accepted
- Date: 2026-09-10

## Context

QuotaMiser は OpenAI 互換の proxy であり、その表面・SSE 中継・provider アダプタ・ルータは既存の実装と大きく重なる。`openintelligence-labs/tokenmiser`（Rust、MIT）がそれらを備えている。

一方、本製品の中核である**送信前の見積り → 予約 → 精算による fail-closed な admission control** は上流に存在しない。上流の予算機構は**事後記録・USD 建て・プロセス内メモリ**であり、本製品が必要とする**送信前・トークン建て・永続**とは設計が別物である。

この重なり方をどう扱うかは、要件や設計から一意に導けない。上流追従の利益と、無関係な変更を取り込み続けるコストの比較になる。

## Decision

**fork せず、必要な部分だけを選択的に移植する。**

移植するのは OpenAI 互換の HTTP 表面、SSE の中継、provider アダプタの骨格、ルータの骨格。admission control 一式（見積り、予約台帳、精算、受理範囲の allowlist、安全入力の検証、安全ラッチ）は新規に書く。

移植した時点で、上流の著作権表示と vendoring 元の commit SHA を `LICENSE` と `README.md` に記録する。

## Rationale

中核が別物である以上、上流追従で得られるものは周辺部の保守だけである。一方 fork すれば、本製品が使わない予算機構・コスト最適化機能の変更を継続的に取り込むことになる。本製品はそれらを**非目標として明示的に排除している**（安いモデルへの誘導はしない、有料経路を持たない）ため、上流の主要な進化方向と製品の方向が食い違う。

利益がコストを下回ると判断した。

## Alternatives considered

**fork して上流を追う。** 周辺部の改善を自動的に受け取れる。しかし admission control を上流の予算機構と共存させるか置き換えるかという問題が恒久的に残り、マージのたびに再燃する。上流が有料経路の最適化を進めるほど、本製品の「有料経路を持たない」という制約との衝突が増える。

**ゼロから書く。** 依存が無く方向の衝突も無いが、OpenAI 互換表面と SSE 中継は本製品の差別化要素ではなく、既存の正しい実装を書き直す理由が無い。

## Consequences

- 上流のバグ修正は自動的には入らない。移植した部分に問題が見つかった場合、上流を確認して手で取り込む。
- 移植部分の出所を追跡できるようにする責任が生じる。commit SHA の記録がその手段である。
- MIT ライセンスの表示義務を果たす必要がある。

## References

- [`../requirements/requirements.md`](../requirements/requirements.md) — 非目標の節（有料 API のコスト最適化を目的としない）
- [`../design/design.md`](../design/design.md) — 出発点の節（移植する範囲と新規に書く範囲）
- `README.md` — Attribution の節
