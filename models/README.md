# Models

This directory stores **downloaded** embedding model assets used by Context.

- Binaries under `models/**` are **not committed** to git.
- `models/manifest.json` is the source of truth for what to download and verify.

Common workflow:

```bash
context install-models
context doctor --json
```

The legacy binary alias `context-finder` is also supported.

Optional Python downloader (useful when you prefer `huggingface_hub`):

```bash
python3 -m pip install huggingface_hub
python3 scripts/download_onnx_models.py --list
python3 scripts/download_onnx_models.py --model bge-small
```

v1 roster (see `models/manifest.json` for exact assets + sha256):
- `bge-small`
- `multilingual-e5-small`
- `bge-base`
- `nomic-embed-text-v1`
- `embeddinggemma-300m`
