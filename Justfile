default:
    @just --list

build:
    cargo build --release

install: build
    mkdir -p ~/.local/bin
    cp target/release/tofu ~/.local/bin/
    @echo "Installed to ~/.local/bin/tofu"

clean:
    cargo clean

dev:
    cargo run

test: build
    echo -e "firefox\nchromium\nterminal\nnvim\ncode\nspotify\ndiscord\nsteam" | ./target/release/tofu
