class Greeter {
  greet(who) {
    return format(who);
  }
}

function format(s) {
  return "Hello " + s;
}

function main() {
  new Greeter().greet("x");
}
