// mandelbrot — see ../praxis/mandelbrot.px for the description.
use std::io::Read;

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap();
    let size: i64 = buf.trim().parse().unwrap();
    let max_iter: i64 = 400;

    let mut total: i64 = 0;
    let mut py: i64 = 0;
    while py < size {
        let y0 = py as f64 / size as f64 * 2.0 - 1.0;
        let mut px: i64 = 0;
        while px < size {
            let x0 = px as f64 / size as f64 * 3.0 - 2.0;
            let mut x: f64 = 0.0;
            let mut y: f64 = 0.0;
            let mut i: i64 = 0;
            while i < max_iter && x * x + y * y <= 4.0 {
                let xt = x * x - y * y + x0;
                y = 2.0 * x * y + y0;
                x = xt;
                i += 1;
            }
            total += i;
            px += 1;
        }
        py += 1;
    }

    println!("{total}");
}
