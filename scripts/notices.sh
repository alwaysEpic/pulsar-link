#!/usr/bin/env bash
# Write THIRD-PARTY-NOTICES.txt: every dependency's licence and copyright notice, which
# their licences (MIT's among them) require to accompany each copy of the binary. Needs
# cargo-about (`cargo install cargo-about --locked --features cli`).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo about generate --locked --fail -m crates/pulsar-link/Cargo.toml \
  -o THIRD-PARTY-NOTICES.txt scripts/notices.hbs
# A crate with no licence file of its own gets the bare template, copyright line unfilled;
# about.toml clarifies each one from upstream. (The licences' own "how to apply" sections,
# the GPL's "<year> <name of author>" and Apache's "[yyyy]", are text, not a notice.)
if grep -nE '<year> <(owner|copyright holders)>|\[year\] \[fullname\]' THIRD-PARTY-NOTICES.txt; then
  echo "a template notice with no copyright holder: clarify its crate in about.toml" >&2
  exit 1
fi
echo THIRD-PARTY-NOTICES.txt
