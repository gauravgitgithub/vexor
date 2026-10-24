# Vexor demo: `vhs docs/demo/demo.tape` from the repository root, after
# `cargo build --release` (it uses target/release/vexor).
Output docs/demo/demo-vexor.gif

Set Shell "zsh"
Set FontSize 16
Set Width 1180
Set Height 660
Set Padding 18
Set TypingSpeed 30ms
Set Theme {"background": "#0d1117", "foreground": "#e6edf3", "cursor": "#58a6ff", "black": "#484f58", "green": "#3fb950", "yellow": "#d29922", "red": "#f85149", "blue": "#58a6ff", "magenta": "#bc8cff", "cyan": "#39c5cf", "white": "#e6edf3", "brightBlack": "#6e7681", "brightGreen": "#56d364", "brightYellow": "#e3b341", "brightRed": "#ff7b72", "brightBlue": "#79c0ff", "brightMagenta": "#d2a8ff", "brightCyan": "#56d4dd", "brightWhite": "#ffffff"}

Hide
Type "setopt interactivecomments nomonitor; export PATH=$PWD/target/release:$PATH VEXOR_HOME=$(mktemp -d /tmp/vxdemo.XXXX) PS1='%F{blue}$%f '; cd docs/demo; clear"
Enter
Show

Type "# A Node.js app in cluster mode: 4 workers share port 3000"
Enter
Type "vexor start app.js -i 4 --name api"
Enter
Sleep 2.5s
Type "curl -s localhost:3000; curl -s localhost:3000"
Enter
Sleep 2s

Hide
Type "(while :; do curl -s localhost:3000 >/dev/null; done) &>/dev/null & LOAD=$!; clear"
Enter
Show

Type "# Crash a worker: only that one restarts, the others keep serving"
Enter
Type "kill -9 $(curl -s localhost:3000 | awk '{print $4}') && sleep 1.5 && vexor ls"
Enter
Sleep 3.5s

Hide
Type "clear"
Enter
Show

Type "# Zero-downtime reload: new workers must be ready before old ones retire"
Enter
Type "vexor reload api"
Enter
Sleep 3s
Type "# Scale out: only the difference starts"
Enter
Type "vexor scale api +2"
Enter
Sleep 3s

Type "clear; vexor monit"
Enter
Sleep 7s
Ctrl+C
Sleep 300ms

Hide
Type "clear"
Enter
Show

Type "# What's wrong with this setup, and how to fix it"
Enter
Type "vexor doctor"
Enter
Sleep 4s

Hide
Type "kill $LOAD; vexor kill >/dev/null; rm -rf $VEXOR_HOME"
Enter
