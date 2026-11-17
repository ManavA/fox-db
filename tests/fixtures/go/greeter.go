package demo

type Greeter struct{}

func (g *Greeter) Greet(who string) string {
	return format(who)
}

func format(s string) string {
	return "Hello " + s
}

func main() {
	g := &Greeter{}
	g.Greet("x")
}
