package demo;

public class Greeter {
    public String greet(String who) {
        return format(who);
    }

    static String format(String s) {
        return "Hello " + s;
    }

    public static void main(String[] args) {
        new Greeter().greet("x");
    }
}
