# Eval datasets

This folder contains retrieval evaluation datasets used by:

- `context-finder eval`
- `context-finder eval-compare`

Datasets are **offline-friendly** and CI-safe when run in stub mode.

## Format (schema_version = 1)

Each dataset is JSON with:

- `schema_version`: must be `1`
- `name`: optional
- `cases[]`:
  - `id`: stable identifier
  - `query`: query string
  - `expected_paths[]`: one or more repo-relative file paths expected in top-k
  - optional `expected_symbols[]`
  - optional `intent`: `identifier` / `path` / `conceptual`

Notes:

- Datasets are for **positive** retrieval (they must have `expected_paths`).
- Negative behaviors (e.g. “must not return unrelated hits”) are validated via targeted tests.

