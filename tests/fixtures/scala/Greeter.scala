class Greeter {
  def greet(who: String): String = format(who)
}

object Main {
  def main(args: Array[String]): Unit = {
    new Greeter().greet("x")
  }
}

def format(s: String): String = s"Hello $s"
