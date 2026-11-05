use std::io::{Cursor, Write};

use zip::{CompressionMethod, System, ZipWriter, write::SimpleFileOptions};

#[path = "../src/sources/archive.rs"]
mod archive;

use archive::{
    ArchiveEntryKind, ArchiveError, ArchiveLimits, ArchivePathError, EntryMetadataProblem,
    validate_zip_archive, validate_zip_archive_with_limits,
};

const LOCAL_FILE_HEADER: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const CENTRAL_DIRECTORY_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const END_OF_CENTRAL_DIRECTORY: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const ZIP64_END_OF_CENTRAL_DIRECTORY: [u8; 4] = [0x50, 0x4b, 0x06, 0x06];
const ZIP64_END_LOCATOR: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];

fn options(method: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o100644)
}

fn archive_with_entries(entries: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in entries {
        writer
            .start_file(*name, options(method))
            .expect("test ZIP entry starts");
        writer
            .write_all(contents)
            .expect("test ZIP entry contents write");
    }
    writer.finish().expect("test ZIP finishes").into_inner()
}

fn archive_with_file_options(
    name: &str,
    contents: &[u8],
    file_options: SimpleFileOptions,
) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(name, file_options)
        .expect("test ZIP entry starts");
    writer
        .write_all(contents)
        .expect("test ZIP entry contents write");
    writer.finish().expect("test ZIP finishes").into_inner()
}

fn archive_with_raw_name(raw_name: &[u8], contents: &[u8]) -> Vec<u8> {
    let safe_name = "a".repeat(raw_name.len());
    let mut bytes = archive_with_entries(&[(&safe_name, contents)], CompressionMethod::STORE);
    replace_entry_names(&mut bytes, &[raw_name]);
    bytes
}

fn signature_positions(bytes: &[u8], signature: [u8; 4]) -> Vec<usize> {
    bytes
        .windows(signature.len())
        .enumerate()
        .filter_map(|(offset, candidate)| (candidate == signature).then_some(offset))
        .collect()
}

