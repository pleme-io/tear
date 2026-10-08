use tear_core::feeder::Parser;

struct Grid;

impl vte::Perform for Grid {}

fn main() {
    let mut parser = Parser::new();
    parser.advance(&mut Grid, b"\xc3");
}
