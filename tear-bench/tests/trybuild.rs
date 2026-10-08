#[test]
fn a_variant_or_a_metric_without_a_row_does_not_compile() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/rows_complete.rs");
    t.compile_fail("tests/ui/durability_row_missing.rs");
    t.compile_fail("tests/ui/host_role_row_missing.rs");
    t.compile_fail("tests/ui/transport_row_missing.rs");
    t.compile_fail("tests/ui/case_row_missing.rs");
    t.compile_fail("tests/ui/sub_variant_row_missing.rs");
    t.compile_fail("tests/ui/budget_metric_missing.rs");
}

#[test]
fn a_holder_sink_cannot_be_built_around_a_shared_connection() {
    trybuild::TestCases::new().compile_fail("tests/ui/sink_around_shared_connection.rs");
}