fn replace_entry_names(bytes: &mut [u8], names: &[&[u8]]) {
    let local_headers = signature_positions(bytes, LOCAL_FILE_HEADER);
    let central_headers = signature_positions(bytes, CENTRAL_DIRECTORY_HEADER);
    assert_eq!(local_headers.len(), names.len());
    assert_eq!(central_headers.len(), names.len());

    for ((local, central), name) in local_headers.iter().zip(&central_headers).zip(names) {
        let local_name_len = usize::from(u16_at(bytes, *local + 26));
        let central_name_len = usize::from(u16_at(bytes, *central + 28));
        assert_eq!(local_name_len, name.len());
        assert_eq!(central_name_len, name.len());
        bytes[*local + 30..*local + 30 + name.len()].copy_from_slice(name);
        bytes[*central + 46..*central + 46 + name.len()].copy_from_slice(name);
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn set_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn add_central_extra_field(bytes: &mut Vec<u8>, header_id: u16, data: &[u8]) {
    let central = signature_positions(bytes, CENTRAL_DIRECTORY_HEADER)[0];
    let name_bytes = usize::from(u16_at(bytes, central + 28));
    let old_extra_bytes = usize::from(u16_at(bytes, central + 30));
    let insert_at = central + 46 + name_bytes + old_extra_bytes;
    let mut extra = Vec::with_capacity(data.len() + 4);
    extra.extend_from_slice(&header_id.to_le_bytes());
    extra.extend_from_slice(&(data.len() as u16).to_le_bytes());
    extra.extend_from_slice(data);
    bytes.splice(insert_at..insert_at, extra);
    set_u16(
        bytes,
        central + 30,
        old_extra_bytes as u16 + (data.len() as u16) + 4,
    );
    let eocd = *signature_positions(bytes, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let old_central_bytes = u32_at(bytes, eocd + 12);
    set_u32(bytes, eocd + 12, old_central_bytes + data.len() as u32 + 4);
}

fn add_local_extra_field(bytes: &mut Vec<u8>, header_id: u16, data: &[u8]) {
    let local = signature_positions(bytes, LOCAL_FILE_HEADER)[0];
    let name_bytes = usize::from(u16_at(bytes, local + 26));
    let old_extra_bytes = usize::from(u16_at(bytes, local + 28));
    let insert_at = local + 30 + name_bytes + old_extra_bytes;
    let mut extra = Vec::with_capacity(data.len() + 4);
    extra.extend_from_slice(&header_id.to_le_bytes());
    extra.extend_from_slice(&(data.len() as u16).to_le_bytes());
    extra.extend_from_slice(data);
    let added_bytes = extra.len() as u32;
    bytes.splice(insert_at..insert_at, extra);
    set_u16(
        bytes,
        local + 28,
        old_extra_bytes as u16 + added_bytes as u16,
    );
    let eocd = *signature_positions(bytes, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let central_offset = u32_at(bytes, eocd + 16);
    set_u32(bytes, eocd + 16, central_offset + added_bytes);
}

fn add_malformed_local_extra(bytes: &mut Vec<u8>, data: &[u8]) {
    let local = signature_positions(bytes, LOCAL_FILE_HEADER)[0];
    let name_bytes = usize::from(u16_at(bytes, local + 26));
    let old_extra_bytes = usize::from(u16_at(bytes, local + 28));
    let insert_at = local + 30 + name_bytes + old_extra_bytes;
    let added_bytes = data.len() as u32;
    bytes.splice(insert_at..insert_at, data.iter().copied());
    set_u16(
        bytes,
        local + 28,
        old_extra_bytes as u16 + added_bytes as u16,
    );
    let eocd = *signature_positions(bytes, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let central_offset = u32_at(bytes, eocd + 16);
    set_u32(bytes, eocd + 16, central_offset + added_bytes);
}

fn with_zip64_declared_entry_count(bytes: &[u8], count: u64) -> Vec<u8> {
    let eocd = *signature_positions(bytes, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let central_size = u32_at(bytes, eocd + 12);
    let central_offset = u32_at(bytes, eocd + 16);
    let zip64_position = eocd;
    let locator_position = zip64_position + 56;
    let eocd_position = locator_position + 20;
    let mut output = Vec::with_capacity(bytes.len() + 76);
    output.extend_from_slice(&bytes[..eocd]);

    let mut zip64_end = [0_u8; 56];
    zip64_end[..4].copy_from_slice(&ZIP64_END_OF_CENTRAL_DIRECTORY);
    set_u64(&mut zip64_end, 4, 44);
    set_u16(&mut zip64_end, 12, 45);
    set_u16(&mut zip64_end, 14, 45);
    set_u64(&mut zip64_end, 24, count);
    set_u64(&mut zip64_end, 32, count);
    set_u64(&mut zip64_end, 40, u64::from(central_size));
    set_u64(&mut zip64_end, 48, u64::from(central_offset));
    output.extend_from_slice(&zip64_end);

    let mut locator = [0_u8; 20];
    locator[..4].copy_from_slice(&ZIP64_END_LOCATOR);
    set_u64(&mut locator, 8, zip64_position as u64);
    set_u32(&mut locator, 16, 1);
    output.extend_from_slice(&locator);

    let mut classic_eocd = bytes[eocd..].to_vec();
    set_u16(&mut classic_eocd, 8, u16::MAX);
    set_u16(&mut classic_eocd, 10, u16::MAX);
    set_u32(&mut classic_eocd, 12, u32::MAX);
    set_u32(&mut classic_eocd, 16, u32::MAX);
    assert_eq!(output.len(), eocd_position);
    output.extend_from_slice(&classic_eocd);
    output
}

fn assert_rejected(bytes: &[u8]) -> ArchiveError {
    match validate_zip_archive(bytes) {
        Ok(manifest) => panic!("invalid archive returned a manifest: {manifest:?}"),
        Err(error) => error,
    }
}

#[test]
fn accepts_one_skill_with_nested_assets_and_ordinary_scripts() {
    let archive = archive_with_entries(
        &[
            (
                "demo/SKILL.md",
                b"---\nname: demo\ndescription: fixture\n---\n",
            ),
            ("demo/references/guide.md", b"reference"),
            ("demo/assets/template.bin", b"\0\xff\x10binary"),
            ("demo/scripts/check.sh", b"#!/bin/sh\nexit 71\n"),
        ],
        CompressionMethod::STORE,
    );

    let manifest = validate_zip_archive(&archive).expect("valid nested skill archive");
    assert_eq!(manifest.explicit_entry_count(), 4);
    assert!(manifest.total_uncompressed_bytes() > 0);
    assert!(manifest.compressed_archive_bytes() > 0);
    assert!(manifest.entries().iter().any(|entry| {
        entry.path().as_str() == "demo/references/guide.md"
            && entry.kind() == ArchiveEntryKind::File
            && entry.is_explicit()
            && entry.path().depth() == 3
            && entry.compressed_bytes() > 0
            && entry.crc32() != 0
    }));
    assert!(manifest.entries().iter().any(|entry| {
        entry.path().as_str() == "demo/scripts/check.sh" && entry.kind() == ArchiveEntryKind::File
    }));
    assert!(manifest.entries().iter().any(|entry| {
        entry.path().as_str() == "demo/references"
            && entry.kind() == ArchiveEntryKind::Directory
            && !entry.is_explicit()
    }));
}

#[test]
fn accepts_empty_directories_and_binary_assets() {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_directory(
            "skill/empty/",
            SimpleFileOptions::default().unix_permissions(0o040755),
        )
        .expect("empty directory starts");
    writer
        .start_file("skill/data.bin", options(CompressionMethod::STORE))
        .expect("binary file starts");
    writer
        .write_all(&[0, 255, 1, 0, 128])
        .expect("binary bytes write");
    let archive = writer.finish().expect("test ZIP finishes").into_inner();

    let manifest = validate_zip_archive(&archive).expect("valid empty directory archive");
    let empty = manifest
        .entries()
        .iter()
        .find(|entry| entry.path().as_str() == "skill/empty")
        .expect("explicit empty directory is listed");
    assert_eq!(empty.kind(), ArchiveEntryKind::Directory);
    assert!(empty.is_explicit());
    assert_eq!(empty.uncompressed_bytes(), 0);
}

#[test]
fn accepts_only_metadata_empty_dos_regular_files_as_the_github_compatibility_case() {
    let dos_file = archive_with_file_options(
        "repository/ordinary.txt",
        b"ordinary data",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Dos)
            .external_attributes(0),
    );
    let manifest = validate_zip_archive(&dos_file).expect("plain DOS regular file is supported");
    assert!(manifest.entries().iter().any(|entry| {
        entry.path().as_str() == "repository/ordinary.txt"
            && entry.kind() == ArchiveEntryKind::File
            && entry.is_explicit()
    }));

    let unix_without_type = archive_with_file_options(
        "repository/unknown.txt",
        b"unknown type",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Unix)
            .external_attributes(0),
    );
    assert!(matches!(
        assert_rejected(&unix_without_type),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::UnknownType,
            ..
        }
    ));

    let dos_directory_without_marker = archive_with_file_options(
        "repository/directory/",
        b"",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Dos)
            .external_attributes(0),
    );
    assert!(matches!(
        assert_rejected(&dos_directory_without_marker),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::UnknownType,
            ..
        }
    ));

    let directory_without_name_marker = archive_with_file_options(
        "repository/looks-like-a-file",
        b"",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Dos)
            .external_attributes(0x10),
    );
    assert!(matches!(
        assert_rejected(&directory_without_name_marker),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::NameTypeMismatch,
            ..
        }
    ));

    let unknown_system = archive_with_file_options(
        "repository/unknown-system.txt",
        b"data",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Unknown)
            .external_attributes(0),
    );
    assert!(matches!(
        assert_rejected(&unknown_system),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::UnknownType,
            ..
        }
    ));
}

