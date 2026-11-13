//! The command-line tool of an app, installed beside it on request (macOS).
//!
//! A macOS disk image holds only the app; each release publishes the tool as a zip of its own
//! (`<app>-cli-<version>-macos-universal.zip`, checked against the release's `SHA256SUMS.txt` like
//! every download). The tool goes into `<data>/cli/<app>/`, replaced whole on an update (the new
//! folder is put in place, then the old one goes: macOS kills a tool that was written over in
//! place), with a link in `<data>/cli/bin/` so one `PATH` line covers every app. The path
//! never carries a version, so a program that stored it (an MCP client's configuration) keeps
//! working after an update. Only what the launcher put there is ever removed.
//!
//! The tool must be signed by the app's own Developer ID team and notarized, the checks the app
//! itself passes (`macos_verify`) apart from the Gatekeeper assessment, which only judges apps.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use artcraft_launcher_core::platform::{self, Platform};
use artcraft_launcher_core::protocol::JobPhase;
use artcraft_launcher_core::release::{Asset, Release};
use artcraft_launcher_core::{CraftApp, version};

use super::{Job, bind_download_url, download};
use crate::manifest::CliRecord;
use crate::paths::Paths;
use crate::process::RunOutput;

const CODESIGN: &str = "/usr/bin/codesign";
const TIMEOUT: Duration = Duration::from_secs(120);

/// Runs a system tool (`process::run_with_output`; a fake in tests).
pub(crate) type Run<'a> = &'a mut dyn FnMut(&str, &[&str], Duration) -> Result<RunOutput, String>;

/// The release's command-line tool for `platform`, if it publishes one.
pub(crate) fn asset<'a>(app: &CraftApp, release: &'a Release, platform: Platform) -> Option<&'a Asset> {
    let stems: Vec<&str> = app.stems().collect();
    platform::pick_cli_asset(release, &stems, platform)
}

/// The tool's file name inside its folder.
pub(crate) fn binary_name(app: &CraftApp) -> String {
    format!("{}-cli", app.id)
}

/// Downloads, checks and installs the tool of `release` for `app`.
/// `existing` is the record of the tool the launcher installed earlier, if any: it alone says the
/// folder and link at the destination are the launcher's to replace.
pub(crate) fn install(
    app: &CraftApp,
    release: &Release,
    asset: &Asset,
    existing: Option<&CliRecord>,
    job: &mut Job<'_>,
    run: Run<'_>,
    persist: &mut dyn FnMut(&CliRecord) -> Result<(), String>,
) -> Result<CliRecord, String> {
    app.mac_team.ok_or("ArtCraft Launcher has no macOS signing team recorded for this app, so its command-line tool wasn't installed")?;
    bind(app, release, asset)?;
    // Before the download: nothing here is worth fetching if the destination isn't ours to replace.
    ensure_ours(job.paths, app, existing)?;
    let (file, _) = download(release, asset, job)?;
    (job.phase)(JobPhase::Installing);
    let record = place(app, &release.version_label(), &file, job.paths, existing, run, persist)?;
    let _ = std::fs::remove_file(&file);
    Ok(record)
}

/// The version in the file's name and the tag in its URL must be the release's: a feed can't
/// advertise one version and point at another's tool.
fn bind(app: &CraftApp, release: &Release, asset: &Asset) -> Result<(), String> {
    let mismatch = || format!("{}'s command-line tool download does not match the advertised version {}.", app.name, release.version_label());
    if let Some(release_version) = release.version.as_ref() {
        let stems: Vec<&str> = app.stems().collect();
        let (named, _) = platform::parse_cli_asset_name(&asset.name, &stems).ok_or_else(mismatch)?;
        if version::parse(&named).as_ref() != Some(release_version) {
            return Err(mismatch());
        }
    }
    bind_download_url(app, release, asset)
}

/// Whether the folder and the link the tool would go to are free or the launcher's own: a folder
/// is the launcher's when `existing` names it, a link when it points into the launcher's `cli`
/// folder. Anything else is a user's, and is left alone.
fn ensure_ours(paths: &Paths, app: &CraftApp, existing: Option<&CliRecord>) -> Result<(), String> {
    let conflict =
        |path: &Path| format!("{} already exists and wasn't made by ArtCraft Launcher, so the command-line tool wasn't installed", path.display());
    // A link in place of either folder would lead the install (staging, the swap, the new link) out of them.
    for dir in [paths.cli(), paths.cli_bin()] {
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => return Err(format!("{} is a link, so the command-line tool wasn't installed", dir.display())),
            Ok(m) if !m.is_dir() => return Err(format!("{} isn't a folder, so the command-line tool wasn't installed", dir.display())),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("couldn't inspect {} ({e})", dir.display())),
            _ => {}
        }
    }
    let folder = paths.cli().join(app.id);
    if std::fs::symlink_metadata(&folder).is_ok() && !existing.is_some_and(|record| record.folder == folder) {
        return Err(conflict(&folder));
    }
    let link = paths.cli_bin().join(binary_name(app));
    if link_is_ours(paths, &link) == Some(false) {
        return Err(conflict(&link));
    }
    Ok(())
}

/// Whether what is at `link` is the launcher's: a link into its own `cli` folder. `None` when nothing is there.
fn link_is_ours(paths: &Paths, link: &Path) -> Option<bool> {
    let meta = std::fs::symlink_metadata(link).ok()?;
    Some(meta.file_type().is_symlink() && std::fs::read_link(link).is_ok_and(|to| to.starts_with(paths.cli())))
}

