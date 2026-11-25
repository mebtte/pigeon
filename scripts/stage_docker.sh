#!/usr/bin/env bash
set -euo pipefail

mkdir -p docker-context/rootfs/data docker-context/rootfs/tmp
: > docker-context/rootfs/data/.keep
: > docker-context/rootfs/tmp/.keep

for architecture in "$@"; do
  case "$architecture" in
    amd64) asset=pigeon-linux-x86_64 ;;
    arm64) asset=pigeon-linux-arm64 ;;
    *) echo "Unsupported Docker architecture: $architecture" >&2; exit 2 ;;
  esac

  mkdir -p "docker-context/linux-$architecture"
  install -m 0555 "release/$asset" "docker-context/linux-$architecture/pigeon"
done
