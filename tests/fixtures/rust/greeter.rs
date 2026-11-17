pub struct Greeter;

impl Greeter {
    pub fn greet(&self, who: &str) -> String {
        format_name(who)
    }
}

fn format_name(s: &str) -> String {
    format!("Hello {s}")
}

fn main() {
    Greeter.greet("x");
}
