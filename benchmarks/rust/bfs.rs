// bfs — see ../praxis/bfs.px for the description.
//
// `Vec<bool>` is Rust's idiomatic dense visited mark, the way `BitSet` is
// Praxis's and `bytearray` is Python's. Everything else — the adjacency lists,
// the level-synchronous two-deque walk, the checksums — is the same algorithm
// on the same graph.
use std::collections::VecDeque;
use std::io::Read;

fn open_cell(x: i64, y: i64) -> bool {
    (x * 31 + y * 17 + 5) % 11 != 0
}

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let runs: i64 = buf.trim().parse().unwrap();
    let w: i64 = 320;
    let h: i64 = 320;

    let mut adj: Vec<Vec<i64>> = Vec::new();
    let mut y: i64 = 0;
    while y < h {
        let mut x: i64 = 0;
        while x < w {
            let mut nb: Vec<i64> = Vec::new();
            if open_cell(x, y) {
                if x > 0 && open_cell(x - 1, y) {
                    nb.push(y * w + x - 1);
                }
                if x + 1 < w && open_cell(x + 1, y) {
                    nb.push(y * w + x + 1);
                }
                if y > 0 && open_cell(x, y - 1) {
                    nb.push((y - 1) * w + x);
                }
                if y + 1 < h && open_cell(x, y + 1) {
                    nb.push((y + 1) * w + x);
                }
            }
            adj.push(nb);
            x += 1;
        }
        y += 1;
    }

    let mut open_count: i64 = 0;
    let mut c: i64 = 0;
    while c < w * h {
        if !adj[c as usize].is_empty() {
            open_count += 1;
        }
        c += 1;
    }

    let mut reached_total: i64 = 0;
    let mut dist_total: i64 = 0;
    let mut run: i64 = 0;
    while run < runs {
        let mut start = (run * 7919) % (w * h);
        while adj[start as usize].is_empty() {
            start = (start + 1) % (w * h);
        }

        let mut visited: Vec<bool> = vec![false; (w * h) as usize];
        visited[start as usize] = true;
        let mut current: VecDeque<i64> = VecDeque::new();
        current.push_back(start);
        let mut dist: i64 = 0;

        while !current.is_empty() {
            let mut next: VecDeque<i64> = VecDeque::new();
            while let Some(node) = current.pop_front() {
                reached_total += 1;
                dist_total = (dist_total + dist) % 1000003;
                for &nb in &adj[node as usize] {
                    if !visited[nb as usize] {
                        visited[nb as usize] = true;
                        next.push_back(nb);
                    }
                }
            }
            current = next;
            dist += 1;
        }
        run += 1;
    }

    println!("{open_count}");
    println!("{reached_total}");
    println!("{dist_total}");
}
