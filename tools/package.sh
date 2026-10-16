#!/bin/sh
set -eu
repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"
binary=${1:-target/release/oyasys}
test -x "$binary" || { echo "Build first: cargo build --release --locked" >&2; exit 1; }
mkdir -p dist
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/oyasys/deploy" "$stage/oyasys/tools" "$stage/oyasys/docs"
cp "$binary" "$stage/oyasys/oyasys"
cp deploy/oyasys.service deploy/install.sh deploy/environment.example deploy/traefik.example.yaml deploy/update.py deploy/oyasys-update.service deploy/oyasys-update.path "$stage/oyasys/deploy/"
cp tools/profile.py "$stage/oyasys/tools/"
# Explicit public-document allowlist: local notes must never enter a release.
for public_file in \
  docs/api.md docs/architecture.md docs/installation.md docs/performance.md docs/updates.md \
  docs/images/dashboard.png \
  docs/measurements/browser-final.json \
  docs/measurements/cache-comparison-warm.json \
  docs/measurements/cache128-history.json \
  docs/measurements/cache512-history.json \
  docs/measurements/clients-final.json \
  docs/measurements/clients-initial.json \
  docs/measurements/history-contention-final.json \
  docs/measurements/history-contention-initial.json \
  docs/measurements/history-initial.json \
  docs/measurements/idle-final.json \
  docs/measurements/idle-initial.json \
  docs/measurements/storage-encoding.json \
  docs/measurements/update-bridge-idle.json
do
  mkdir -p "$stage/oyasys/$(dirname "$public_file")"
  cp "$public_file" "$stage/oyasys/$public_file"
done
cp README.md DESIGN.md PRODUCT.md CONTEXT.md "$stage/oyasys/"
cp web/vendor/LICENSE-uPlot "$stage/oyasys/"
python3 tools/license_bundle.py "$stage/oyasys/licenses"
tar -czf dist/oyasys-linux-x86_64.tar.gz -C "$stage" oyasys
(cd dist && sha256sum oyasys-linux-x86_64.tar.gz > oyasys-linux-x86_64.tar.gz.sha256)
python3 tools/release_manifest.py "$binary"
echo 'Created dist/oyasys-linux-x86_64.tar.gz (built host libc compatibility applies).'