/// Unpacks `zip`, checks the app's signing team, and retains a backup until its link and record are committed.
/// Nothing is replaced unless every check passed and the destination is free or the launcher's own
/// (see [`ensure_ours`]), so a failure leaves the old tool as it was.
pub(super) fn place(
    app: &CraftApp,
    version: &str,
    zip: &Path,
    paths: &Paths,
    existing: Option<&CliRecord>,
    run: Run<'_>,
    persist: &mut dyn FnMut(&CliRecord) -> Result<(), String>,
) -> Result<CliRecord, String> {
    let team = app.mac_team.ok_or("no macOS signing team is configured for this app")?;
    // First of all: staging is created inside `cli`, so a link there must be found before it is.
    ensure_ours(paths, app, existing)?;
    let root = paths.cli();
    std::fs::create_dir_all(&root).map_err(|e| format!("couldn't create {} ({e})", root.display()))?;
    // A folder of our own: a predictable name there may be the user's.
    let staging = tempfile::Builder::new()
        .prefix(".artcraft-launcher-")
        .tempdir_in(&root)
        .map_err(|e| format!("couldn't create a staging folder in {} ({e})", root.display()))?;
    let unpacked = staging.path().join("unpacked");
    extract(zip, &unpacked, 50_000, 2 * 1024 * 1024 * 1024)?;
    let top = super::single_top_folder(&unpacked)?;
    let name = binary_name(app);
    let bin = top.join(&name);
    if !std::fs::symlink_metadata(&bin).is_ok_and(|m| m.is_file()) {
        return Err(format!("the download doesn't contain {name}"));
    }
    verify(team, &bin, run)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("couldn't make {name} runnable ({e})"))?;
    }
    // Only execute the staged tool after its signature and notarization have passed.
    let binary = bin.to_str().ok_or("couldn't read the command-line tool's unusual path")?;
    let output = run(binary, &["--version"], Duration::from_secs(5)).map_err(|e| format!("couldn't read the command-line tool's version ({e})"))?;
    if !output.success {
        return Err(format!("couldn't read the command-line tool's version ({})", output.status));
    }
    let actual = crate::detect::parse_cli_version(&output.stdout, app).ok_or("the command-line tool didn't report a recognizable version")?;
    let expected = version::parse(version).ok_or("the requested command-line tool version isn't valid")?;
    if actual != expected {
        return Err(format!("the command-line tool reports version {actual}, but this release requires {expected}"));
    }
    // Checked again here, and before the swap: the link must be settled while the old tool is still there.
    ensure_ours(paths, app, existing)?;
    let folder = root.join(app.id);
    let link_path = paths.cli_bin().join(&name);
    let old_link = std::fs::read_link(&link_path).ok();
    let backup = staging.path().join("old");
    let had_old = std::fs::symlink_metadata(&folder).is_ok();
    if had_old {
        std::fs::rename(&folder, &backup).map_err(|e| format!("couldn't move the old command-line tool aside ({e})"))?;
    }
    let record = CliRecord { version: version.to_string(), folder: folder.clone(), link: link_path.clone(), installed_at: crate::now() };
    let mut placed = false;
    let result = (|| {
        std::fs::rename(&top, &folder).map_err(|e| format!("couldn't put the command-line tool in place ({e})"))?;
        placed = true;
        link(paths, &name, &folder.join(&name))?;
        persist(&record).map_err(|e| format!("couldn't save the command-line tool's record ({e})"))
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<(), String> {
            if placed {
                std::fs::remove_dir_all(&folder).map_err(|e| format!("couldn't remove the new tool ({e})"))?;
            }
            if had_old {
                std::fs::rename(&backup, &folder).map_err(|e| format!("couldn't restore the old tool ({e})"))?;
            }
            match old_link {
                Some(target) => {
                    if std::fs::read_link(&link_path).ok().as_ref() != Some(&target) {
                        if std::fs::read_link(&link_path).is_ok_and(|to| to == folder.join(&name)) {
                            std::fs::remove_file(&link_path).map_err(|e| format!("couldn't remove the new link ({e})"))?;
                        }
                        std::os::unix::fs::symlink(target, &link_path).map_err(|e| format!("couldn't restore the old link ({e})"))?;
                    }
                }
                None if std::fs::read_link(&link_path).is_ok_and(|to| to == folder.join(&name)) => {
                    std::fs::remove_file(&link_path).map_err(|e| format!("couldn't remove the new link ({e})"))?;
                }
                None => {}
            }
            Ok(())
        })();
        if let Err(restore) = rollback {
            let saved = staging.keep();
            return Err(format!("{error}; {restore}; recovery files were kept at {}", saved.display()));
        }
        return Err(error);
    }
    Ok(record)
}

