"""Retired. Walkman Sync is the walkman-sync binary (no Python runtime)."""
import sys

print("Walkman Sync is now the walkman-sync binary — no Python runtime.", file=sys.stderr)
print("  cargo build --release && target/release/walkman-sync", file=sys.stderr)
sys.exit(1)
