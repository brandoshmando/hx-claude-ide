//! The Claude Code IDE lock file.
//!
//! Claude Code discovers running editors by scanning `~/.claude/ide/*.lock`.
//! The WebSocket port lives in the *filename* (`<port>.lock`); the file *body*
//! is a JSON object Claude reads to get the auth token, workspace, etc. (F1).
//!
//! Nothing here is wired into the FFI entrypoints yet — that happens in T08.

use std::fs;
use std::io::Write;
// These `...Ext` traits add the Unix-only `.mode(...)` methods used below.
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

/// A fresh 128-bit auth token as 32 lowercase hex characters.
///
/// 16 random bytes × 2 hex digits each = 32 chars. Claude compares this against
/// the `x-claude-code-ide-authorization` header on the WebSocket upgrade.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    // Fills `bytes` from the OS CSPRNG. Failure here means the OS has no
    // randomness source, which is effectively impossible on a real host.
    getrandom::getrandom(&mut bytes).expect("OS randomness unavailable");
    // `{:02x}` = hex, zero-padded to 2 digits. Collect the per-byte strings
    // into one String.
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory that holds lock files: `$CLAUDE_CONFIG_DIR/ide` when that env
/// var is set (Claude honors it to relocate its config), else `$HOME/.claude/ide`.
fn ide_dir() -> PathBuf {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").expect("HOME not set");
            PathBuf::from(home).join(".claude")
        }
    };
    base.join("ide")
}

/// An on-disk lock file. Dropping it best-effort removes the file, so a clean
/// shutdown leaves no stale lock behind (a hard crash leaks it, but Claude
/// self-heals by checking the recorded `pid`).
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    /// Writes `<ide_dir>/<port>.lock` for `port`, advertising `workspace` as the
    /// single workspace folder and `token` as the auth secret.
    ///
    /// The directory is created `0700` and the file is `0600`. We write to a
    /// temp file and `rename` it into place so a reader never sees a partial
    /// file (rename is atomic on the same filesystem).
    pub fn write(port: u16, workspace: &str, token: &str) -> std::io::Result<LockFile> {
        let dir = ide_dir();
        fs::DirBuilder::new()
            .recursive(true) // create parents as needed; ok if it already exists
            .mode(0o700) // applied to directories this call creates
            .create(&dir)?;

        // The exact key set Claude Code expects (F1). `json!` builds a
        // serde_json::Value; `.to_string()` serializes it compactly.
        let body = serde_json::json!({
            "pid": std::process::id(),
            "workspaceFolders": [workspace],
            "ideName": "Helix",
            "transport": "ws",
            "authToken": token,
        });

        let path = dir.join(format!("{port}.lock"));
        let tmp = dir.join(format!("{port}.lock.tmp"));

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600) // creation mode for the temp file
            .open(&tmp)?;
        file.write_all(body.to_string().as_bytes())?;
        file.sync_all()?; // flush to disk before the rename
        // Force exactly 0600 regardless of the process umask, then swap in.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, &path)?;

        Ok(LockFile { path })
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Best-effort: ignore errors (the file may already be gone).
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn token_is_32_lowercase_hex() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Two separate tokens should not collide.
        assert_ne!(t, generate_token());
    }

    #[test]
    fn writes_lockfile_with_expected_shape() {
        // Isolate the write under a unique temp CLAUDE_CONFIG_DIR.
        let base = std::env::temp_dir().join(format!("hxcc-test-{}", std::process::id()));
        std::env::set_var("CLAUDE_CONFIG_DIR", &base);

        let token = generate_token();
        let lock = LockFile::write(12345, "/tmp/project", &token).expect("write lock");

        let path = base.join("ide").join("12345.lock");
        assert!(path.exists(), "lock file should exist at {path:?}");

        // File must be 0600.
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file mode should be 0600");

        // Body: exact keys and values (F1).
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["ideName"], "Helix");
        assert_eq!(json["transport"], "ws");
        assert_eq!(json["authToken"], token);
        assert_eq!(json["workspaceFolders"], serde_json::json!(["/tmp/project"]));
        assert!(json["pid"].is_number());
        assert!(json.get("port").is_none(), "port lives in the filename, not the body");

        // Drop removes the file.
        drop(lock);
        assert!(!path.exists(), "drop should remove the lock file");
        let _ = fs::remove_dir_all(&base);
    }
}
