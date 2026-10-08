#[test]
fn only_the_feeder_mints_what_a_parser_advances_over() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/chunk_minted_outside_the_feeder.rs");
    t.compile_fail("tests/ui/parser_advanced_over_raw_bytes.rs");
}