/// Extracts regular files into fresh staging, bounding metadata and actual expanded bytes before replacement.
fn extract(file: &Path, destination: &Path, max_entries: usize, max_bytes: u64) -> Result<(), String> {
    let file = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("couldn't read the command-line tool ZIP ({e})"))?;
    if archive.has_duplicate_names() {
        return Err("the command-line tool ZIP contains duplicate paths".into());
    }
    if archive.len() > max_entries {
        return Err("the command-line tool ZIP contains too many entries".into());
    }
    let mut declared_total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index_data(index).map_err(|e| e.to_string())?;
        declared_total = declared_total
            .checked_add(entry.size())
            .filter(|total| *total <= max_bytes)
            .ok_or("the command-line tool ZIP exceeds the maximum unpacked size")?;
    }
    let mut remaining = max_bytes;
    let mut seen = std::collections::BTreeSet::new();
    std::fs::create_dir(destination).map_err(|e| e.to_string())?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let relative = entry.enclosed_name().ok_or("the command-line tool ZIP contains an unsafe path")?;
        let name = entry.name().map_err(|e| e.to_string())?;
        let mode = entry.unix_mode().unwrap_or(0);
        let kind = mode & 0o170000;
        if relative.as_os_str().is_empty()
            || relative.components().any(|c| !matches!(c, Component::Normal(_)))
            || name.starts_with('/')
            || name.contains(['\\', ':'])
            || name.split('/').any(|part| matches!(part, "." | ".."))
            || !matches!(kind, 0 | 0o100000 | 0o040000)
            || (kind == 0o040000 && !entry.is_dir())
            || (kind == 0o100000 && entry.is_dir())
        {
            return Err("the command-line tool ZIP contains an unsafe path, link or special file".into());
        }
        // Case variants collide on the default macOS filesystem; create_new also prevents alias overwrites.
        if !seen.insert(relative.to_string_lossy().to_lowercase()) {
            return Err("the command-line tool ZIP contains duplicate paths".into());
        }
        let declared = entry.size();
        if declared > remaining {
            return Err("the command-line tool ZIP exceeds the maximum unpacked size".into());
        }
        let target = destination.join(relative);
        if entry.is_dir() {
            if declared != 0 {
                return Err("the command-line tool ZIP has a nonempty directory entry".into());
            }
            std::fs::create_dir_all(target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut (&mut entry).take(declared), &mut output).map_err(|e| e.to_string())?;
        // Read beyond the declared size only into memory: dishonest metadata cannot spend extra disk budget.
        let mut extra = [0; 1];
        if copied != declared || entry.read(&mut extra).map_err(|e| e.to_string())? != 0 {
            return Err("the command-line tool ZIP has an unexpected unpacked size".into());
        }
        remaining = remaining.checked_sub(copied).ok_or("the command-line tool ZIP exceeds the maximum unpacked size")?;
        #[cfg(unix)]
        if mode != 0 {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(std::fs::Permissions::from_mode(mode & 0o777)).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn check(result: Result<RunOutput, String>, what: &str) -> Result<(), String> {
    let output = result.map_err(|reason| format!("{what} did not succeed ({reason})"))?;
    if output.success {
        return Ok(());
    }
    let reason = output.stderr.trim();
    let reason = if reason.is_empty() { output.status } else { reason.to_string() };
    Err(format!("{what} did not succeed ({reason})"))
}

/// Signature integrity, the app's own Developer ID team, and notarization: each must hold before
/// anything is replaced.
fn verify(team: &str, bin: &Path, run: Run<'_>) -> Result<(), String> {
    let path = bin.to_str().ok_or("couldn't check the tool's unusual path, so it wasn't installed")?;
    if team.is_empty() || !team.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("no valid macOS signing team is configured, so the command-line tool wasn't installed".into());
    }
    // Developer ID Application's standard requirement, as for the app itself.
    let requirement = format!(
        "=anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
         and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU]=\"{team}\""
    );
    for (what, args) in [
        ("signature integrity", vec!["--verify", "--all-architectures", "--strict", path]),
        ("developer identity", vec!["--verify", "--all-architectures", "--strict", "-R", &requirement, path]),
        ("notarization", vec!["--verify", "--check-notarization", "-R=notarized", path]),
    ] {
        check(run(CODESIGN, &args, TIMEOUT), &format!("macOS verification of the command-line tool ({what})"))
            .map_err(|e| format!("{e}, so it wasn't installed"))?;
    }
    Ok(())
}

/// Creates the stable command link, leaving an existing correct link untouched.
#[cfg(unix)]
fn link(paths: &Paths, name: &str, target: &Path) -> Result<PathBuf, String> {
    let dir = paths.cli_bin();
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {} ({e})", dir.display()))?;
    let link = dir.join(name);
    // Never delete an existing command to create a link: even an owned link may change after inspection.
    match link_is_ours(paths, &link) {
        Some(true) if std::fs::read_link(&link).is_ok_and(|to| to == target) => return Ok(link),
        Some(_) => {
            return Err(format!("{} already exists and doesn't point to this command-line tool, so it wasn't replaced", link.display()));
        }
        None => {}
    }
    std::os::unix::fs::symlink(target, &link).map_err(|e| format!("couldn't link {} ({e})", link.display()))?;
    Ok(link)
}

