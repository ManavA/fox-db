class Greeter {
    func greet(who: String) -> String {
        return format(who)
    }
}

func format(_ s: String) -> String {
    return "Hello \(s)"
}

func main() {
    Greeter().greet(who: "x")
}
