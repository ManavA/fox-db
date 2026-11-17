class Greeter {
  String greet(String who) {
    return format(who);
  }
}

String format(String s) {
  return 'Hello $s';
}

void main() {
  Greeter().greet('x');
}
