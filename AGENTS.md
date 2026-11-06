# Working in this repository

## Fast feedback
- `cargo test` runs everything in about ten seconds: rail parsers and writers, routing, names and
  screening, the ledger, the lifecycle, the chain indexer, whole simulated months, determinism,
  duplicate delivery and the property tests. The Postgres and API test runs when
  `PIE_DATABASE_URL` is set (CI starts a database); without it, it says so and skips.
- `cargo run -- evaluate` runs the dev worlds with the engine and the baseline and rewrites
  `reports/results.json`; `cargo run -- report` regenerates `reports/REPORT.md` and
  `reports/dashboard.html`; `cargo run -- replay-pg --db URL` writes a month into Postgres and
  rebuilds the engine from it.
- `cargo run --example inspect -- engine <Kind>` lists dev-world deposits of one planted kind and what
  the engine did with them.

## Invariants
- **Nothing but the simulation and the evaluation reads the truth.** `src/sim/` and `src/eval.rs`
  only; `tests/isolation.rs` fails if engine code mentions it. The simulated operations desk decides
  holds from the truth because it plays the people; the engine never sees why it decided.
- **Exact money.** Integer minor units and token base units; parsing refuses to round.
- **Every journal balances and the reserve covers the tokens**, after every journal: the ledger
  refuses otherwise, and so does Postgres (`migrations/001_schema.sql`).
- **The event log is the record.** Inputs are appended with the journals they produced, in one
  transaction; memory is rebuilt from the log. Events, journals and postings are append-only.
- **Never credit the wrong customer.** A reference is repaired only when it is one edit from exactly
  one live reference, judged on the whole word, and the sender is the customer. Everything else goes
  back to the sender.
- **People decide holds.** The engine holds; it never rejects on a name score. A decision carries the
  name of the person who made it.
- **Settings live in `config/`**: thresholds, windows, confirmations, name variants. The baseline is
  `config/baseline.json`, not a code path.
- **Reports are generated.** Do not hand-edit `reports/`.

## The held-out rule
`config/world_heldout.json` is pinned by `eval/heldout/v1.sha256` and was written before the engine
was run on it. `cargo run -- heldout` records its first run once in `reports/heldout/v1/first_run.json`
and refuses to overwrite it; `--rerun` writes `latest.json`. Never change the engine or its settings
to do better on a held-out world, and never inspect its individual failures. If you do, write
`regression` into `eval/heldout/v1.STATUS` and have someone who has not seen the code write a new one.
