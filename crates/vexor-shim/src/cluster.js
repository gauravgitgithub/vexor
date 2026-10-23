// Cluster primary for PM2-style cluster mode (docs/architecture.md §6).
//
// Runs on the app's own Node as `node -e <this> <config JSON>`, with the
// shim on the other end of the Node IPC channel. It holds the listening
// sockets that workers share, and otherwise only executes commands: the
// shim keeps one lifecycle state machine per worker and decides when to
// start, stop and restart them.
//
//   shim -> primary: {cmd:'spawn', id, instance}  {cmd:'signal', id, signal}
//                    {cmd:'retire', id, signal}  {cmd:'shutdown'}
//   primary -> shim: {ev:'spawned', id, pid}  {ev:'spawn_failed', id, error}
//                    {ev:'ready', id}  {ev:'listening', id}  {ev:'exit', id, code, signal}
//
// `id` names one worker process; `instance` is the instance number it
// serves (a reload's replacement has a new id and the same instance).
//
// Plain ES5: it must run on every Node version the app may use.

(function () {
  var cluster = require('cluster');
  var config = JSON.parse(process.argv[process.argv.length - 1]);
  var setup = cluster.setupPrimary || cluster.setupMaster;
  setup.call(cluster, {
    exec: config.script,
    args: config.args,
    execArgv: config.node_args,
    silent: false
  });

  var workers = {}; // id -> cluster worker

  function send(msg) {
    if (process.connected) {
      process.send(msg);
    }
  }

  function idOf(worker) {
    for (var id in workers) {
      if (workers[id] === worker) {
        return Number(id);
      }
    }
    return -1;
  }

  cluster.on('message', function (worker, msg) {
    // Older Node passes (msg) only; newer passes (worker, msg).
    if (arguments.length === 1) {
      msg = worker;
      worker = null;
    }
    if (msg === 'ready' && worker) {
      send({ ev: 'ready', id: idOf(worker) });
    }
  });

  cluster.on('listening', function (worker) {
    send({ ev: 'listening', id: idOf(worker) });
  });

  cluster.on('exit', function (worker, code, signal) {
    var id = idOf(worker);
    if (id >= 0) {
      delete workers[id];
      send({ ev: 'exit', id: id, code: code === null ? null : code, signal: signal || null });
    }
  });

  process.on('message', function (msg) {
    if (!msg || typeof msg !== 'object') {
      return;
    }
    if (msg.cmd === 'spawn') {
      var env = {};
      env[config.instance_var] = String(msg.instance);
      try {
        var w = cluster.fork(env);
        workers[msg.id] = w;
        send({ ev: 'spawned', id: msg.id, pid: w.process.pid });
      } catch (e) {
        send({ ev: 'spawn_failed', id: msg.id, error: String((e && e.message) || e) });
      }
    } else if (msg.cmd === 'signal') {
      var target = workers[msg.id];
      if (target) {
        try {
          target.process.kill(msg.signal);
        } catch (e) {
          // Already gone; its exit event follows.
        }
      }
    } else if (msg.cmd === 'retire') {
      // Graceful retirement (reload): stop routing new connections to the
      // worker and let in-flight ones finish (worker.disconnect closes its
      // servers), then send the stop signal. The shim sends SIGKILL after
      // kill_timeout.
      var old = workers[msg.id];
      var stopSignal = msg.signal || 'SIGINT';
      if (old) {
        old.once('disconnect', function () {
          try {
            old.process.kill(stopSignal);
          } catch (e) {
            // Already gone.
          }
        });
        try {
          old.disconnect();
        } catch (e) {
          old.process.kill(stopSignal);
        }
      }
    } else if (msg.cmd === 'shutdown') {
      process.exit(0);
    }
  });

  // The shim decides when workers stop; a group-wide SIGTERM or SIGINT must
  // not take the primary (and every worker's connections) down first.
  process.on('SIGTERM', function () {});
  process.on('SIGINT', function () {});

  // If the shim dies, keep serving: as in fork mode, the app outlives its
  // shim, and the daemon refuses to start a duplicate while these processes
  // are alive (docs/architecture.md §7). The listening sockets keep this
  // process running.
})();
