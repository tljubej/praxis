// tree — see ../praxis/tree.px for the description.
//
// The arena shape is kept, rather than a `Box`ed tree, so all three
// implementations walk the same data structure. Praxis has no self-referring
// types, and choosing a different structure here would measure a different
// program.
use std::io::Read;

#[derive(Clone, Copy)]
struct Node {
    l: i64,
    r: i64,
    v: i64,
}

fn build(ns: &mut Vec<Node>, depth: i64, v: i64) -> i64 {
    if depth == 0 {
        ns.push(Node { l: -1, r: -1, v });
        return ns.len() as i64 - 1;
    }
    let li = build(ns, depth - 1, (v * 2 + 1) % 1000003);
    let ri = build(ns, depth - 1, (v * 2 + 2) % 1000003);
    ns.push(Node { l: li, r: ri, v });
    ns.len() as i64 - 1
}

fn walk(ns: &Vec<Node>, i: i64, salt: i64) -> i64 {
    let node = ns[i as usize];
    if node.l < 0 {
        (node.v + salt) % 1000003
    } else {
        (node.v + walk(ns, node.l, salt) + walk(ns, node.r, salt) * 3) % 1000003
    }
}

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let reps: i64 = buf.trim().parse().unwrap();
    let depth: i64 = 16;

    let mut nodes: Vec<Node> = Vec::new();
    let root = build(&mut nodes, depth, 1);

    let mut acc: i64 = 0;
    let mut r: i64 = 0;
    while r < reps {
        acc = (acc + walk(&nodes, root, r)) % 1000003;
        r += 1;
    }

    println!("{}", nodes.len());
    println!("{acc}");
}
