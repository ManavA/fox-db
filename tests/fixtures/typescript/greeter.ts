export class Greeter {
  greet(who: string): string {
    return format(who);
  }
}

function format(s: string): string {
  return `Hello ${s}`;
}

export function main(): void {
  new Greeter().greet("x");
}
