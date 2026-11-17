class Greeter:
    def greet(self, who):
        return format_name(who)


def format_name(s):
    return "Hello " + s


def main():
    Greeter().greet("x")
