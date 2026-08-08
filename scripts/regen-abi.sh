#!/usr/bin/env bash

set -euo pipefail

cmd=(cargo run -p tocat-wasm-abi --example tocat-abi-header)

if ! "${cmd[@]}" -- --check; then
  "${cmd[@]}"
fi
