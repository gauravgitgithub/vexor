# vexord

**Pre-alpha.** Rust-native process manager for Node.js apps, aiming for PM2
compatibility. This package installs the `vexor` command.

```sh
npm install -g vexord
vexor start app.js -i 2
vexor list
```

npm installs the prebuilt binary for your platform as an optional dependency
(`vexord-linux-x64`, `vexord-linux-arm64`, `vexord-darwin-arm64`,
`vexord-darwin-x64`). There are no install scripts and no downloads at install
time. If optional dependencies are disabled, `vexor` tells you how to fix it.

The launcher needs Node.js 10 or newer. The apps you run with vexor can use
any Node.js version, including older ones, and vexor itself does not need
Node.js: on machines without a suitable Node, install the binary directly with
`install.sh` from the repository.

Documentation, compatibility notes and source:
https://github.com/gauravgitgithub/vexor

License: MIT OR Apache-2.0.
