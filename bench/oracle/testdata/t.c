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
#define CHECK(x) ((x) > 0)
int user5(void)
{
	return CHECK(
		target(5));
}
#ifdef NOT_DEFINED
int user6(void) { return target(6); }
#else
int user6(void) { return 0; }
#endif
int other(int a, int b) { return a + b; }
int user7(void) { return other(1); }
int target(int a) { return a; }
