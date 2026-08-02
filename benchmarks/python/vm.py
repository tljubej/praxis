# vm — see ../praxis/vm.px for the description.
#
# Instructions are `(opcode, arg)` tuples — Python's analogue of the tagged
# union the other two use — and dispatch is an if/elif chain on the opcode,
# which is what a Python bytecode interpreter is normally written as. A `match`
# statement over integer literals compiles to the same comparison chain.
import sys

PUSH = 0
LOAD = 1
STORE = 2
ADD = 3
MUL = 4
MOD = 5
LT = 6
JMPZ = 7
JMP = 8
HALT = 9


def main():
    limit = int(sys.stdin.read().strip())

    # The program, hand-assembled. Register 0 is the loop counter, register 1
    # the accumulator; the loop head is instruction 4 and the exit target is 27.
    prog = [
        (PUSH, 0),        #  0
        (STORE, 0),       #  1   i = 0
        (PUSH, 1),        #  2
        (STORE, 1),       #  3   acc = 1
        (LOAD, 1),        #  4   <- loop head
        (PUSH, 31),       #  5
        (MUL, 0),         #  6   acc * 31
        (LOAD, 0),        #  7
        (PUSH, 7),        #  8
        (MUL, 0),         #  9   i * 7
        (PUSH, 13),       # 10
        (ADD, 0),         # 11   i * 7 + 13
        (PUSH, 1000003),  # 12
        (MOD, 0),         # 13   (i * 7 + 13) % 1000003
        (ADD, 0),         # 14   acc * 31 + that
        (PUSH, 1000003),  # 15
        (MOD, 0),         # 16
        (STORE, 1),       # 17   acc = ...
        (LOAD, 0),        # 18
        (PUSH, 1),        # 19
        (ADD, 0),         # 20
        (STORE, 0),       # 21   i = i + 1
        (LOAD, 0),        # 22
        (PUSH, limit),    # 23
        (LT, 0),          # 24   i < limit
        (JMPZ, 27),       # 25
        (JMP, 4),         # 26
        (LOAD, 1),        # 27   <- exit
        (HALT, 0),        # 28
    ]

    stack = []
    pc = 0
    r0 = 0
    r1 = 0
    r2 = 0
    r3 = 0
    steps = 0
    running = True

    while running:
        op, arg = prog[pc]
        pc += 1
        steps += 1
        if op == PUSH:
            stack.append(arg)
        elif op == LOAD:
            if arg == 0:
                stack.append(r0)
            elif arg == 1:
                stack.append(r1)
            elif arg == 2:
                stack.append(r2)
            else:
                stack.append(r3)
        elif op == STORE:
            v = stack.pop()
            if arg == 0:
                r0 = v
            elif arg == 1:
                r1 = v
            elif arg == 2:
                r2 = v
            else:
                r3 = v
        elif op == ADD:
            b = stack.pop()
            a = stack.pop()
            stack.append(a + b)
        elif op == MUL:
            b = stack.pop()
            a = stack.pop()
            stack.append(a * b)
        elif op == MOD:
            b = stack.pop()
            a = stack.pop()
            stack.append(a % b)
        elif op == LT:
            b = stack.pop()
            a = stack.pop()
            stack.append(1 if a < b else 0)
        elif op == JMPZ:
            v = stack.pop()
            if v == 0:
                pc = arg
        elif op == JMP:
            pc = arg
        else:
            running = False

    print(stack.pop())
    print(steps)


main()
