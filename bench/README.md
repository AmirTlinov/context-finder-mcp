# Benchmarks

This folder contains a small benchmark harness for `context` to measure:

- Indexing wall-clock time + max RSS (via `/usr/bin/time`)
- Search latency + max RSS
- Precision@k against a small, user-maintained dataset

## Quick start

```bash
cargo build --release
python3 bench/run.py --profile general --model bge-small
```

## Datasets

- `data/audit_candidates.json` — portable example dataset (tracked)
- `data/audit_candidates.local.json` — local dataset override (gitignored)

The harness defaults to `data/audit_candidates.local.json` if it exists, otherwise it falls back to the example dataset.
