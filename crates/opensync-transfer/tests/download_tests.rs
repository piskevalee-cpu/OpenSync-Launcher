use crc32fast::Hasher as Crc32;
use opensync_transfer::{
    DownloadControl, TransferError, extract_zip_stream, safe_rel_path,
};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Write};
use tempfile::tempdir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Builds a streaming STORE ZIP using general-purpose bit flag 0x0008 (Data Descriptor)
/// matching the exact wire format Node's `archiver` v8 emits.
fn build_streaming_store_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        // Local File Header signature
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0008u16.to_le_bytes()); // bit 3 = data descriptor
        out.extend_from_slice(&0u16.to_le_bytes()); // method 0 = STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc = 0 (deferred)
        out.extend_from_slice(&0u32.to_le_bytes()); // comp = 0 (deferred)
        out.extend_from_slice(&0u32.to_le_bytes()); // uncomp = 0 (deferred)
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        // Data Descriptor: PK\x07\x08 + crc32 + comp_size + uncomp_size
        let mut crc = Crc32::new();
        crc.update(data);
        out.extend_from_slice(&[0x50, 0x4b, 0x07, 0x08]);
        out.extend_from_slice(&crc.finalize().to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    // Central directory terminator marker
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out
}

fn build_standard_zip(entries: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zw = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default().compression_method(method);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn download_stream_extracts_standard_store_zip_with_sha256() {
    let dir = tempdir().unwrap();
    let payload_a = vec![0xABu8; 512 * 1024]; // 512 KB
    let payload_b = b"level=5\nhp=100\n".to_vec();
    let zip_bytes = build_standard_zip(
        &[
            ("game/assets/world.pak", &payload_a),
            ("game/saves/slot1.sav", &payload_b),
        ],
        CompressionMethod::Stored,
    );

    let ctrl = DownloadControl::new(zip_bytes.len() as u64, 2);
    let extracted = extract_zip_stream(Cursor::new(zip_bytes), dir.path(), &ctrl).unwrap();
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted[0].path, "game/assets/world.pak");
    assert_eq!(extracted[0].sha256, sha256_hex(&payload_a));
    assert_eq!(extracted[1].path, "game/saves/slot1.sav");
    assert_eq!(extracted[1].sha256, sha256_hex(&payload_b));

    assert_eq!(
        std::fs::read(dir.path().join("game/assets/world.pak")).unwrap(),
        payload_a
    );
    assert_eq!(
        std::fs::read(dir.path().join("game/saves/slot1.sav")).unwrap(),
        payload_b
    );
}

#[test]
fn download_stream_handles_archiver_data_descriptors_and_decoy_signatures() {
    let dir = tempdir().unwrap();
    // Embed a decoy PK\x07\x08 signature right inside binary game data to verify zero false positives
    let mut binary_with_decoy = vec![0x11u8; 64 * 1024];
    binary_with_decoy[1234..1238].copy_from_slice(&[0x50, 0x4b, 0x07, 0x08]);
    binary_with_decoy[1238..1250].fill(0x22);

    let text_file = b"hello from streaming archiver".to_vec();
    let zip_bytes = build_streaming_store_zip(&[
        ("bin/game.dat", &binary_with_decoy),
        ("readme.txt", &text_file),
    ]);

    let ctrl = DownloadControl::new(0, 2);
    let extracted = extract_zip_stream(Cursor::new(zip_bytes), dir.path(), &ctrl).unwrap();
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted[0].size, binary_with_decoy.len() as u64);
    assert_eq!(extracted[0].sha256, sha256_hex(&binary_with_decoy));
    assert_eq!(extracted[1].sha256, sha256_hex(&text_file));
    assert_eq!(
        std::fs::read(dir.path().join("bin/game.dat")).unwrap(),
        binary_with_decoy
    );
}

#[test]
fn download_stream_supports_deflate_fallback() {
    let dir = tempdir().unwrap();
    let compressible = "OpenSync fast game streaming ".repeat(10_000);
    let zip_bytes = build_standard_zip(
        &[("docs/lore.txt", compressible.as_bytes())],
        CompressionMethod::Deflated,
    );

    let ctrl = DownloadControl::new(compressible.len() as u64, 1);
    let extracted = extract_zip_stream(Cursor::new(zip_bytes), dir.path(), &ctrl).unwrap();
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].sha256, sha256_hex(compressible.as_bytes()));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("docs/lore.txt")).unwrap(),
        compressible
    );
}

#[test]
fn download_rejects_path_traversal_entries() {
    assert!(matches!(
        safe_rel_path("../etc/passwd"),
        Err(TransferError::UnsafePath(_))
    ));
    assert!(matches!(
        safe_rel_path("/abs/path.bin"),
        Err(TransferError::UnsafePath(_))
    ));
    assert!(matches!(
        safe_rel_path("sub\\win\\path.exe"),
        Err(TransferError::UnsafePath(_))
    ));
    assert!(matches!(
        safe_rel_path("a\0b"),
        Err(TransferError::UnsafePath(_))
    ));

    let dir = tempdir().unwrap();
    let evil_zip = build_streaming_store_zip(&[("../../evil.sh", b"rm -rf /")]);
    let ctrl = DownloadControl::new(0, 1);
    let err = extract_zip_stream(Cursor::new(evil_zip), dir.path(), &ctrl).unwrap_err();
    assert!(matches!(err, TransferError::UnsafePath(_)));
}

#[test]
fn download_control_cancels_cleanly_mid_stream() {
    let dir = tempdir().unwrap();
    let big = vec![0x5Au8; 4 * 1024 * 1024]; // 4 MB
    let zip_bytes = build_standard_zip(&[("big.pak", &big)], CompressionMethod::Stored);

    let ctrl = DownloadControl::new(big.len() as u64, 1);
    ctrl.cancel();
    let err = extract_zip_stream(Cursor::new(zip_bytes), dir.path(), &ctrl).unwrap_err();
    assert!(matches!(err, TransferError::Cancelled));
}
