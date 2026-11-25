use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Seek},
    path::{Path, PathBuf},
};

use unicode_normalization::UnicodeNormalization;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const MAX_FILENAME_BYTES: usize = 255;

pub(super) struct Selection {
    filename: String,
    input: UploadInput,
}

enum UploadInput {
    File(Source),
    Archive(Vec<ArchiveEntry>),
}

struct Source {
    path: PathBuf,
    name: String,
    directory: bool,
}

struct ArchiveEntry {
    source: Source,
    name: String,
}

pub(super) fn inspect(paths: Vec<PathBuf>) -> io::Result<Selection> {
    let mut sources = paths
        .into_iter()
        .map(Source::inspect)
        .collect::<io::Result<Vec<_>>>()?;
    let filename = match sources.as_slice() {
        [] => return Err(io::Error::other("Select at least one file or directory")),
        [source] if !source.directory => {
            if source.name.len() > MAX_FILENAME_BYTES
                || source.name.contains('\\')
                || source.name.chars().any(char::is_control)
            {
                return Err(path_error(&source.path, "Invalid upload filename"));
            }
            return Ok(Selection {
                filename: source.name.clone(),
                input: UploadInput::File(sources.pop().expect("single source")),
            });
        }
        [source] => {
            let name = portable_name(&source.name);
            format!("{}.zip", truncate(&name, MAX_FILENAME_BYTES - 4))
        }
        _ => "files.zip".to_owned(),
    };

    // Take the directory listing before creating the temporary ZIP. This also
    // keeps the ZIP out of the selection when a source contains the temp directory.
    let mut pending = Vec::new();
    let mut entries = Vec::new();
    schedule(sources, "", &mut pending);
    while let Some(entry) = pending.pop() {
        if entry.source.directory {
            let path = &entry.source.path;
            let mut children = fs::read_dir(path)
                .and_then(|entries| entries.collect::<io::Result<Vec<_>>>())
                .map_err(|e| path_error(path, e))?;
            children.sort_by_key(|child| child.file_name());
            let sources = children
                .into_iter()
                .map(|child| Source::inspect(child.path()))
                .collect::<io::Result<_>>()?;
            schedule(sources, &entry.name, &mut pending);
        }
        entries.push(entry);
    }
    Ok(Selection {
        filename,
        input: UploadInput::Archive(entries),
    })
}

impl Selection {
    pub(super) fn filename(&self) -> &str {
        &self.filename
    }

    pub(super) fn prepare(self) -> io::Result<(File, u64)> {
        let file = match self.input {
            UploadInput::File(source) => source.open()?,
            UploadInput::Archive(entries) => write_archive(entries)?,
        };
        let size = file.metadata()?.len();
        Ok((file, size))
    }
}

impl Source {
    fn inspect(path: PathBuf) -> io::Result<Self> {
        // Remove trailing separators and `.` so a directory symlink is still
        // identified as a symlink when selected as `link/` or `link/.`.
        let mut path: PathBuf = path.components().collect();
        let directory = directory_kind(&path)?;
        if path.file_name().is_none() {
            path = path.canonicalize().map_err(|e| path_error(&path, e))?;
        }
        let name = path
            .file_name()
            .map(|name| {
                name.to_str()
                    .ok_or_else(|| path_error(&path, "Filename must be valid UTF-8"))
            })
            .transpose()?
            .unwrap_or("files")
            .to_owned();
        Ok(Self {
            path,
            name,
            directory,
        })
    }

    fn check_kind(&self) -> io::Result<()> {
        if directory_kind(&self.path)? == self.directory {
            Ok(())
        } else {
            Err(path_error(
                &self.path,
                "File type changed since confirmation",
            ))
        }
    }

    fn open(&self) -> io::Result<File> {
        self.check_kind()?;
        File::open(&self.path).map_err(|e| path_error(&self.path, e))
    }
}

fn directory_kind(path: &Path) -> io::Result<bool> {
    let kind = fs::symlink_metadata(path)
        .map_err(|e| path_error(path, e))?
        .file_type();
    if kind.is_dir() || kind.is_file() {
        Ok(kind.is_dir())
    } else {
        Err(path_error(path, "Expected a regular file or directory"))
    }
}

fn write_archive(entries: Vec<ArchiveEntry>) -> io::Result<File> {
    let mut zip = ZipWriter::new(tempfile::tempfile()?);
    // Use ZIP64 file headers so streamed files can grow beyond 4 GiB.
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);
    for entry in entries {
        if entry.source.directory {
            entry.source.check_kind()?;
            zip.add_directory(entry.name, SimpleFileOptions::default())
                .map_err(|e| path_error(&entry.source.path, e))?;
        } else {
            let mut input = entry.source.open()?;
            zip.start_file(entry.name, options)
                .map_err(|e| path_error(&entry.source.path, e))?;
            io::copy(&mut input, &mut zip).map_err(|e| path_error(&entry.source.path, e))?;
        }
    }
    let mut file = zip.finish().map_err(io::Error::other)?;
    file.rewind()?;
    Ok(file)
}

