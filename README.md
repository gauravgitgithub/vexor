# Vexor

A Rust-native process manager for production Node.js apps, designed as a
PM2-compatible alternative.

> **Status: pre-alpha.** Only the project skeleton exists. Nothing here is
> usable yet, and none of the features described in `docs/` are implemented.

## Design docs

- [Architecture](docs/architecture.md)
- [Compatibility specification](docs/compatibility.md)
- [Competitive assessment](docs/competitive.md)
- [Threat model](docs/security.md)
- [PM2 migration design](docs/migration.md)
- [Benchmark methodology](docs/benchmarks.md)
- [Phased plan](docs/plan.md)

## Development

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it
automatically.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check all        # needs cargo-deny
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
