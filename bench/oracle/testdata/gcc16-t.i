# 0 "t.c"
# 0 "<built-in>"
# 0 "<command-line>"
# 1 "/usr/include/stdc-predef.h" 1 3
# 0 "<command-line>" 2
# 1 "t.c"
# 1 "t.h" 1
int target(int a, double oracle_extra);

static inline int in_header(void) { return target(1); }
# 2 "t.c" 2
typedef int (*fn)(int);
struct ops { fn f; };
static struct ops table[] = {
 { target },
};
static fn table2 = target;
static void take(fn f) { (void)f; }
int user1(void) { return target(2); }
int user2(void) { fn p; p = target; take(target); return target(3); }
fn user3(void) { return target; }
int user4(void) { return target == table2; }

int user5(void)
{
 return ((target(5)) > 0)
            ;
}



int user6(void) { return 0; }

int other(int a, int b) { return a + b; }
int user7(void) { return other(1); }
int target(int a) { return a; }
