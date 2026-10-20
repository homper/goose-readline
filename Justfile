default := just-file()

# Build the client.
build:
    cargo build

# Run the unit tests (editor, rendering, permissions, usage).
test:
    cargo test --lib

# Run the headless ACP integration test (needs Z.AI key + network).
integration:
    cargo test --test acp_integration -- --nocapture

# Run the client in this directory.
run:
    cargo run

# Compile warnings as errors.
lint:
    cargo clippy --all-targets -- -D warnings

# Update the goose path dependency's sources (pull + rebuild).
refresh-goose:
    cd ../goose && git pull --ff-only
    cargo update -p goose

# Show this help.
default:
    @just --list
