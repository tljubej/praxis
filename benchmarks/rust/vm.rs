// vm — see ../praxis/vm.px for the description.
use std::collections::VecDeque;
use std::io::Read;

#[derive(Clone, Copy)]
enum Op {
    Push(i64),
    Load(i64),
    Store(i64),
    Add,
    Mul,
    Mod,
    Lt,
    JmpZ(i64),
    Jmp(i64),
    Halt,
}

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let limit: i64 = buf.trim().parse().unwrap();

    // The program, hand-assembled. Register 0 is the loop counter, register 1
    // the accumulator; the loop head is instruction 4 and the exit target is 27.
    let mut prog: Vec<Op> = Vec::new();
    prog.push(Op::Push(0)); //  0
    prog.push(Op::Store(0)); //  1   i = 0
    prog.push(Op::Push(1)); //  2
    prog.push(Op::Store(1)); //  3   acc = 1
    prog.push(Op::Load(1)); //  4   <- loop head
    prog.push(Op::Push(31)); //  5
    prog.push(Op::Mul); //  6   acc * 31
    prog.push(Op::Load(0)); //  7
    prog.push(Op::Push(7)); //  8
    prog.push(Op::Mul); //  9   i * 7
    prog.push(Op::Push(13)); // 10
    prog.push(Op::Add); // 11   i * 7 + 13
    prog.push(Op::Push(1000003)); // 12
    prog.push(Op::Mod); // 13   (i * 7 + 13) % 1000003
    prog.push(Op::Add); // 14   acc * 31 + that
    prog.push(Op::Push(1000003)); // 15
    prog.push(Op::Mod); // 16
    prog.push(Op::Store(1)); // 17   acc = ...
    prog.push(Op::Load(0)); // 18
    prog.push(Op::Push(1)); // 19
    prog.push(Op::Add); // 20
    prog.push(Op::Store(0)); // 21   i = i + 1
    prog.push(Op::Load(0)); // 22
    prog.push(Op::Push(limit)); // 23
    prog.push(Op::Lt); // 24   i < limit
    prog.push(Op::JmpZ(27)); // 25
    prog.push(Op::Jmp(4)); // 26
    prog.push(Op::Load(1)); // 27   <- exit
    prog.push(Op::Halt); // 28

    let mut stack: VecDeque<i64> = VecDeque::new();
    let mut pc: i64 = 0;
    let mut r0: i64 = 0;
    let mut r1: i64 = 0;
    let mut r2: i64 = 0;
    let mut r3: i64 = 0;
    let mut steps: i64 = 0;
    let mut running = true;

    while running {
        let op = prog[pc as usize];
        pc += 1;
        steps += 1;
        match op {
            Op::Push(k) => stack.push_back(k),
            Op::Load(k) => {
                if k == 0 {
                    stack.push_back(r0)
                } else if k == 1 {
                    stack.push_back(r1)
                } else if k == 2 {
                    stack.push_back(r2)
                } else {
                    stack.push_back(r3)
                }
            }
            Op::Store(k) => {
                let v = stack.pop_back().unwrap();
                if k == 0 {
                    r0 = v
                } else if k == 1 {
                    r1 = v
                } else if k == 2 {
                    r2 = v
                } else {
                    r3 = v
                }
            }
            Op::Add => {
                let b = stack.pop_back().unwrap();
                let a = stack.pop_back().unwrap();
                stack.push_back(a + b);
            }
            Op::Mul => {
                let b = stack.pop_back().unwrap();
                let a = stack.pop_back().unwrap();
                stack.push_back(a * b);
            }
            Op::Mod => {
                let b = stack.pop_back().unwrap();
                let a = stack.pop_back().unwrap();
                stack.push_back(a % b);
            }
            Op::Lt => {
                let b = stack.pop_back().unwrap();
                let a = stack.pop_back().unwrap();
                stack.push_back(if a < b { 1 } else { 0 });
            }
            Op::JmpZ(t) => {
                let v = stack.pop_back().unwrap();
                if v == 0 {
                    pc = t;
                }
            }
            Op::Jmp(t) => pc = t,
            Op::Halt => running = false,
        }
    }

    println!("{}", stack.pop_back().unwrap());
    println!("{steps}");
}
