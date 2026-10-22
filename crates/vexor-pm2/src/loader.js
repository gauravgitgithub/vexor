// Loads a PM2 ecosystem file and prints it as JSON on stdout.
//
// Runs on the app's own Node (any version back to 0.12), so: plain ES5, no
// dependencies. Usage: node -e "<this file>" <config path>
//
// Output: {"ok": <config>} or {"error": "<message>"}. Functions and other
// values JSON can't represent are reported, not silently dropped.

(function () {
  var path = require('path');
  var file = path.resolve(process.argv[process.argv.length - 1]);

  function out(obj) {
    process.stdout.write(JSON.stringify(obj) + '\n');
  }

  function fail(message) {
    out({ error: message });
    process.exitCode = 1;
  }

  // Replace values JSON can't carry with a marker the translator reports.
  function clean(value, where) {
    if (typeof value === 'function') {
      return { __vexor_unsupported__: 'function', at: where };
    }
    if (typeof value === 'undefined') {
      return null;
    }
    if (value instanceof RegExp) {
      return { __vexor_unsupported__: 'regexp', at: where };
    }
    if (Array.isArray(value)) {
      return value.map(function (v, i) {
        return clean(v, where + '[' + i + ']');
      });
    }
    if (value && typeof value === 'object') {
      var result = {};
      Object.keys(value).forEach(function (k) {
        result[k] = clean(value[k], where ? where + '.' + k : k);
      });
      return result;
    }
    return value;
  }

  function done(config) {
    if (config && config.__esModule && config['default']) {
      config = config['default'];
    }
    try {
      out({ ok: clean(config, '') });
    } catch (e) {
      fail('cannot serialize the configuration: ' + e.message);
    }
  }

  process.chdir(path.dirname(file));
  if (/\.mjs$/.test(file)) {
    var importer;
    try {
      // Built at run time so old Node doesn't reject `import(` while parsing.
      importer = new Function('p', 'return import(p)');
    } catch (e) {
      return fail('this Node version (' + process.version + ') cannot load ES modules (.mjs)');
    }
    var url = require('url');
    var target = url.pathToFileURL ? url.pathToFileURL(file).href : 'file://' + file;
    importer(target).then(
      function (mod) {
        done(mod['default'] || mod);
      },
      function (e) {
        fail((e && e.stack) || String(e));
      }
    );
    return;
  }
  try {
    done(require(file));
  } catch (e) {
    fail((e && e.stack) || String(e));
  }
})();
