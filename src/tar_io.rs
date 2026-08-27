use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u64,
    pub gid: u64,
}

pub enum EntryData<'a> {
    Bytes(&'a [u8]),
    File(&'a mut File),
    Dir,
}

fn default_file_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
    }
}

fn make_header(
    entry_type: tar::EntryType,
    mode: u32,
    size: u64,
    chown: Option<Owner>,
) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    let (uid, gid) = chown.map(|owner| (owner.uid, owner.gid)).unwrap_or((0, 0));
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_size(size);
    header.set_mtime(0);
    header.set_uid(uid);
    header.set_gid(gid);
    header.set_username("").ok();
    header.set_groupname("").ok();
    header
}

fn append_entry<W: Write, R: io::Read>(
    builder: &mut tar::Builder<W>,
    path: &str,
    header: &mut tar::Header,
    data: R,
) -> Result<()> {
    let path = path.trim_start_matches('/');
    anyhow::ensure!(!path.is_empty(), "tar entry path must not be empty");
    builder
        .append_data(header, path, data)
        .with_context(|| format!("appending tar entry {path}"))
}

pub fn write_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    destination: &str,
    source: EntryData,
    mode_override: Option<u32>,
    chown: Option<Owner>,
) -> Result<()> {
    match source {
        EntryData::Bytes(bytes) => {
            let mut header = make_header(
                tar::EntryType::Regular,
                mode_override.unwrap_or(0o644),
                bytes.len() as u64,
                chown,
            );
            append_entry(builder, destination, &mut header, bytes)
        }
        EntryData::File(file) => {
            file.seek(SeekFrom::Start(0))
                .with_context(|| format!("rewind {destination}"))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("stat {destination}"))?;
            let mode = mode_override.unwrap_or_else(|| default_file_mode(&metadata));
            let mut header = make_header(tar::EntryType::Regular, mode, metadata.len(), chown);
            append_entry(builder, destination, &mut header, file)
        }
        EntryData::Dir => {
            let path = format!(
                "{}/",
                destination.trim_start_matches('/').trim_end_matches('/')
            );
            let mut header = make_header(
                tar::EntryType::Directory,
                mode_override.unwrap_or(0o755),
                0,
                chown,
            );
            append_entry(builder, &path, &mut header, io::empty())
        }
    }
}
