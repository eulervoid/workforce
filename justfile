default:
    @just --list

check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

wf *args:
    cargo run -q -p wf-cli -- {{args}}
