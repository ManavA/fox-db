namespace Demo
{
    public class Greeter
    {
        public string Greet(string who)
        {
            return Format(who);
        }

        static string Format(string s) => "Hello " + s;

        public static void Main()
        {
            new Greeter().Greet("x");
        }
    }
}
