# Vexor

A Rust-native process manager for production Node.js apps, designed as a
PM2-compatible alternative.

> **Status: prerelease (0.3.0).** Usable for evaluation, not yet recommended
> for production: no published benchmarks yet, and a few PM2 settings
> (`uid`/`gid`, `increment_var`, `shutdown_with_message`) are not supported.
> See [CHANGELOG.md](CHANGELOG.md) and [docs/plan.md](docs/plan.md).

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

## Quick start

```sh
vexor start app.js -i 4            # cluster mode, 4 instances
vexor start ecosystem.config.js    # PM2 ecosystem files work as they are
vexor check ecosystem.config.js    # what is supported, before starting
vexor ls
vexor logs app --follow
vexor reload app                   # new processes first, old ones after
vexor startup                      # start at boot (systemd, launchd)
```

## Moving from PM2

```sh
pm2 save                                   # so PM2's saved list is current
vexor migrate --from-pm2                   # dry run: the plan, changes nothing
vexor migrate --from-pm2 --backup          # copy PM2's state and the plan
vexor migrate --from-pm2 --apply           # move apps one at a time
vexor migrate --status
vexor migrate --rollback                   # move them back to PM2
```

Each app is stopped under PM2 and started under Vexor, so expect a gap of
about its startup time; an app Vexor can't run healthily goes back to PM2.
Then run `pm2 unstartup` and `vexor startup`. Details:
[docs/migration.md](docs/migration.md).

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
