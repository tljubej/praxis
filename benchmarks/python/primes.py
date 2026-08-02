# primes — see ../praxis/primes.px for the description.
import sys


def is_prime(n):
    if n < 2:
        return False
    if n % 2 == 0:
        return n == 2
    d = 3
    while d * d <= n:
        if n % d == 0:
            return False
        d += 2
    return True


def main():
    limit = int(sys.stdin.read().strip())

    count = 0
    total = 0
    n = 2
    while n < limit:
        if is_prime(n):
            count += 1
            total += n
        n += 1

    print(count)
    print(total)


main()
