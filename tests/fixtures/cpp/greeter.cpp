#include <string>

std::string format(const std::string &s) { return "Hello " + s; }

class Greeter {
public:
    std::string greet(const std::string &who) {
        return format(who);
    }
};

int main() {
    Greeter g;
    g.greet("x");
    return 0;
}
