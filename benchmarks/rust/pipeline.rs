// pipeline — see ../praxis/pipeline.px for the description.
use std::io::Read;

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let n: i64 = buf.trim().parse().unwrap();

    let mut v: Vec<i64> = Vec::new();
    let mut i: i64 = 0;
    while i < n {
        v.push((i * 7919 + 13) % 1000003);
        i += 1;
    }

    let mut acc: i64 = 0;
    let mut r: i64 = 0;
    while r < 12 {
        let salt = r;
        acc = (acc
            + v.iter()
                .map(|x| (x * 3 + salt) % 1000003)
                .filter(|x| x % 5 != 0)
                .sum::<i64>())
            % 1000003;
        r += 1;
    }

    let mut folded: i64 = 0;
    let mut s: i64 = 0;
    while s < 12 {
        let salt = s;
        folded = (folded
            + v.iter()
                .enumerate()
                .map(|(idx, x)| (x + idx as i64 * salt) % 1000003)
                .fold(0i64, |a, x| (a * 2 + x) % 1000003))
            % 1000003;
        s += 1;
    }

    println!("{}", v.len());
    println!("{acc}");
    println!("{folded}");
}
