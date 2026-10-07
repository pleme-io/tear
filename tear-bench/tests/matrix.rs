use std::collections::BTreeSet;

use tear_bench::matrix::{
    Budget, Case, Cell, Control, ControlKind, Handover, Input, LANDED, Metric, Remote, Rung,
    Variant, cells, durability_of, durability_row, host_role_row, red_set, row, transport_row,
};
use tear_bench::verdict::{FloorSet, Preconditions, Verdict, derive};
use tear_client::Transport;
use tear_config::SessionDurability;
use tear_types::{Durability, HostRole};

const PENDING_CELLS: usize = 64;
const R1_CELLS: &[(Case, Metric)] = &[(Case::C13, Metric::Flushes)];

#[test]
fn the_matrix_declares_all_thirteen_cases_with_their_sub_variants() {
    let numbers: BTreeSet<u8> = Case::ALL.iter().map(|c| c.number()).collect();
    assert_eq!(numbers.len(), 13);
    assert_eq!(
        Case::ALL.len(),
        13 - 3 + Remote::ALL.len() + Handover::ALL.len() + Input::ALL.len()
    );
    let names: BTreeSet<String> = Case::ALL.iter().map(|c| c.name()).collect();
    assert_eq!(names.len(), Case::ALL.len());
    for (i, c) in Case::ALL.iter().enumerate() {
        assert_eq!(c.index(), i);
        assert_eq!(Case::parse(&c.name()), Some(*c));
    }
    assert_eq!(cells().len(), Case::ALL.len() * Metric::ALL.len());
}

#[test]
fn every_cell_is_pending_or_not_applicable_until_its_rung_lands() {
    assert_eq!(LANDED, &[Rung::R1]);
    let mut pending = 0;
    let mut budgeted = Vec::new();
    for cell in cells() {
        match cell.budget() {
            Budget::Pending {
                rung,
                today,
                receipt,
            } => {
                pending += 1;
                assert!(!LANDED.contains(&rung), "{}", cell.name());
                assert!(!today.is_empty() && !receipt.is_empty(), "{}", cell.name());
            }
            Budget::NotApplicable { why } => assert!(!why.is_empty(), "{}", cell.name()),
            _ => budgeted.push((cell.case, cell.metric)),
        }
    }
    assert_eq!(pending, PENDING_CELLS, "a §2 cell was added or lost");
    assert_eq!(budgeted, R1_CELLS, "only landed rungs' cells carry budgets");
    assert_eq!(
        Cell::new(Case::C13, Metric::Flushes).budget(),
        Budget::Count { max: 0 }
    );
}

#[test]
fn every_control_reddens_a_declared_cell_and_every_budgeted_cell_has_a_control() {
    for c in Control::ALL {
        let red = red_set(*c);
        assert!(!red.is_empty(), "{} reddens nothing", c.name());
        for cell in red {
            assert!(
                !matches!(cell.budget(), Budget::NotApplicable { .. }),
                "{} reddens the not-applicable {}",
                c.name(),
                cell.name()
            );
        }
    }
    let covered: Vec<Cell> = Control::ALL.iter().flat_map(|c| red_set(*c)).collect();
    for cell in cells().into_iter().filter(|c| c.budget().is_budgeted()) {
        assert!(
            covered.contains(&cell),
            "{} has no negative control",
            cell.name()
        );
    }
}

#[test]
fn faults_are_compiled_only_controls_and_name_their_rung() {
    for c in Control::ALL {
        if c.kind() == ControlKind::Fault {
            assert!(!c.today().is_empty());
        }
        assert!(c.rung().index() > 0, "{} names R0", c.name());
        assert!(
            LANDED.contains(&c.rung()) == (c.rung() == Rung::R1),
            "{} names {}",
            c.name(),
            c.rung().name()
        );
    }
}

