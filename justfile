build:
  cargo build

check:
  cargo test
  cargo fmt
  cargo clippy

# Install to a stable, host-local path with its runtime closure pinned as a GC root. Claims are
# ended, and board locks re-checked, by `at` jobs that run fpgahog again later, so it must not
# live in target/.
install:
  sudo mkdir -p /var/lib/fpgahog
  nix build .#default --no-link --print-out-paths | tail -1 | xargs -I{} sudo nix-store --add-root /var/lib/fpgahog/pkg --realise {}
