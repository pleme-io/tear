#[test]
fn a_capability_without_a_row_does_not_compile() {
    trybuild::TestCases::new().compile_fail("tests/ui/capability_without_row.rs");
}
