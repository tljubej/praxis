// hashwork — see ../praxis/hashwork.px for the description.
//
// `std::collections::HashMap`/`HashSet` with the default SipHash-1-3 hasher,
// because that is exactly what the Praxis runtime's `Map`/`Set`/`Counter` are
// built on (crates/praxis-runtime/src/maps.rs). Swapping in a faster hasher
// here would be measuring a different program.
use std::collections::{HashMap, HashSet};
use std::io::Read;

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let n: i64 = buf.trim().parse().unwrap();

    let mut m: HashMap<i64, i64> = HashMap::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut counts: HashMap<i64, i64> = HashMap::new();

    let mut state: i64 = 12345;
    let mut i: i64 = 0;
    while i < n {
        state = (state * 1103515245 + 12345) % 2147483648;
        let k = state % 65536;
        m.insert(k, i);
        seen.insert(k % 1024);
        *counts.entry(k % 256).or_insert(0) += 1;
        i += 1;
    }

    let mut probe: i64 = 999;
    let mut hits: i64 = 0;
    let mut j: i64 = 0;
    while j < n {
        probe = (probe * 1103515245 + 12345) % 2147483648;
        let k = probe % 98304;
        if m.contains_key(&k) {
            hits += 1;
        }
        j += 1;
    }

    println!("{}", m.len());
    println!("{}", seen.len());
    println!("{}", counts.values().sum::<i64>());
    println!("{hits}");
}
