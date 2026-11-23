#include "t.h"
typedef int (*fn)(int);
struct ops { fn f; };
static struct ops table[] = {
	{ target },
};
static fn table2 = target;
static void take(fn f) { (void)f; }
int user1(void) { return target(2); }
int user2(void) { fn p; p = target; take(target); return CALL_T(3); }
fn user3(void) { return target; }
int user4(void) { return target == table2; }
int target(int a) { return a; }
