#include <stdio.h>

struct greeter { int n; };

static const char *format_name(const char *s) {
    return s;
}

const char *greet(struct greeter *g, const char *who) {
    return format_name(who);
}

int main(void) {
    struct greeter g;
    puts(greet(&g, "x"));
    return 0;
}
