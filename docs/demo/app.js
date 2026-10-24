// Demo app for docs/demo/demo.tape: answers with its worker's pid, burns
// a little CPU per request, and logs every 300th request, so `vexor monit`
// has something to show.
let served = 0;
require('http')
  .createServer((req, res) => {
    const t = Date.now();
    while (Date.now() - t < 2);
    if (++served % 300 === 0) console.log(`worker ${process.pid}: ${served} requests served`);
    res.end(`hello from pid ${process.pid}\n`);
  })
  .listen(3000);
