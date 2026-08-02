# collatz — see ../praxis/collatz.px for the description.
import sys


def main():
    limit = int(sys.stdin.read().strip())

    best_n = 0
    best_len = 0
    n = 1
    while n <= limit:
        c = n
        steps = 0
        while c != 1:
            if c % 2 == 0:
                c = c // 2
            else:
                c = 3 * c + 1
            steps += 1
        if steps > best_len:
            best_len = steps
            best_n = n
        n += 1

    print(best_n)
    print(best_len)


main()
