# mandelbrot — see ../praxis/mandelbrot.px for the description.
import sys


def main():
    size = int(sys.stdin.read().strip())
    max_iter = 400

    total = 0
    py = 0
    while py < size:
        y0 = py / size * 2.0 - 1.0
        px = 0
        while px < size:
            x0 = px / size * 3.0 - 2.0
            x = 0.0
            y = 0.0
            i = 0
            while i < max_iter and x * x + y * y <= 4.0:
                xt = x * x - y * y + x0
                y = 2.0 * x * y + y0
                x = xt
                i += 1
            total += i
            px += 1
        py += 1

    print(total)


main()
