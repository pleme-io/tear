use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp = tmp_path(path);
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write(path, &json)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_lands_whole_file_private_and_leaves_no_temp() {
        let t = TempDir::new("atomic");
        let p = t.path().join("nested/doc.json");
        write_json(&p, &vec![1, 2, 3]).unwrap();
        let back: Option<Vec<i32>> = read_json(&p).unwrap();
        assert_eq!(back, Some(vec![1, 2, 3]));
        assert!(!tmp_path(&p).exists());
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = fs::metadata(p.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
    }

    #[test]
    fn read_of_absent_file_is_none_and_garbage_is_an_error() {
        let t = TempDir::new("atomic-read");
        let p = t.path().join("x.json");
        assert!(read_json::<u32>(&p).unwrap().is_none());
        fs::write(&p, b"{not json").unwrap();
        assert!(read_json::<u32>(&p).is_err());
    }
}
