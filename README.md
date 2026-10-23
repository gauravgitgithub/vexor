# Vexor

A Rust-native process manager for production Node.js apps, designed as a
PM2-compatible alternative.

> **Status: prerelease (0.1.1).** Usable for evaluation, not yet recommended
> for production: PM2 migration, memory/watch/cron restarts and published
> benchmarks are still missing. See [CHANGELOG.md](CHANGELOG.md) and
> [docs/plan.md](docs/plan.md).

## Install

```sh
npm install -g vexord        # installs the `vexor` command (launcher needs Node >= 10)
```

or, without npm or Node.js (checks the SHA-256 checksum before installing):

```sh
curl -fsSL https://raw.githubusercontent.com/gauravgitgithub/vexor/main/install.sh | sh
```

Prebuilt for Linux x64/arm64 (static, any distribution) and macOS arm64/x64.
The apps vexor runs can use any Node.js version; Node 8 through 24 are tested.

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
