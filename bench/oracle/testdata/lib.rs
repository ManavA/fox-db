pub fn target(a: u8, _oracle_extra: f64) -> u8 {
    a
}

pub fn users() -> u8 {
    let x = target(1);
    let f: fn(u8) -> u8 = target;
    let v: Vec<u8> = vec![1].into_iter().map(target).collect();
    x + f(2) + v[0] + S::m(&S, target(3))
}

pub struct S;
impl S {
    pub fn m(&self, a: u8) -> u8 {
        a
    }
}

macro_rules! twice {
    ($e:expr) => {
        target($e) + target($e)
    };
}

pub fn more() -> u8 {
    let a = std::cmp::max(
        target(4),
        1,
    );
    let g = target;
    a + twice!(5) + other(1)
}

fn other(a: u8, b: u8) -> u8 {
    a + b
}
