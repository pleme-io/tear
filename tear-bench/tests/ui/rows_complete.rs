use tear_bench::matrix::{Case, DurabilityRow, Row};
use tear_config::SessionDurability;
use tear_types::Durability;

tear_bench::product_rows! {
    fn rows(Durability) -> DurabilityRow {
        Durability::ProcessBound => DurabilityRow { config: SessionDurability::ProcessBound, yaml: "process_bound", label: "bound", cases: &[Case::C2] },
        Durability::Held => DurabilityRow { config: SessionDurability::Held, yaml: "held", label: "held", cases: &[Case::C4] },
    }
}

const R: Row = tear_bench::matrix::row(Case::C1);

tear_bench::bench_matrix! {
    Case::C1 | Case::C2 | Case::C3 | Case::C4 | Case::C5 | Case::C6(_) | Case::C7(_) | Case::C8 | Case::C9 | Case::C10 | Case::C11 | Case::C12(_) | Case::C13 => R,
}

fn main() {
    let _ = rows(Durability::Held);
    let _ = row(Case::C13);
}
