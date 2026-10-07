use tear_bench::matrix::{Case, TransportKind, TransportRow};
use tear_client::Transport;

tear_bench::product_rows! {
    fn rows(&Transport) -> TransportRow {
        Transport::Unix(_) => TransportRow { kind: TransportKind::Unix, cases: &[Case::C2] },
    }
}

fn main() {
    let _ = rows(&Transport::Unix("x".into()));
}
