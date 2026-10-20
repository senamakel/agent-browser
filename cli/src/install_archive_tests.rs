use super::*;
use std::io::Write;

#[cfg(unix)]
#[test]
fn extract_zip_clears_privileged_bits_and_preserves_executable_mode() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("chrome");

    let mut archive = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    archive
        .start_file(
            "chrome-linux64/chrome",
            zip::write::SimpleFileOptions::default().unix_permissions(0o100755),
        )
        .unwrap();
    archive.write_all(b"executable").unwrap();
    archive
        .start_file(
            "chrome-linux64/privileged",
            zip::write::SimpleFileOptions::default().unix_permissions(0o104755),
        )
        .unwrap();
    archive.write_all(b"privileged").unwrap();
    let bytes = archive.finish().unwrap().into_inner();

    extract_zip(bytes, &dest).unwrap();

    let executable = fs::metadata(dest.join("chrome")).unwrap();
    assert_eq!(executable.permissions().mode() & 0o7777, 0o755);
    let privileged = fs::metadata(dest.join("privileged")).unwrap();
    assert_eq!(privileged.permissions().mode() & 0o7777, 0o755);
}