fn schedule(sources: Vec<Source>, parent: &str, pending: &mut Vec<ArchiveEntry>) {
    let names: Vec<_> = sources
        .iter()
        .map(|source| portable_name(&source.name))
        .collect();
    // Reserve originals so a suffix cannot take a later sibling's actual name.
    let mut reserved: HashSet<_> = names.iter().map(|name| name_key(name)).collect();
    let mut next_suffix = HashMap::new();
    let start = pending.len();
    for (source, mut name) in sources.into_iter().zip(names) {
        let next = next_suffix.entry(name_key(&name)).or_insert(1);
        if *next == 1 {
            *next = 2;
        } else {
            loop {
                let candidate = suffixed_name(&name, *next);
                *next += 1;
                if reserved.insert(name_key(&candidate)) {
                    name = candidate;
                    break;
                }
            }
        }
        if !parent.is_empty() {
            name = format!("{parent}/{name}");
        }
        pending.push(ArchiveEntry { name, source });
    }
    pending[start..].reverse();
}

fn portable_name(name: &str) -> String {
    // Each ZIP path segment must be extractable on Windows as well as Unix.
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let safe = truncate(safe.trim_start(), MAX_FILENAME_BYTES)
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if safe.is_empty() {
        return "file".to_owned();
    }
    let device = safe
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let port = device
        .strip_prefix("COM")
        .or_else(|| device.strip_prefix("LPT"));
    if matches!(
        device.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || matches!(
        port,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
    ) {
        format!("_{}", truncate(safe, MAX_FILENAME_BYTES - 1))
    } else {
        safe.to_owned()
    }
}

fn suffixed_name(name: &str, suffix: usize) -> String {
    let separator = name
        .rfind('.')
        .filter(|&index| index > 0)
        .unwrap_or(name.len());
    let (stem, extension) = name.split_at(separator);
    let suffix = format!(" ({suffix})");
    let room = MAX_FILENAME_BYTES - suffix.len();
    let extension = truncate(
        extension,
        room.saturating_sub(stem.chars().next().map_or(0, char::len_utf8)),
    );
    format!(
        "{}{suffix}{extension}",
        truncate(stem, room - extension.len())
    )
}

fn truncate(name: &str, bytes: usize) -> &str {
    &name[..name.floor_char_boundary(bytes)]
}

fn name_key(name: &str) -> String {
    name.nfc().flat_map(char::to_lowercase).collect()
}

fn path_error(path: &Path, reason: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{reason}: {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn sends_a_single_file_with_its_original_name_and_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("original.txt");
        fs::write(&path, b"pigeon").unwrap();

        let selection = inspect(vec![path]).unwrap();
        assert_eq!(selection.filename(), "original.txt");
        let (mut file, size) = selection.prepare().unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(size, 6);
        assert_eq!(contents, "pigeon");
    }

    #[test]
    fn archives_a_single_directory_and_preserves_its_empty_folders() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("documents");
        fs::create_dir_all(folder.join("empty")).unwrap();
        fs::write(folder.join("notes.txt"), b"notes").unwrap();

        let selection = inspect(vec![folder]).unwrap();
        assert_eq!(selection.filename(), "documents.zip");
        let (file, size) = selection.prepare().unwrap();
        assert_eq!(file.metadata().unwrap().len(), size);
        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert!(zip.by_name("documents/empty/").unwrap().is_dir());
        let mut notes = String::new();
        zip.by_name("documents/notes.txt")
            .unwrap()
            .read_to_string(&mut notes)
            .unwrap();
        assert_eq!(notes, "notes");
    }

    #[test]
    fn mixed_inputs_keep_original_names_and_number_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let make_file = |parent: &str, name: &str| {
            let parent = temp.path().join(parent);
            fs::create_dir(&parent).unwrap();
            let path = parent.join(name);
            fs::write(&path, name).unwrap();
            path
        };
        let first = make_file("a", "readme.txt");
        let second = make_file("b", "README.txt");
        let numbered = make_file("c", "readme (2).txt");
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();

        let selection = inspect(vec![first, second, numbered, folder]).unwrap();
        assert_eq!(selection.filename(), "files.zip");
        let (file, _) = selection.prepare().unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        let names = archive.file_names().collect::<HashSet<_>>();
        assert_eq!(
            names,
            HashSet::from(["readme.txt", "README (3).txt", "readme (2).txt", "folder/"])
        );
    }

    #[test]
    fn normalizes_portable_names_and_keeps_utf8_suffixes_under_the_byte_limit() {
        assert_eq!(portable_name(".."), "file");
        assert_eq!(portable_name("drive:example\\name"), "drive_example_name");
        assert_eq!(portable_name("AUX.txt"), "_AUX.txt");
        assert_eq!(name_key("É.txt"), name_key("e\u{301}.TXT"));
        let name = portable_name(&"é".repeat(128));
        assert!(suffixed_name(&name, 2).len() <= MAX_FILENAME_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn reports_the_path_of_a_symbolic_link() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        for path in [temp.path().to_owned(), link.join(".")] {
            let error = inspect(vec![path]).err().expect("reject symlink");
            assert!(error.to_string().contains("link"));
        }
    }
}
