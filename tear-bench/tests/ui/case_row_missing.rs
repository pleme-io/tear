use tear_bench::matrix::{Case, Row};

const R: Row = tear_bench::matrix::row(Case::C1);

tear_bench::bench_matrix! {
    Case::C1 | Case::C2 | Case::C3 | Case::C4 | Case::C5 | Case::C6(_) | Case::C7(_) | Case::C8 | Case::C9 | Case::C10 | Case::C11 | Case::C12(_) => R,
}

fn main() {
    let _ = row(Case::C1);
}
