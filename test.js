const { Context } = require('./lib');

let ctx = new Context((val) => {
  console.log("CALLBACK", val);
});

console.log("opening database");
ctx.open("test-db");

console.log("connecting");
ctx.connect();

setTimeout(() => {
  console.log("disconnecting")
  ctx.disconnect();
}, 100);
