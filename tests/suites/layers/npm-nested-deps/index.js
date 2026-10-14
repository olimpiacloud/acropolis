// debug@4.3.4 needs ms@2.1.2, so the installer nests it under node_modules/debug/node_modules/ms: the debug dir is
// both a package and the ancestor of another one, which once put it twice in the node_modules layer.
const nested = require("debug/node_modules/ms/package.json").version;
const top = require("ms/package.json").version;
console.log(`nested ms ${nested}, top ms ${top}`);
