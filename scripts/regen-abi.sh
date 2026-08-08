#!/usr/bin/env bash

set -euo pipefail

cmd=(cargo run -p tocat-abi --features generate --bin tocat-abi-header)

if ! "${cmd[@]}" -- --check; then
  "${cmd[@]}"
fi
