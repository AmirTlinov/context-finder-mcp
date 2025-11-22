#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS_DIR="${ROOT_DIR}/.deps/ort_cuda"
WHEEL_DIR="$(mktemp -d)"
ORT_VERSION="${ORT_VERSION:-1.19.0}"
CUDA_PKGS=(
  "nvidia-cublas-cu12"
  "nvidia-cuda-runtime-cu12"
  "nvidia-cudnn-cu12"
  "nvidia-cufft-cu12"
  "nvidia-curand-cu12"
  "nvidia-cusolver-cu12"
  "nvidia-cusparse-cu12"
)

echo "[setup_cuda_deps] target dir: ${DEPS_DIR}"
mkdir -p "${DEPS_DIR}"

echo "[setup_cuda_deps] downloading onnxruntime-gpu==${ORT_VERSION} wheel..."
python3 - <<'PY' "${ORT_VERSION}" "${WHEEL_DIR}"
import subprocess, sys, pathlib, shutil
ver = sys.argv[1]
wheel_dir = pathlib.Path(sys.argv[2])
subprocess.run(
    [
        sys.executable,
        "-m",
        "pip",
        "download",
        "--no-deps",
        f"onnxruntime-gpu=={ver}",
        "-d",
        str(wheel_dir),
    ],
    check=True,
)
PY

WHEEL_PATH="$(ls "${WHEEL_DIR}"/onnxruntime_gpu-*.whl | head -n1)"
if [[ ! -f "${WHEEL_PATH}" ]]; then
  echo "[setup_cuda_deps] wheel not found, aborting" >&2
  exit 1
fi

echo "[setup_cuda_deps] extracting CUDA provider libs from ${WHEEL_PATH}"
python3 - <<'PY' "${WHEEL_PATH}" "${DEPS_DIR}"
import pathlib, sys, zipfile, shutil, os
wheel = pathlib.Path(sys.argv[1])
dest = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(wheel, "r") as zf:
    members = [m for m in zf.namelist() if m.startswith("onnxruntime/capi/") and (m.endswith(".so") or ".so." in m)]
    zf.extractall(dest, members)
    data_members = [m for m in zf.namelist() if m.startswith("onnxruntime/capi/.data/") and (m.endswith(".so") or ".so." in m)]
    zf.extractall(dest, data_members)

# flatten .data/lib if present
data_lib = dest / "onnxruntime" / "capi" / ".data" / "lib"
if data_lib.exists():
    for so in data_lib.glob("*.so*"):
        target = dest / so.name
        if target.exists():
            target.unlink()
        shutil.move(str(so), target)
    shutil.rmtree(data_lib, ignore_errors=True)

# move top-level so files
src_dir = dest / "onnxruntime" / "capi"
for so in src_dir.glob("*.so*"):
    target = dest / so.name
    if target.exists():
        target.unlink()
    shutil.move(str(so), target)

# clean residual structure
shutil.rmtree(dest / "onnxruntime", ignore_errors=True)
PY

echo "[setup_cuda_deps] done. libs:"
ls -1 "${DEPS_DIR}"

echo "[setup_cuda_deps] downloading CUDA dependencies wheels: ${CUDA_PKGS[*]}"
python3 - <<'PY' "${WHEEL_DIR}" "${CUDA_PKGS[@]}"
import subprocess, sys, pathlib
wheel_dir = pathlib.Path(sys.argv[1])
pkgs = sys.argv[2:]
subprocess.run(
    [sys.executable, "-m", "pip", "download", "--no-deps", "-d", str(wheel_dir), *pkgs],
    check=True,
)
PY

echo "[setup_cuda_deps] extracting CUDA shared libraries"
python3 - <<'PY' "${WHEEL_DIR}" "${DEPS_DIR}"
import pathlib, sys, zipfile, shutil
wheel_dir = pathlib.Path(sys.argv[1])
dest = pathlib.Path(sys.argv[2])

for wheel in wheel_dir.glob("*.whl"):
    with zipfile.ZipFile(wheel, "r") as zf:
        members = [m for m in zf.namelist() if ("/lib/" in m or ".so" in m)]
        for m in members:
            name = pathlib.Path(m).name
            if not name:
                continue
            target_path = dest / name
            with zf.open(m) as src, open(target_path, "wb") as dst:
                shutil.copyfileobj(src, dst)
PY

echo "[setup_cuda_deps] final lib set:"
ls -1 "${DEPS_DIR}"

rm -rf "${WHEEL_DIR}"
echo "[setup_cuda_deps] complete"
