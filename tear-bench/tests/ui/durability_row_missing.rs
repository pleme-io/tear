use tear_bench::matrix::{Case, DurabilityRow};
use tear_config::SessionDurability;
use tear_types::Durability;

tear_bench::product_rows! {
    fn rows(Durability) -> DurabilityRow {
        Durability::ProcessBound => DurabilityRow { config: SessionDurability::ProcessBound, yaml: "process_bound", label: "bound", cases: &[Case::C2] },
    }
}

fn main() {
    let _ = rows(Durability::ProcessBound);
}
