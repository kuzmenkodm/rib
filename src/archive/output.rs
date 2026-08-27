use std::fs::File;
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

pub fn write_image_tar(
    target: &Path,
    archive_name: &str,
    write_entries: impl FnOnce(&mut tar::Builder<BufWriter<File>>) -> Result<()>,
) -> Result<()> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = {
        #[cfg(unix)]
        {
            tempfile::Builder::new()
                .permissions(std::fs::Permissions::from_mode(0o666))
                .tempfile_in(parent)
        }

        #[cfg(not(unix))]
        {
            tempfile::NamedTempFile::new_in(parent)
        }
    }
    .with_context(|| format!("creating temporary archive in {}", parent.display()))?;
    let file = temporary
        .reopen()
        .with_context(|| format!("opening temporary {archive_name}"))?;
    let mut archive = tar::Builder::new(BufWriter::new(file));

    write_entries(&mut archive)?;

    let mut writer = archive
        .into_inner()
        .with_context(|| format!("finalizing {archive_name} tar"))?;
    writer
        .flush()
        .with_context(|| format!("flushing {archive_name}"))?;
    drop(writer);
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing {archive_name}"))?;
    let _published = temporary
        .persist(target)
        .map_err(|error| error.error)
        .with_context(|| format!("publishing {}", target.display()))?;
    Ok(())
}