/// A real folder: not a link, whatever it points at.
fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Takes away what `record` says the launcher installed: its link (when it still points at the
/// tool) and its folder (when it is a folder directly inside the launcher's own `cli` folder).
/// The `cli` and `cli/bin` folders themselves must be real folders: a link there could lead
/// anywhere, and nothing is removed through it. An error leaves the record in place for a retry.
pub(crate) fn remove(paths: &Paths, app: &CraftApp, record: &CliRecord) -> Result<(), String> {
    if record.folder != paths.cli().join(app.id) || record.link != paths.cli_bin().join(binary_name(app)) {
        return Err("the command-line tool's removal record doesn't match this app, so nothing was removed".into());
    }
    let (root, bin_dir) = (paths.cli(), paths.cli_bin());
    let inspect = |path: &Path| match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("couldn't inspect {} ({e})", path.display())),
    };
    // Inspect both before removal: an inaccessible entry must retain its record for a later retry.
    let folder_meta = inspect(&record.folder)?;
    let link_meta = inspect(&record.link)?;
    if link_meta.is_some_and(|m| m.file_type().is_symlink())
        && record.link.parent() == Some(bin_dir.as_path())
        && std::fs::read_link(&record.link).map_err(|e| format!("couldn't inspect {} ({e})", record.link.display()))?.starts_with(&record.folder)
    {
        if !is_plain_dir(&root) || !is_plain_dir(&bin_dir) {
            return Err(format!("{} isn't a real folder, so nothing was removed through it", bin_dir.display()));
        }
        std::fs::remove_file(&record.link).map_err(|e| format!("couldn't remove {} ({e})", record.link.display()))?;
    }
    let inside = record.folder.parent() == Some(root.as_path()) && record.folder != bin_dir;
    if inside && folder_meta.is_some() {
        if !is_plain_dir(&root) || !is_plain_dir(&record.folder) {
            return Err(format!("{} isn't a real folder, so nothing was removed through it", record.folder.display()));
        }
        std::fs::remove_dir_all(&record.folder).map_err(|e| format!("couldn't remove {} ({e})", record.folder.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use artcraft_launcher_core::{Arch, Os};
    use std::cell::RefCell;

    fn place(
        app: &CraftApp,
        version: &str,
        files: &[(&str, &str)],
        team: &str,
        paths: &Paths,
        existing: Option<&CliRecord>,
        run: Run<'_>,
    ) -> Result<CliRecord, String> {
        assert_eq!(app.mac_team, Some(team));
        let zip = archive(files);
        super::place(app, version, zip.path(), paths, existing, run, &mut |_| Ok(()))
    }

    fn ok() -> Result<RunOutput, String> {
        Ok(RunOutput { success: true, code: Some(0), stdout: String::new(), stderr: String::new(), status: "exit 0".into() })
    }

    fn app() -> &'static CraftApp {
        CraftApp::by_id("pdfcraft").unwrap()
    }

    fn archive(files: &[(&str, &str)]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(file.reopen().unwrap());
        for (name, contents) in files {
            let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            if name.ends_with('/') {
                zip.add_directory(*name, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(contents.as_bytes()).unwrap();
            }
        }
        zip.finish().unwrap();
        file
    }

    /// Only signature and version subprocesses are faked; extraction reads actual ZIP fixtures.
    fn runner<'a>(
        version: &'a str,
        calls: &'a RefCell<Vec<String>>,
        verdicts: &'a [bool],
    ) -> impl FnMut(&str, &[&str], Duration) -> Result<RunOutput, String> + 'a {
        move |program, args, _| {
            calls.borrow_mut().push(format!("{} {}", program.rsplit('/').next().unwrap(), args.join(" ")));
            if program != CODESIGN {
                assert_eq!(args, ["--version"]);
                let mut output = ok()?;
                output.stdout = format!("pdfcraft-cli {version}\n");
                return Ok(output);
            }
            let n = calls.borrow().iter().filter(|c| c.starts_with("codesign")).count();
            if verdicts.get(n - 1).copied().unwrap_or(true) {
                ok()
            } else {
                Ok(RunOutput {
                    success: false,
                    code: Some(3),
                    stdout: String::new(),
                    stderr: "code failed to satisfy specified code requirement(s)".into(),
                    status: "exit 3".into(),
                })
            }
        }
    }

    fn declared_size(file: &Path, size: u32) {
        let mut bytes = std::fs::read(file).unwrap();
        for (signature, offset) in [(b"PK\x03\x04", 22), (b"PK\x01\x02", 24)] {
            let header = bytes.windows(4).position(|window| window == signature).unwrap();
            bytes[header + offset..header + offset + 4].copy_from_slice(&size.to_le_bytes());
        }
        std::fs::write(file, bytes).unwrap();
    }

    #[test]
    fn extraction_bounds_entry_count_and_cumulative_bytes_before_writing_the_entry() {
        let file = archive(&[("one", "12"), ("two", "34")]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("entry-limit");
        let err = extract(file.path(), &destination, 1, 10).unwrap_err();
        assert!(err.contains("too many entries"), "{err}");
        assert!(!destination.exists(), "entry limit is checked before any extraction");
        let destination = dir.path().join("byte-limit");
        let err = extract(file.path(), &destination, 2, 3).unwrap_err();
        assert!(err.contains("maximum unpacked size"), "{err}");
        assert!(!destination.exists(), "declared cumulative size is checked before any extraction");
    }

    #[test]
    fn streamed_zip_data_descriptors_keep_their_central_directory_size_limits() {
        use std::io::Write;
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new_stream(file.reopen().unwrap());
        zip.start_file("tool", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"streamed").unwrap();
        zip.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("fits");
        extract(file.path(), &destination, 1, 8).unwrap();
        assert_eq!(std::fs::read(destination.join("tool")).unwrap(), b"streamed");
        assert!(extract(file.path(), &dir.path().join("too-big"), 1, 7).is_err());
    }

    #[test]
    fn a_compressed_bomb_and_dishonest_size_cannot_write_past_the_budget() {
        let contents = "a".repeat(8_192);
        let file = archive(&[("bomb", &contents)]);
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("declared-large");
        assert!(extract(file.path(), &destination, 1, 16).is_err());
        assert!(!destination.join("bomb").exists());
        declared_size(file.path(), 1);
        let destination = dir.path().join("dishonest-small");
        assert!(extract(file.path(), &destination, 1, 1).is_err());
        assert!(std::fs::metadata(destination.join("bomb")).map_or(true, |meta| meta.len() <= 1), "extra expanded bytes are never written");
    }

    #[test]
    fn unsafe_zip_paths_duplicates_special_files_and_truncation_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for (index, name) in ["../escape", "/absolute", "folder/../escape", "folder\\file", "C:escape"].iter().enumerate() {
            let file = archive(&[(name, "bad")]);
            assert!(extract(file.path(), &dir.path().join(format!("unsafe-{index}")), 10, 100).is_err(), "{name}");
        }
        assert!(!dir.path().join("escape").exists());
        let file = archive(&[("same", "first"), ("SAME", "second")]);
        let destination = dir.path().join("case-duplicate");
        assert!(extract(file.path(), &destination, 10, 100).unwrap_err().contains("duplicate"));
        assert_eq!(std::fs::read_to_string(destination.join("same")).unwrap(), "first");
        let file = archive(&[("aaa", "first"), ("bbb", "second")]);
        let mut bytes = std::fs::read(file.path()).unwrap();
        let names: Vec<_> = bytes.windows(3).enumerate().filter_map(|(i, window)| (window == b"bbb").then_some(i)).collect();
        for index in names {
            bytes[index..index + 3].copy_from_slice(b"aaa");
        }
        std::fs::write(file.path(), bytes).unwrap();
        assert!(extract(file.path(), &dir.path().join("exact-duplicate"), 10, 100).unwrap_err().contains("duplicate"));
        let file = archive(&[("fifo", "")]);
        let mut bytes = std::fs::read(file.path()).unwrap();
        let central = bytes.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
        bytes[central + 38..central + 42].copy_from_slice(&(0o010644u32 << 16).to_le_bytes());
        std::fs::write(file.path(), &bytes).unwrap();
        assert!(extract(file.path(), &dir.path().join("special-file"), 10, 100).unwrap_err().contains("special file"));
        bytes.truncate(bytes.len() - 10);
        std::fs::write(file.path(), bytes).unwrap();
        assert!(extract(file.path(), &dir.path().join("truncated"), 10, 100).is_err());
    }

    #[test]
    fn wrapper_layout_keeps_sibling_runtime_files_and_their_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let record = place(
            app(),
            "0.20.0",
            &[("bundle/pdfcraft-cli", "tool"), ("bundle/runtime/helper", "runtime"), ("bundle/LICENSE", "license")],
            "DJ6XS33FX8",
            &paths,
            None,
            &mut runner("0.20.0", &calls, &[]),
        )
        .unwrap();
        let sibling = record.folder.join("runtime/helper");
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "runtime");
        assert_eq!(std::fs::metadata(sibling).unwrap().permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn oversized_archives_leave_first_installs_and_updates_unchanged() {
        for update in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let old = update
                .then(|| place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap());
            let mut manifest = crate::manifest::Manifest::default();
            if let Some(old) = &old {
                manifest.cli.insert(app().id.into(), old.clone());
            }
            manifest.save(&paths.manifest()).unwrap();
            let before = std::fs::read(paths.manifest()).unwrap();
            let file = archive(&[("pdfcraft-cli", "new")]);
            declared_size(file.path(), 2 * 1024 * 1024 * 1024 + 1);
            calls.borrow_mut().clear();
            let mut saved = false;
            let err = super::place(app(), "0.20.0", file.path(), &paths, old.as_ref(), &mut runner("0.20.0", &calls, &[]), &mut |_| {
                saved = true;
                Ok(())
            })
            .unwrap_err();
            assert!(err.contains("maximum unpacked size"), "{err}");
            assert!(!saved && calls.borrow().is_empty());
            assert_eq!(std::fs::read(paths.manifest()).unwrap(), before);
            if let Some(old) = old {
                assert_eq!(std::fs::read_to_string(old.link).unwrap(), "old");
            } else {
                assert!(!paths.cli().join("pdfcraft").exists());
            }
        }
    }

    #[test]
    fn a_blocked_bin_folder_keeps_first_installs_and_updates_unchanged() {
        for update in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let old = update
                .then(|| place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap());
            if let Some(old) = &old {
                std::fs::remove_file(&old.link).unwrap();
                std::fs::remove_dir(paths.cli_bin()).unwrap();
            } else {
                std::fs::create_dir_all(paths.cli()).unwrap();
            }
            std::fs::write(paths.cli_bin(), "user file").unwrap();
            let err = place(app(), "0.20.0", &[("pdfcraft-cli", "new")], "DJ6XS33FX8", &paths, old.as_ref(), &mut runner("0.20.0", &calls, &[]))
                .unwrap_err();
            assert!(err.contains("isn't a folder"), "{err}");
            assert_eq!(std::fs::read_to_string(paths.cli_bin()).unwrap(), "user file");
            if let Some(old) = old {
                assert_eq!(std::fs::read_to_string(old.folder.join("pdfcraft-cli")).unwrap(), "old");
            } else {
                assert!(!paths.cli().join("pdfcraft").exists());
            }
        }
    }

    #[test]
    fn link_failure_after_the_swap_restores_first_installs_and_updates() {
        use std::os::unix::fs::PermissionsExt;
        for update in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let old = update
                .then(|| place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap());
            std::fs::create_dir_all(paths.cli_bin()).unwrap();
            // A missing link needs creation after the folder swap, so unwritable bin exercises rollback.
            if let Some(old) = &old {
                std::fs::remove_file(&old.link).unwrap();
            }
            let mode = std::fs::metadata(paths.cli_bin()).unwrap().permissions();
            std::fs::set_permissions(paths.cli_bin(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let mut saved = false;
            let result = super::place(
                app(),
                "0.20.0",
                archive(&[("pdfcraft-cli", "new")]).path(),
                &paths,
                old.as_ref(),
                &mut runner("0.20.0", &calls, &[]),
                &mut |_| {
                    saved = true;
                    Ok(())
                },
            );
            std::fs::set_permissions(paths.cli_bin(), mode).unwrap();
            let err = result.unwrap_err();
            assert!(err.contains("couldn't link"), "{err}");
            assert!(!saved, "manifest is never committed when linking fails");
            if let Some(old) = old {
                assert_eq!(std::fs::read_to_string(old.folder.join("pdfcraft-cli")).unwrap(), "old");
                assert!(std::fs::symlink_metadata(&old.link).is_err(), "the previously missing link stays absent");
            } else {
                assert!(!paths.cli().join("pdfcraft").exists());
                assert!(std::fs::symlink_metadata(paths.cli_bin().join("pdfcraft-cli")).is_err());
            }
        }
    }

    #[test]
    fn a_failed_record_save_rolls_back_first_installs_and_updates() {
        for update in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let old = update
                .then(|| place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap());
            let mut manifest = crate::manifest::Manifest::default();
            if let Some(old) = &old {
                manifest.cli.insert(app().id.into(), old.clone());
            }
            manifest.save(&paths.manifest()).unwrap();
            let before = std::fs::read(paths.manifest()).unwrap();
            let err = super::place(
                app(),
                "0.20.0",
                archive(&[("pdfcraft-cli", "new")]).path(),
                &paths,
                old.as_ref(),
                &mut runner("0.20.0", &calls, &[]),
                &mut |record| {
                    assert_eq!(std::fs::read_to_string(&record.link).unwrap(), "new", "failure happens after placement and linking");
                    Err("disk full".into())
                },
            )
            .unwrap_err();
            assert!(err.contains("disk full"), "{err}");
            assert_eq!(std::fs::read(paths.manifest()).unwrap(), before);
            if let Some(old) = old {
                assert_eq!(std::fs::read_to_string(&old.link).unwrap(), "old");
                assert_eq!(std::fs::read_link(&old.link).unwrap(), old.folder.join("pdfcraft-cli"));
            } else {
                assert!(!paths.cli().join("pdfcraft").exists());
                assert!(std::fs::symlink_metadata(paths.cli_bin().join("pdfcraft-cli")).is_err());
                place(app(), "0.20.0", &[("pdfcraft-cli", "retry")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap();
            }
        }
    }

    #[test]
    fn an_install_puts_the_tool_in_a_stable_folder_with_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let mut run = runner("0.20.0", &calls, &[]);
        let record = place(app(), "0.20.0", &[("pdfcraft-cli", "tool"), ("LICENSE", "x")], "DJ6XS33FX8", &paths, None, &mut run).unwrap();
        assert_eq!(record.folder, paths.cli().join("pdfcraft"));
        assert_eq!(std::fs::read_to_string(record.folder.join("pdfcraft-cli")).unwrap(), "tool");
        assert_eq!(record.link, paths.cli_bin().join("pdfcraft-cli"));
        assert_eq!(std::fs::read_link(&record.link).unwrap(), record.folder.join("pdfcraft-cli"));
        let calls = calls.borrow();
        assert_eq!(calls.iter().filter(|c| c.starts_with("codesign")).count(), 3, "integrity, identity and notarization: {calls:?}");
        assert!(calls.iter().any(|c| c.contains("leaf[subject.OU]=\"DJ6XS33FX8\"")), "{calls:?}");
        // The staging folder is gone: only the app's folder and bin are left.
        let mut left: Vec<_> = std::fs::read_dir(paths.cli()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["bin", "pdfcraft"]);
    }

    #[test]
    fn an_update_replaces_the_folder_whole_and_keeps_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let first =
            place(app(), "0.19.0", &[("pdfcraft-cli", "old"), ("OLD-ONLY", "x")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[]))
                .unwrap();
        let second =
            place(app(), "0.20.0", &[("pdfcraft-cli", "new")], "DJ6XS33FX8", &paths, Some(&first), &mut runner("0.20.0", &calls, &[])).unwrap();
        assert_eq!((first.folder.clone(), first.link.clone()), (second.folder.clone(), second.link.clone()), "the path never carries a version");
        assert_eq!(std::fs::read_to_string(second.folder.join("pdfcraft-cli")).unwrap(), "new");
        assert!(!second.folder.join("OLD-ONLY").exists(), "the old folder went whole");
        assert_eq!(second.version, "0.20.0");
    }

    #[test]
    fn invalid_executable_versions_leave_first_installs_and_updates_unchanged() {
        for update in [false, true] {
            for (stdout, success, failed_to_run) in [
                ("pdfcraft-cli 0.19.0", true, false),
                ("pdfcraft-cli unknown", true, false),
                ("pdfcraft-cli 0.20.0", false, false),
                ("", false, true),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                let calls = RefCell::new(Vec::new());
                let old = update.then(|| {
                    place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap()
                });
                let mut manifest = crate::manifest::Manifest::default();
                if let Some(old) = &old {
                    manifest.cli.insert(app().id.into(), old.clone());
                }
                manifest.save(&paths.manifest()).unwrap();
                let before = std::fs::read(paths.manifest()).unwrap();
                calls.borrow_mut().clear();
                let zip = archive(&[("pdfcraft-cli", "new")]);
                let mut base = runner("0.20.0", &calls, &[]);
                let mut run = |program: &str, args: &[&str], timeout| {
                    if args == ["--version"] {
                        assert_eq!(timeout, Duration::from_secs(5));
                        assert_eq!(calls.borrow().iter().filter(|call| call.starts_with("codesign")).count(), 3, "never execute an unverified tool");
                        if failed_to_run {
                            return Err("version command timed out".into());
                        }
                        Ok(RunOutput {
                            success,
                            code: Some(if success { 0 } else { 1 }),
                            stdout: stdout.into(),
                            stderr: String::new(),
                            status: "version command".into(),
                        })
                    } else {
                        base(program, args, timeout)
                    }
                };
                let mut saved = false;
                let err = super::place(app(), "0.20.0", zip.path(), &paths, old.as_ref(), &mut run, &mut |_| {
                    saved = true;
                    Ok(())
                })
                .unwrap_err();
                assert!(err.contains("version"), "{err}");
                assert!(!saved);
                assert_eq!(std::fs::read(paths.manifest()).unwrap(), before);
                if let Some(old) = old {
                    assert_eq!(std::fs::read_to_string(&old.link).unwrap(), "old");
                    assert_eq!(std::fs::read_link(&old.link).unwrap(), old.folder.join("pdfcraft-cli"));
                } else {
                    assert!(!paths.cli().join("pdfcraft").exists());
                    assert!(std::fs::symlink_metadata(paths.cli_bin().join("pdfcraft-cli")).is_err());
                }
            }
        }
    }

    #[test]
    fn a_tool_that_fails_a_check_changes_nothing() {
        for (failing, what) in [(0, "signature integrity"), (1, "developer identity"), (2, "notarization")] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let old = place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap();
            let mut verdicts = [true; 3];
            verdicts[failing] = false;
            let calls = RefCell::new(Vec::new());
            let err = place(app(), "0.20.0", &[("pdfcraft-cli", "new")], "DJ6XS33FX8", &paths, Some(&old), &mut runner("0.20.0", &calls, &verdicts))
                .unwrap_err();
            assert!(err.contains(what) && err.contains("wasn't installed"), "{err}");
            assert_eq!(std::fs::read_to_string(old.folder.join("pdfcraft-cli")).unwrap(), "old", "the old tool stays after a failed {what}");
            assert!(std::fs::read_link(&old.link).is_ok());
        }
    }

    #[test]
    fn a_download_that_is_not_the_tool_or_holds_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let err = place(app(), "0.20.0", &[("LICENSE", "x")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap_err();
        assert!(err.contains("doesn't contain pdfcraft-cli"), "{err}");
        assert!(!paths.cli().join("pdfcraft").exists());

        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(file.reopen().unwrap());
        zip.add_symlink("evil", "/etc/passwd", zip::write::SimpleFileOptions::default()).unwrap();
        zip.finish().unwrap();
        let err = super::place(app(), "0.20.0", file.path(), &paths, None, &mut runner("0.20.0", &calls, &[]), &mut |_| Ok(())).unwrap_err();
        assert!(err.contains("link"), "{err}");
    }

    #[test]
    fn a_link_that_is_a_file_of_the_users_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli_bin()).unwrap();
        std::fs::write(paths.cli_bin().join("pdfcraft-cli"), "mine").unwrap();
        let calls = RefCell::new(Vec::new());
        let err = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap_err();
        assert!(err.contains("wasn't made by ArtCraft Launcher"), "{err}");
        assert_eq!(std::fs::read_to_string(paths.cli_bin().join("pdfcraft-cli")).unwrap(), "mine");
    }

    #[test]
    fn a_folder_of_the_users_at_the_destination_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli().join("pdfcraft")).unwrap();
        std::fs::write(paths.cli().join("pdfcraft").join("keep"), "mine").unwrap();
        let calls = RefCell::new(Vec::new());
        let err = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap_err();
        assert!(err.contains("wasn't made by ArtCraft Launcher"), "{err}");
        assert_eq!(std::fs::read_to_string(paths.cli().join("pdfcraft").join("keep")).unwrap(), "mine");
        // A record naming that very folder makes it the launcher's to replace.
        let record =
            CliRecord { version: "0.19.0".into(), folder: paths.cli().join("pdfcraft"), link: paths.cli_bin().join("pdfcraft-cli"), installed_at: 0 };
        place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, Some(&record), &mut runner("0.20.0", &calls, &[])).unwrap();
    }

    #[test]
    fn a_link_of_the_users_at_the_name_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli_bin()).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", paths.cli_bin().join("pdfcraft-cli")).unwrap();
        let calls = RefCell::new(Vec::new());
        let err = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap_err();
        assert!(err.contains("wasn't made by ArtCraft Launcher"), "{err}");
        assert_eq!(std::fs::read_link(paths.cli_bin().join("pdfcraft-cli")).unwrap(), Path::new("/etc/hosts"));
        assert!(!paths.cli().join("pdfcraft").exists(), "nothing was put in place");
    }

    #[test]
    fn an_update_blocked_at_the_link_keeps_the_old_tool() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let old = place(app(), "0.19.0", &[("pdfcraft-cli", "old")], "DJ6XS33FX8", &paths, None, &mut runner("0.19.0", &calls, &[])).unwrap();
        // The user replaces the link with a file of their own.
        std::fs::remove_file(&old.link).unwrap();
        std::fs::write(&old.link, "mine").unwrap();
        let err =
            place(app(), "0.20.0", &[("pdfcraft-cli", "new")], "DJ6XS33FX8", &paths, Some(&old), &mut runner("0.20.0", &calls, &[])).unwrap_err();
        assert!(err.contains("wasn't made by ArtCraft Launcher"), "{err}");
        assert_eq!(std::fs::read_to_string(old.folder.join("pdfcraft-cli")).unwrap(), "old", "the old tool is still there");
        assert_eq!(std::fs::read_to_string(&old.link).unwrap(), "mine");
    }

    #[test]
    fn nothing_is_removed_through_a_linked_cli_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let record = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap();
        // The launcher's `cli` folder is later replaced by a link to a folder that holds a `pdfcraft` of its own.
        let real = dir.path().join("data").join("real-cli");
        std::fs::rename(paths.cli(), &real).unwrap();
        std::os::unix::fs::symlink(&real, paths.cli()).unwrap();
        let err = remove(&paths, app(), &record).unwrap_err();
        assert!(err.contains("isn't a real folder"), "{err}");
        assert!(real.join("pdfcraft").join("pdfcraft-cli").exists(), "the folder behind the link is untouched");
    }

    #[test]
    fn a_linked_cli_or_bin_folder_is_refused_before_anything_is_placed() {
        for linked in ["cli", "bin"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let elsewhere = dir.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            if linked == "cli" {
                std::fs::create_dir_all(dir.path().join("data")).unwrap();
                std::os::unix::fs::symlink(&elsewhere, paths.cli()).unwrap();
            } else {
                std::fs::create_dir_all(paths.cli()).unwrap();
                std::os::unix::fs::symlink(&elsewhere, paths.cli_bin()).unwrap();
            }
            let calls = RefCell::new(Vec::new());
            let err =
                place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap_err();
            assert!(err.contains("is a link"), "{linked}: {err}");
            assert!(calls.borrow().is_empty(), "nothing was unpacked or checked");
            assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing was written through the {linked} link");
        }
    }

    #[test]
    fn the_link_step_itself_refuses_a_file_that_is_not_the_launchers() {
        // The check before the swap can be out of date by the time the link is made: link() decides again.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli_bin()).unwrap();
        std::fs::write(paths.cli_bin().join("pdfcraft-cli"), "appeared late").unwrap();
        assert!(link(&paths, "pdfcraft-cli", &paths.cli().join("pdfcraft").join("pdfcraft-cli")).is_err());
        assert_eq!(std::fs::read_to_string(paths.cli_bin().join("pdfcraft-cli")).unwrap(), "appeared late");
    }

    #[test]
    fn a_link_to_another_managed_tool_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli_bin()).unwrap();
        let name = paths.cli_bin().join("pdfcraft-cli");
        let target = paths.cli().join("photocraft/photocraft-cli");
        std::os::unix::fs::symlink(&target, &name).unwrap();
        assert!(link(&paths, "pdfcraft-cli", &paths.cli().join("pdfcraft/pdfcraft-cli")).is_err());
        assert_eq!(std::fs::read_link(name).unwrap(), target);
    }

    #[test]
    fn removal_takes_only_what_the_launcher_installed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let calls = RefCell::new(Vec::new());
        let record = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap();
        let other = paths.cli_bin().join("other-cli");
        std::fs::write(&other, "not ours").unwrap();
        remove(&paths, app(), &record).unwrap();
        assert!(!record.folder.exists());
        assert!(std::fs::symlink_metadata(&record.link).is_err());
        assert!(other.exists(), "a file beside the link stays");

        // A record pointing outside the launcher's folder removes nothing.
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        let wild = CliRecord { version: "1".into(), folder: outside.clone(), link: paths.cli_bin().join("nolink"), installed_at: 0 };
        remove(&paths, app(), &wild).unwrap_err();
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn inaccessible_cli_entries_fail_removal_and_keep_the_tool_and_link() {
        use std::os::unix::fs::PermissionsExt;
        for block_root in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let calls = RefCell::new(Vec::new());
            let record = place(app(), "0.20.0", &[("pdfcraft-cli", "tool")], "DJ6XS33FX8", &paths, None, &mut runner("0.20.0", &calls, &[])).unwrap();
            let blocked = if block_root { paths.cli() } else { paths.cli_bin() };
            let mode = std::fs::metadata(&blocked).unwrap().permissions();
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o600)).unwrap();
            let result = remove(&paths, app(), &record);
            std::fs::set_permissions(&blocked, mode).unwrap();
            let err = result.unwrap_err();
            let inaccessible = if block_root { &record.folder } else { &record.link };
            assert!(err.contains(&format!("couldn't inspect {}", inaccessible.display())), "{err}");
            assert_eq!(std::fs::read_to_string(record.folder.join("pdfcraft-cli")).unwrap(), "tool");
            assert_eq!(std::fs::read_link(&record.link).unwrap(), record.folder.join("pdfcraft-cli"));
            remove(&paths, app(), &record).unwrap();
        }
    }

    #[test]
    fn corrupt_removal_records_cannot_remove_parent_or_another_apps_tool() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.cli_bin()).unwrap();
        std::fs::write(paths.data.join("keep"), "user data").unwrap();
        let other = paths.cli().join("deckcraft");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("deckcraft-cli"), "other app").unwrap();
        for (folder, link) in [
            (paths.cli().join(".."), paths.cli_bin().join("pdfcraft-cli")),
            (other.clone(), paths.cli_bin().join("deckcraft-cli")),
            (paths.cli().join("pdfcraft"), paths.cli_bin().join("deckcraft-cli")),
        ] {
            let record = CliRecord { version: "0.20.0".into(), folder, link, installed_at: 0 };
            assert!(remove(&paths, app(), &record).is_err());
            assert_eq!(std::fs::read_to_string(paths.data.join("keep")).unwrap(), "user data");
            assert_eq!(std::fs::read_to_string(other.join("deckcraft-cli")).unwrap(), "other app");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a checksum-verified PdfCraft v0.20.0 CLI zip in ARTCRAFT_LAUNCHER_TEST_CLI_ZIP and real macOS signing services"]
    fn published_macos_cli_installs_runs_updates_and_removes() {
        let zip = PathBuf::from(
            std::env::var_os("ARTCRAFT_LAUNCHER_TEST_CLI_ZIP").expect("set ARTCRAFT_LAUNCHER_TEST_CLI_ZIP to the verified release zip"),
        );
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut manifest = crate::manifest::Manifest::default();
        let mut existing: Option<CliRecord> = None;
        let stable_link = paths.cli_bin().join("pdfcraft-cli");
        for _ in 0..2 {
            let record = super::place(app(), "0.20.0", &zip, &paths, existing.as_ref(), &mut crate::process::run_with_output, &mut |record| {
                let mut proposed = manifest.clone();
                proposed.cli.insert(app().id.into(), record.clone());
                proposed.save(&paths.manifest())?;
                manifest = proposed;
                Ok(())
            })
            .unwrap();
            assert_eq!(record.link, stable_link);
            assert_eq!(std::fs::read_link(&stable_link).unwrap(), record.folder.join("pdfcraft-cli"));
            assert_eq!(crate::manifest::Manifest::load(&paths.manifest()).cli.get(app().id), Some(&record));
            let output = crate::process::run_with_output(stable_link.to_str().unwrap(), &["--version"], TIMEOUT).unwrap();
            assert!(output.success, "{}: {}", output.status, output.stderr);
            assert!(output.stdout.split_whitespace().any(|part| part == "0.20.0"), "{}", output.stdout);
            println!("installed, verified and ran {}", output.stdout.trim());
            existing = Some(record);
        }
        let record = existing.unwrap();
        remove(&paths, app(), &record).unwrap();
        manifest.cli.remove(app().id);
        manifest.save(&paths.manifest()).unwrap();
        assert!(!record.folder.exists());
        assert!(std::fs::symlink_metadata(&stable_link).is_err());
        assert!(crate::manifest::Manifest::load(&paths.manifest()).cli.is_empty());
    }

    #[test]
    fn the_version_in_the_name_and_the_url_must_be_the_releases() {
        let release = |name: &str, url: &str| Release {
            tag: "v0.20.0".into(),
            version: version::parse("0.20.0"),
            assets: vec![Asset { name: name.into(), size: 1, url: url.into(), sha256: None }],
            ..Release::default()
        };
        let good = release(
            "pdfcraft-cli-0.20.0-macos-universal.zip",
            "https://github.com/x/pdfcraft/releases/download/v0.20.0/pdfcraft-cli-0.20.0-macos-universal.zip",
        );
        assert!(bind(app(), &good, &good.assets[0]).is_ok());
        let stale = release(
            "pdfcraft-cli-0.19.0-macos-universal.zip",
            "https://github.com/x/pdfcraft/releases/download/v0.20.0/pdfcraft-cli-0.19.0-macos-universal.zip",
        );
        assert!(bind(app(), &stale, &stale.assets[0]).is_err(), "an older tool under the new release");
        let other_tag = release(
            "pdfcraft-cli-0.20.0-macos-universal.zip",
            "https://github.com/x/pdfcraft/releases/download/v0.19.0/pdfcraft-cli-0.20.0-macos-universal.zip",
        );
        assert!(bind(app(), &other_tag, &other_tag.assets[0]).is_err(), "a url from another release");
        assert_eq!(
            asset(app(), &good, Platform { os: Os::MacOs, arch: Arch::Arm64 }).map(|a| a.name.as_str()),
            Some("pdfcraft-cli-0.20.0-macos-universal.zip")
        );
    }

    #[test]
    fn the_path_line_the_interface_shows_names_the_link_folder() {
        // On macOS the launcher's data folder is `~/Library/Application Support/ArtCraft Launcher`.
        let paths = Paths { data: PathBuf::from("/Users/sam/Library/Application Support/ArtCraft Launcher"), cache: PathBuf::from("/c") };
        let line = artcraft_launcher_core::protocol::MAC_CLI_PATH_LINE.replace("$HOME", "/Users/sam");
        assert_eq!(line, format!("export PATH=\"{}:$PATH\"", paths.cli_bin().display()));
    }
}