#[test]
fn preserves_github_style_repository_prefix_without_guessing_a_source_root() {
    let archive = archive_with_entries(
        &[(
            "sample-repo-0123456789/skills/demo/SKILL.md",
            b"skill metadata",
        )],
        CompressionMethod::STORE,
    );

    let manifest = validate_zip_archive(&archive).expect("valid GitHub-style zipball");
    assert!(
        manifest.entries().iter().any(|entry| {
            entry.path().as_str() == "sample-repo-0123456789/skills/demo/SKILL.md"
        })
    );
    assert!(
        !manifest
            .entries()
            .iter()
            .any(|entry| entry.path().as_str() == "skills/demo/SKILL.md")
    );
}

#[test]
fn rejects_absolute_traversal_windows_and_ambiguous_paths() {
    let names: &[&[u8]] = &[
        b"../escape",
        b"a/../../escape",
        b"/etc/passwd",
        b"//server/share/file",
        b"C:/escape",
        b"C:\\escape",
        b"\\\\server\\share\\file",
        b"dir\\..\\escape",
        b"file:stream",
        b"CON",
        b"con.txt",
        b"NUL.md",
        b"LPT1.log",
        b"name.",
        b"name ",
        b"a//b",
        b"a/./b",
        b"a/../b",
        b"bad\0name",
        b"bad\nname",
        b"nonascii-\xff",
        b"bad?name",
    ];

    for name in names {
        let archive = archive_with_raw_name(name, b"data");
        assert!(
            matches!(assert_rejected(&archive), ArchiveError::InvalidPath { .. }),
            "hostile path {name:?} was not rejected as a path"
        );
    }
}