#[test]
fn product_variants_map_onto_rows() {
    for d in [Durability::ProcessBound, Durability::Held] {
        let r = durability_row(d);
        assert_eq!(durability_of(r.config), d);
        assert!(!r.cases.is_empty());
    }
    for s in [SessionDurability::ProcessBound, SessionDurability::Held] {
        assert_eq!(durability_row(durability_of(s)).config, s);
    }
    for h in [HostRole::Relay, HostRole::Host] {
        assert!(host_role_row(h).cells.contains(&Metric::Answers));
    }
    let unix = Transport::Unix("run/x.sock".into());
    let tcp = Transport::Tcp("127.0.0.1:1".parse().unwrap());
    assert!(transport_row(&tcp).cases.contains(&Case::C6(Remote::Tcp)));
    assert!(transport_row(&unix).cases.contains(&Case::C2));
}

#[test]
fn presets_have_unique_labels_and_render_today_s_config() {
    let labels: BTreeSet<String> = Variant::PRESETS.iter().map(|(_, v)| v.label()).collect();
    assert_eq!(labels.len(), Variant::PRESETS.len());
    for (name, v) in Variant::PRESETS {
        assert_eq!(&v.label(), name);
        assert_eq!(Variant::preset(name), Some(*v));
    }
    let held = Variant::preset("held").unwrap().config_yaml();
    assert!(held.contains("durability: held") && held.contains("fsync_interval_ms: 1000"));
    let page = Variant::preset("held-page-cache").unwrap().config_yaml();
    assert!(page.contains("fsync_interval_ms: 86400000"));
    let wa = Variant::preset("held-write-ahead").unwrap().config_yaml();
    assert!(wa.contains("fsync_interval_ms: 0"));
    assert!(!Variant::BOUND.config_yaml().contains("journal"));
}

#[test]
fn with_no_samples_no_cell_reads_within() {
    let floors = FloorSet::default();
    for cell in cells() {
        let v = derive(cell.budget(), &[], &floors, &Preconditions::met(1));
        if cell.budget().is_budgeted() {
            assert!(
                matches!(v, Verdict::Errored { .. }),
                "{}: {v:?}",
                cell.name()
            );
        } else {
            assert!(
                matches!(v, Verdict::Pending { .. } | Verdict::NotApplicable { .. }),
                "{}: {v:?}",
                cell.name()
            );
        }
    }
}

#[test]
fn an_audit_write_on_the_key_path_reddens_c13_flushes_and_nothing_else() {
    let cell = Cell::new(Case::C13, Metric::Flushes);
    let floors = FloorSet::default();
    let clean = derive(cell.budget(), &[0.0, 0.0], &floors, &Preconditions::met(1));
    assert!(matches!(clean, Verdict::Within { .. }));
    let faulted = derive(
        cell.budget(),
        &[200.0, 0.0],
        &floors,
        &Preconditions::met(1),
    );
    assert!(faulted.is_red());
    assert_eq!(red_set(Control::AuditEveryKey), vec![cell]);
    assert!(tear_bench::verdict::audit_control(Control::AuditEveryKey, &[(cell, faulted)]).is_ok());
    assert!(tear_bench::verdict::audit_control(Control::AuditEveryKey, &[(cell, clean)]).is_err());
}

#[test]
fn the_rows_cite_the_receipts_section_two_names() {
    for case in Case::ALL {
        let r = row(case);
        assert!(r.receipt.starts_with('§'), "{}: {}", case.name(), r.receipt);
    }
}

#[test]
fn unsafe_code_lives_behind_exactly_one_allow() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut allows = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                let n = text.matches("allow(unsafe_code)").count();
                if n > 0 {
                    allows.push((p.strip_prefix(&src).unwrap().to_path_buf(), n));
                }
            }
        }
    }
    assert_eq!(allows, vec![(std::path::PathBuf::from("seam.rs"), 1)]);
    let lib = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    assert!(lib.contains("#![deny(unsafe_code)]"));
}

#[cfg(feature = "bench-probes")]
#[test]
fn every_product_fault_has_a_control() {
    for f in tear_types::probes::Fault::ALL {
        let c = tear_bench::matrix::control_of_fault(*f);
        assert_eq!(c.kind(), ControlKind::Fault);
        assert_eq!(c.name(), f.name());
    }
}
