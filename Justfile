# Print help message
help:
    @just --list --unsorted
    @echo ""
    @echo "Available variables and default values:"
    @just --evaluate

# Generate Just tab completions for Bash shell
bash-completions:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="$HOME/.local/share/bash-completion/completions"
    mkdir -p "$dir"
    just --completions bash > "$dir/just"

# Build `stacks-node` binary
build *args:
    #!/usr/bin/env bash
    set -euo pipefail
    pushd testnet/stacks-node
    cargo build {{args}}

# Build release version of `stacks-node` binary
build-release: (build "--features" "monitoring_prom,slog_json" "--release")

# Wrapper around `cargo test`
test *args:
    cargo test {{args}}

# Run `cargo fmt` with CLI options not available in rustfmt.toml
fmt:
	cargo fmt -- --config group_imports=StdExternalCrate,imports_granularity=Module