#[test]
fn rejects_duplicate_names_and_case_fold_collisions() {
    let mut duplicate = archive_with_entries(
        &[("first", b"one"), ("other", b"two")],
        CompressionMethod::STORE,
    );
    replace_entry_names(&mut duplicate, &[b"first", b"first"]);
    assert!(matches!(
        assert_rejected(&duplicate),
        ArchiveError::DuplicateCentralDirectoryName
    ));

    let case_collision = archive_with_entries(
        &[("Readme.md", b"one"), ("README.MD", b"two")],
        CompressionMethod::STORE,
    );
    assert!(matches!(
        assert_rejected(&case_collision),
        ArchiveError::CaseFoldCollision { .. }
    ));

    let parent_case_collision = archive_with_entries(
        &[("Assets/one.bin", b"one"), ("assets/two.bin", b"two")],
        CompressionMethod::STORE,
    );
    assert!(matches!(
        assert_rejected(&parent_case_collision),
        ArchiveError::CaseFoldCollision { .. }
    ));
}

#[test]
fn rejects_file_directory_conflicts_including_implied_parents() {
    let archive = archive_with_entries(
        &[("parent", b"file"), ("parent/child.txt", b"child")],
        CompressionMethod::STORE,
    );
    assert!(matches!(
        assert_rejected(&archive),
        ArchiveError::FileDirectoryConflict { .. }
    ));
}

#[test]
fn rejects_symlinks_and_special_unix_file_types() {
    let symlink = archive_with_file_options(
        "skill/link",
        b"../../outside",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Unix)
            .external_attributes(0o120777 << 16),
    );
    assert!(matches!(
        assert_rejected(&symlink),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::SymbolicLink,
            ..
        }
    ));

    let fifo = archive_with_file_options(
        "skill/pipe",
        b"",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Unix)
            .external_attributes(0o010644 << 16),
    );
    assert!(matches!(
        assert_rejected(&fifo),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::SpecialFile,
            ..
        }
    ));

    let dos_tagged_symlink = archive_with_file_options(
        "skill/dos-link",
        b"outside",
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::STORE)
            .system(System::Dos)
            .external_attributes(0o120777 << 16),
    );
    assert!(matches!(
        assert_rejected(&dos_tagged_symlink),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::SymbolicLink,
            ..
        }
    ));

    let mut asi_link =
        archive_with_entries(&[("skill/asi-link", b"outside")], CompressionMethod::STORE);
    add_central_extra_field(&mut asi_link, 0x756e, &[0_u8; 14]);
    assert!(matches!(
        assert_rejected(&asi_link),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::LinkMetadata,
            ..
        }
    ));

    let mut local_asi_link = archive_with_entries(
        &[("skill/local-asi-link", b"outside")],
        CompressionMethod::STORE,
    );
    add_local_extra_field(&mut local_asi_link, 0x756e, &[0_u8; 14]);
    assert!(matches!(
        assert_rejected(&local_asi_link),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::LinkMetadata,
            ..
        }
    ));

    let mut malformed_extra = archive_with_entries(&[("file", b"data")], CompressionMethod::STORE);
    add_malformed_local_extra(&mut malformed_extra, &[0x17]);
    assert!(matches!(
        assert_rejected(&malformed_extra),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::ExtraFieldMalformed,
            ..
        }
    ));
}

