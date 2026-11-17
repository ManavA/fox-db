#import <Foundation/Foundation.h>

NSString *format(NSString *s) {
    return s;
}

@interface Greeter : NSObject
- (NSString *)greet:(NSString *)who;
@end

@implementation Greeter
- (NSString *)greet:(NSString *)who {
    return format(who);
}
@end

int main(void) {
    [[Greeter new] greet:@"x"];
    return 0;
}
