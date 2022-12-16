_default:
    just --list

# Run a WebShell server
run:
    cargo run

# Build the WebShell server
build:
    cargo build

# Format source files
format:
    cargo fmt

tokio-console:
    tokio-console

# Install dependencies
install:
    npm install
    cargo install --locked tokio-console