#[test]
fn rejects_unsupported_compression_and_encryption_flags() {
    let mut unsupported = archive_with_entries(&[("file", b"data")], CompressionMethod::STORE);
    let local = signature_positions(&unsupported, LOCAL_FILE_HEADER)[0];
    let central = signature_positions(&unsupported, CENTRAL_DIRECTORY_HEADER)[0];
    set_u16(&mut unsupported, local + 8, 99);
    set_u16(&mut unsupported, central + 10, 99);
    assert!(matches!(
        assert_rejected(&unsupported),
        ArchiveError::UnsupportedCompression { method: 99, .. }
    ));

    let mut encrypted = archive_with_entries(&[("file", b"data")], CompressionMethod::STORE);
    let local = signature_positions(&encrypted, LOCAL_FILE_HEADER)[0];
    let central = signature_positions(&encrypted, CENTRAL_DIRECTORY_HEADER)[0];
    let local_flags = u16_at(&encrypted, local + 6) | 1;
    let central_flags = u16_at(&encrypted, central + 8) | 1;
    set_u16(&mut encrypted, local + 6, local_flags);
    set_u16(&mut encrypted, central + 8, central_flags);
    assert!(matches!(
        assert_rejected(&encrypted),
        ArchiveError::EncryptedEntry { .. }
    ));

    let mut aes_encrypted = archive_with_entries(&[("file", b"data")], CompressionMethod::STORE);
    add_central_extra_field(&mut aes_encrypted, 0x9901, &[2_u8, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(
        assert_rejected(&aes_encrypted),
        ArchiveError::EncryptedEntry { .. }
    ));
}

#[test]
fn rejects_malformed_archives_bad_checksums_and_truncated_compressed_data() {
    assert!(matches!(
        assert_rejected(&[0, 1, 2, 3]),
        ArchiveError::InvalidArchive { .. }
    ));

    let mut bad_crc = archive_with_entries(&[("file", b"payload")], CompressionMethod::STORE);
    let local = signature_positions(&bad_crc, LOCAL_FILE_HEADER)[0];
    let central = signature_positions(&bad_crc, CENTRAL_DIRECTORY_HEADER)[0];
    let wrong_crc = u32_at(&bad_crc, central + 16) ^ 1;
    set_u32(&mut bad_crc, local + 14, wrong_crc);
    set_u32(&mut bad_crc, central + 16, wrong_crc);
    assert!(matches!(
        assert_rejected(&bad_crc),
        ArchiveError::CorruptEntry { .. }
    ));

    let mut truncated =
        archive_with_entries(&[("file", &[b'Z'; 16 * 1024])], CompressionMethod::DEFLATE);
    let local = signature_positions(&truncated, LOCAL_FILE_HEADER)[0];
    let compressed_bytes = u32_at(&truncated, local + 18) as usize;
    let name_bytes = usize::from(u16_at(&truncated, local + 26));
    let extra_bytes = usize::from(u16_at(&truncated, local + 28));
    let data_start = local + 30 + name_bytes + extra_bytes;
    assert!(compressed_bytes > 1);
    truncated.remove(data_start + compressed_bytes - 1);
    let local = signature_positions(&truncated, LOCAL_FILE_HEADER)[0];
    let central = signature_positions(&truncated, CENTRAL_DIRECTORY_HEADER)[0];
    let eocd = *signature_positions(&truncated, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let local_compressed_bytes = u32_at(&truncated, local + 18) - 1;
    let central_compressed_bytes = u32_at(&truncated, central + 20) - 1;
    let central_directory_offset = u32_at(&truncated, eocd + 16) - 1;
    set_u32(&mut truncated, local + 18, local_compressed_bytes);
    set_u32(&mut truncated, central + 20, central_compressed_bytes);
    set_u32(&mut truncated, eocd + 16, central_directory_offset);
    assert!(matches!(
        assert_rejected(&truncated),
        ArchiveError::CorruptEntry { .. } | ArchiveError::InvalidArchive { .. }
    ));
}

#[test]
fn enforces_archive_entry_file_total_and_path_budgets() {
    let archive = archive_with_entries(
        &[("a", b"1"), ("b", b"2"), ("c", b"3")],
        CompressionMethod::STORE,
    );
    let limits = ArchiveLimits {
        max_entries: 2,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::TooManyEntries { .. })
    ));

    let archive = archive_with_entries(&[("a/b/c/file", b"x")], CompressionMethod::STORE);
    let limits = ArchiveLimits {
        max_entries: 3,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::TooManyManifestPaths { .. })
    ));

    let archive = archive_with_entries(&[("large", &[b'x'; 100])], CompressionMethod::STORE);
    let limits = ArchiveLimits {
        max_file_uncompressed_bytes: 50,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::DeclaredFileTooLarge { .. })
    ));

    let archive = archive_with_entries(
        &[("one", &[b'x'; 60]), ("two", &[b'y'; 60])],
        CompressionMethod::STORE,
    );
    let limits = ArchiveLimits {
        max_file_uncompressed_bytes: 70,
        max_total_uncompressed_bytes: 100,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::TotalExpandedTooLarge { .. })
    ));

    let archive = archive_with_entries(&[("a/b/c", b"data")], CompressionMethod::STORE);
    let limits = ArchiveLimits {
        max_path_depth: 2,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::InvalidPath {
            source: ArchivePathError::TooDeep { .. },
            ..
        })
    ));

    let limits = ArchiveLimits {
        max_path_bytes: 4,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::InvalidPath {
            source: ArchivePathError::PathTooLong { .. },
            ..
        })
    ));

    let long_component = archive_with_entries(&[("long", b"data")], CompressionMethod::STORE);
    let limits = ArchiveLimits {
        max_component_bytes: 2,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&long_component, limits),
        Err(ArchiveError::InvalidPath {
            source: ArchivePathError::ComponentTooLong { .. },
            ..
        })
    ));
}

