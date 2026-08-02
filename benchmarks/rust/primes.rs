// primes — see ../praxis/primes.px for the description.
use std::io::Read;

fn is_prime(n: i64) -> bool {
    if n < 2 {
        return false;
    }
    if n % 2 == 0 {
        return n == 2;
    }
    let mut d: i64 = 3;
    while d * d <= n {
        if n % d == 0 {
            return false;
        }
        d += 2;
    }
    true
}

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let limit: i64 = buf.trim().parse().unwrap();

    let mut count: i64 = 0;
    let mut total: i64 = 0;
    let mut n: i64 = 2;
    while n < limit {
        if is_prime(n) {
            count += 1;
            total += n;
        }
        n += 1;
    }

    println!("{count}");
    println!("{total}");
}
