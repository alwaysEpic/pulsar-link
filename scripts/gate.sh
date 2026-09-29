#!/usr/bin/env bash
# The gate: run before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# The binary a test runs: `cargo test` builds only the test harness, and a stale
# `target/debug/pulsar-link` was once tested by mistake.
cargo build --workspace
# The docs gate is a maintainer tool that the published tree does not carry: it runs
# where its script is, and the skip is said where it is not.
if [[ -f scripts/check_docs.py ]]; then
  python3 scripts/check_docs.py
else
  echo "docs gate skipped: scripts/check_docs.py is not in this tree"
fi