#[test]
fn preflights_classic_and_zip64_entry_counts_before_manifest_creation() {
    let archive = archive_with_entries(&[("only", b"x")], CompressionMethod::STORE);
    let eocd = *signature_positions(&archive, END_OF_CENTRAL_DIRECTORY)
        .last()
        .expect("EOCD exists");
    let mut excessive_classic_count = archive.clone();
    set_u16(&mut excessive_classic_count, eocd + 10, 4_097);
    assert!(matches!(
        assert_rejected(&excessive_classic_count),
        ArchiveError::TooManyEntries {
            actual_entries: 4_097,
            ..
        }
    ));

    let excessive_zip64_count = with_zip64_declared_entry_count(&archive, u64::MAX);
    assert!(matches!(
        assert_rejected(&excessive_zip64_count),
        ArchiveError::TooManyEntries {
            actual_entries: u64::MAX,
            ..
        }
    ));
}

#[test]
fn rejects_archives_over_compressed_limit_and_zip_bomb_style_expansion() {
    let archive = archive_with_entries(&[("file", b"x")], CompressionMethod::STORE);
    let limits = ArchiveLimits {
        max_compressed_archive_bytes: (archive.len() as u64) - 1,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&archive, limits),
        Err(ArchiveError::ArchiveTooLarge { .. })
    ));

    let bomb_contents = vec![b'Z'; 64 * 1024];
    let bomb = archive_with_entries(
        &[("compressed-bomb.bin", &bomb_contents)],
        CompressionMethod::DEFLATE,
    );
    assert!(bomb.len() < bomb_contents.len());
    let limits = ArchiveLimits {
        max_file_uncompressed_bytes: 1_024,
        max_total_uncompressed_bytes: 2_048,
        ..ArchiveLimits::default()
    };
    assert!(matches!(
        validate_zip_archive_with_limits(&bomb, limits),
        Err(ArchiveError::DeclaredFileTooLarge { .. })
    ));
}

#[test]
fn rejects_misleading_small_declared_size_instead_of_accepting_a_partial_manifest() {
    let mut archive = archive_with_entries(
        &[("payload.bin", &[b'P'; 8 * 1024])],
        CompressionMethod::STORE,
    );
    let local = signature_positions(&archive, LOCAL_FILE_HEADER)[0];
    let central = signature_positions(&archive, CENTRAL_DIRECTORY_HEADER)[0];
    set_u32(&mut archive, local + 22, 1);
    set_u32(&mut archive, central + 24, 1);

    assert!(matches!(
        assert_rejected(&archive),
        ArchiveError::CorruptEntry { .. }
            | ArchiveError::ExpandedSizeMismatch { .. }
            | ArchiveError::InvalidArchive { .. }
    ));
}

#[test]
fn rejects_inconsistent_file_directory_metadata() {
    let directory_named_file = archive_with_file_options(
        "looks-like-a-directory/",
        b"",
        options(CompressionMethod::STORE),
    );
    assert!(matches!(
        assert_rejected(&directory_named_file),
        ArchiveError::InvalidEntryMetadata {
            problem: EntryMetadataProblem::NameTypeMismatch,
            ..
        }
    ));
}

#[test]
fn manifest_contains_only_metadata_and_validation_does_not_retain_content() {
    let archive = archive_with_entries(
        &[("script.sh", b"#!/bin/sh\nexit 0\n")],
        CompressionMethod::STORE,
    );
    let manifest = validate_zip_archive(&archive).expect("script is an ordinary file");
    let file = manifest
        .entries()
        .iter()
        .find(|entry| entry.path().as_str() == "script.sh")
        .expect("script is listed");
    assert_eq!(file.kind(), ArchiveEntryKind::File);
    assert_eq!(
        file.uncompressed_bytes(),
        b"#!/bin/sh\nexit 0\n".len() as u64
    );
}
