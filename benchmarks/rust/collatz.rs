// collatz — see ../praxis/collatz.px for the description.
use std::io::Read;

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let limit: i64 = buf.trim().parse().unwrap();

    let mut best_n: i64 = 0;
    let mut best_len: i64 = 0;
    let mut n: i64 = 1;
    while n <= limit {
        let mut c = n;
        let mut steps: i64 = 0;
        while c != 1 {
            if c % 2 == 0 {
                c /= 2;
            } else {
                c = 3 * c + 1;
            }
            steps += 1;
        }
        if steps > best_len {
            best_len = steps;
            best_n = n;
        }
        n += 1;
    }

    println!("{best_n}");
    println!("{best_len}");
}
