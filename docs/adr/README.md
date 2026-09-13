# Architecture Decision Records

Use ADRs for material decisions that require an explicit choice, especially when adversarial design review exposes alternatives that cannot be resolved from existing requirements or design principles.

## When to create an ADR

Create an ADR when a decision:

- materially affects architecture or long-term maintainability;
- resolves a requirement/design conflict;
- selects among meaningful alternatives with lasting consequences;
- requires human judgment rather than routine engineering discretion.

Do not create ADRs for ordinary local coding decisions.

## Naming

Use monotonically increasing identifiers, for example:

```text
0001-use-postgresql.md
0002-event-delivery-semantics.md
```

Copy `_template.md` when creating a new ADR.
