#!/usr/bin/env python3
"""
Download Context embedding model assets using models/manifest.json.

This script is an optional convenience for end users. The CLI equivalent is:
  context install-models

Examples:
  python scripts/download_onnx_models.py --list
  python scripts/download_onnx_models.py --model bge-small
  python scripts/download_onnx_models.py --all

Notes:
  - Defaults to downloading into ./models (or $CONTEXT_MODEL_DIR).
  - For HuggingFace sources, requires: python -m pip install huggingface_hub
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


@dataclass(frozen=True)
class Asset:
    local_rel_path: str
    sha256: str
    source: dict[str, Any]


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def default_model_dir(root: Path) -> Path:
    env = os.environ.get("CONTEXT_MODEL_DIR")
    if env:
        return Path(env)
    return root / "models"


def safe_join(base: Path, rel: str) -> Path:
    rel_path = Path(rel)
    if rel_path.is_absolute() or ".." in rel_path.parts:
        raise ValueError(
            f"asset path must be relative and must not contain '..': {rel}"
        )

    base_resolved = base.resolve()
    full = (base_resolved / rel_path).resolve()
    try:
        full.relative_to(base_resolved)
    except ValueError as exc:
        raise ValueError(f"asset path escapes model dir: {rel}") from exc
    return full


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download_url(url: str, out_path: Path, expected_sha256: str | None) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(url) as resp:  # noqa: S310
        with out_path.open("wb") as fh:
            digest = hashlib.sha256()
            while True:
                chunk = resp.read(1024 * 1024)
                if not chunk:
                    break
                fh.write(chunk)
                digest.update(chunk)
    if (
        expected_sha256 is not None
        and expected_sha256
        and digest.hexdigest() != expected_sha256
    ):
        raise ValueError(f"sha256 mismatch for {url}")


def try_import_hf() -> Any:
    try:
        from huggingface_hub import hf_hub_download  # type: ignore

        return hf_hub_download
    except ModuleNotFoundError:
        print(
            "Missing dependency: huggingface_hub.\n"
            "Install it with:\n"
            "  python -m pip install huggingface_hub\n",
            file=sys.stderr,
        )
        raise


def download_huggingface(
    repo_id: str,
    revision: str,
    filename: str,
    out_path: Path,
    expected_sha256: str | None,
) -> None:
    hf_hub_download = try_import_hf()
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="context-models-") as tmp:
        downloaded = Path(
            hf_hub_download(
                repo_id=repo_id,
                revision=revision,
                filename=filename,
                local_dir=tmp,
                local_dir_use_symlinks=False,
            )
        )
        tmp_local = out_path.with_name(out_path.name + ".tmp")
        shutil.copy2(downloaded, tmp_local)
        if expected_sha256:
            actual = sha256_file(tmp_local)
            if actual != expected_sha256:
                tmp_local.unlink(missing_ok=True)
                raise ValueError(
                    f"sha256 mismatch for {repo_id}@{revision}:{filename} (expected {expected_sha256}, got {actual})"
                )
        os.replace(tmp_local, out_path)


def load_manifest(path: Path) -> dict[str, Any]:
    raw = path.read_text(encoding="utf-8")
    manifest = json.loads(raw)
    if manifest.get("schema_version") != 1:
        raise ValueError("Unsupported manifest schema_version (expected 1)")
    models = manifest.get("models")
    if not isinstance(models, list):
        raise ValueError("Invalid manifest: missing 'models' list")
    return manifest


def parse_assets(manifest: dict[str, Any]) -> dict[str, list[Asset]]:
    out: dict[str, list[Asset]] = {}
    for model in manifest.get("models", []):
        model_id = model.get("id")
        if not isinstance(model_id, str) or not model_id:
            continue
        assets = []
        for asset in model.get("assets", []):
            local_rel = asset.get("path")
            sha = str(asset.get("sha256", "")).strip().lower()
            source = asset.get("source")
            if not isinstance(local_rel, str) or not isinstance(source, dict):
                continue
            assets.append(Asset(local_rel_path=local_rel, sha256=sha, source=source))
        out[model_id] = assets
    return out


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Download Context model assets")
    parser.add_argument(
        "--model-dir",
        type=Path,
        default=None,
        help="Model directory (overrides CONTEXT_MODEL_DIR; defaults to ./models)",
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=None,
        help="Path to models/manifest.json (defaults to <model-dir>/manifest.json)",
    )
    parser.add_argument(
        "--model",
        action="append",
        default=[],
        help="Model id to download (repeatable; defaults to bge-small)",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Download all models from the manifest",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="List models available in the manifest",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Redownload even if files already exist with correct sha256",
    )
    parser.add_argument(
        "--no-verify",
        action="store_true",
        help="Skip sha256 verification (faster, less safe)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    root = repo_root()
    model_dir = args.model_dir or default_model_dir(root)
    manifest_path = args.manifest or (model_dir / "manifest.json")
    if not manifest_path.exists():
        raise FileNotFoundError(
            f"manifest not found: {manifest_path}. "
            "Run from repo root (so ./models/manifest.json exists), "
            "or pass --model-dir/--manifest explicitly."
        )

    manifest = load_manifest(manifest_path)
    models = manifest.get("models", [])

    if args.list:
        for model in models:
            mid = model.get("id")
            desc = model.get("description")
            if isinstance(mid, str):
                suffix = f" — {desc}" if isinstance(desc, str) and desc else ""
                print(f"{mid}{suffix}")
        return 0

    assets_by_model = parse_assets(manifest)
    requested = []
    if args.all:
        requested = sorted(assets_by_model.keys())
    else:
        raw = []
        for item in args.model:
            raw.extend([s.strip() for s in item.split(",") if s.strip()])
        requested = raw or (
            ["bge-small"]
            if "bge-small" in assets_by_model
            else [next(iter(assets_by_model))]
        )

    verify = not args.no_verify

    # If the model_dir differs from the manifest's directory, ensure the manifest exists in model_dir
    # so `context` can use it later with CONTEXT_MODEL_DIR.
    model_dir.mkdir(parents=True, exist_ok=True)
    if (model_dir / "manifest.json") != manifest_path and not (
        model_dir / "manifest.json"
    ).exists():
        shutil.copy2(manifest_path, model_dir / "manifest.json")

    for model_id in requested:
        assets = assets_by_model.get(model_id)
        if not assets:
            print(f"[WARN] unknown model id: {model_id}", file=sys.stderr)
            continue
        for asset in assets:
            local = safe_join(model_dir, asset.local_rel_path)
            expected = asset.sha256 if verify else ""

            if local.exists() and not args.force:
                if not verify or not expected:
                    print(f"[skip] {asset.local_rel_path} (exists)")
                    continue
                actual = sha256_file(local)
                if actual == expected:
                    print(f"[skip] {asset.local_rel_path} (sha256 ok)")
                    continue
                print(f"[re-download] {asset.local_rel_path} (sha256 mismatch)")

            source_type = asset.source.get("type")
            if source_type == "huggingface":
                repo = str(asset.source.get("repo"))
                revision = str(asset.source.get("revision", "main"))
                filename = str(asset.source.get("filename"))
                print(
                    f"[download] {asset.local_rel_path} from hf:{repo}@{revision}:{filename}"
                )
                download_huggingface(repo, revision, filename, local, expected)
                continue
            if source_type == "url":
                url = str(asset.source.get("url"))
                print(f"[download] {asset.local_rel_path} from {url}")
                tmp_local = local.with_name(local.name + ".tmp")
                download_url(url, tmp_local, expected if verify else None)
                os.replace(tmp_local, local)
                continue

            raise ValueError(
                f"Unsupported source type: {source_type!r} for {asset.local_rel_path}"
            )

    print(f"[ok] model_dir={model_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
