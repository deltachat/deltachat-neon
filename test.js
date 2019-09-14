const { Context } = require('./lib');

let ctx = new Context((event, data) => {
  console.log(`[${event}] ${data}`);
});

console.log("opening database");
ctx.open("test-db");

console.log("connecting");
ctx.connect();

setTimeout(() => {
  console.log("disconnecting")
  ctx.disconnect();
}, 200);
