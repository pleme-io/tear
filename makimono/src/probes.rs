use std::io;
use std::path::PathBuf;

use tear_types::probes::{self, DUMP_ENV, FAULTS_ENV, FaultList};

pub fn arm_from_env() -> FaultList {
    let list = std::env::var(FAULTS_ENV)
        .map(|v| FaultList::parse(&v))
        .unwrap_or_default();
    for entry in &list.refused {
        eprintln!("{FAULTS_ENV}: `{entry}` names no fault and was ignored");
    }
    probes::arm(&list.faults);
    list
}

pub fn dump(role: &str) -> io::Result<Option<PathBuf>> {
    let Some(dir) = std::env::var_os(DUMP_ENV).map(PathBuf::from) else {
        return Ok(None);
    };
    let path = dir.join(format!("{role}-{}.tsv", std::process::id()));
    let mut body = String::from("kind\tname\tvalue\n");
    for r in probes::readings() {
        body.push_str(&r.row());
        body.push('\n');
    }
    crate::atomic::write(&path, body.as_bytes())?;
    Ok(Some(path))
}
