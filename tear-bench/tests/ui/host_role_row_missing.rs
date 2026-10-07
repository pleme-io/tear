use tear_bench::matrix::{Answerer, Case, HostRoleRow, Metric};
use tear_types::HostRole;

tear_bench::product_rows! {
    fn rows(HostRole) -> HostRoleRow {
        HostRole::Relay => HostRoleRow { answerer: Answerer::Consumer, cells: &[Metric::Answers], cases: &[Case::C3] },
    }
}

fn main() {
    let _ = rows(HostRole::Relay);
}
