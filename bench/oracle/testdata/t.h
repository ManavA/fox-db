int target(int a, double oracle_extra);
#define CALL_T(x) target(x)
static inline int in_header(void) { return target(1); }
