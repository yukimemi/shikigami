#!/usr/bin/env bash
# Regenerate vhs/demo.gif (or another tape passed as $1) with the
# `shikigami-vhs` Docker image built from the sibling Dockerfile.
#
# Run from Linux/macOS, or from WSL on Windows:
#   wsl --cd /mnt/c/Users/<you>/src/github.com/yukimemi/shikigami/vhs -- bash regen.sh
# or via cargo-make, which wraps both cases:
#   cargo make vhs-regen
#
# Requires the image: `docker build -t shikigami-vhs .`
# No credentials of any kind — the tape builds its own jj repo in /tmp
# inside the container.

set -euo pipefail

TAPE="${1:-demo.tape}"

cd "$(dirname "$0")"

if [[ ! -f "$TAPE" ]]; then
    echo "no such tape: $TAPE" >&2
    exit 1
fi

exec docker run --rm -v "$PWD:/vhs" shikigami-vhs "$TAPE"
