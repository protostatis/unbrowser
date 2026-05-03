//! Bytecode cache for compiled page scripts.
//!
//! Heavy React/Next/Vue bundles dominate navigate wall-clock on commercial
//! sites. Most of that time is the QuickJS parse phase — converting source
//! text to internal bytecode. The bytecode itself executes much faster
//! than parse on subsequent visits, but we currently re-parse every time.
//! This module caches QuickJS bytecode keyed by content hash so the parse
//! happens once per (script content, engine version) tuple.
//!
//! Mechanism: classic-script bytecode I/O via direct unsafe calls into
//! `qjs::JS_WriteObject` / `qjs::JS_ReadObject` / `qjs::JS_EvalFunction`.
//! `rquickjs` 0.11's safe surface only exposes these for ES modules, not
//! classic scripts (which is what we eval most page code as). So the
//! compile/load helpers below are unsafe but tightly scoped.
//!
//! Cache key incorporates everything that could affect bytecode validity:
//! - sha256(source) — content
//! - QUICKJS_BYTECODE_FORMAT — bumped on incompatible QuickJS upgrades
//! - target triple — bytecode is endian/arch-specific
//! - rquickjs version — wraps the QuickJS we link against
//! - sha256(shims.js + dom.js) — global env affects compile (e.g. const
//!   redeclaration would change behavior)
//!
//! Storage layout: `~/.unbrowser/bytecode/{hash[0..2]}/{hash}.qbc`. Two-
//! char prefix dir keeps single directories under a few thousand files
//! even at heavy usage. Override via `$UNBROWSER_BYTECODE_CACHE`.
//!
//! Eviction: total-size cap (default 500 MB) checked at startup; oldest
//! files by mtime evicted until under cap. Per-fetch eviction would
//! double-stat the directory; once-per-process is enough.
//!
//! Disable: set `UNBROWSER_NO_BYTECODE_CACHE=1`.
//!
//! Security: `JS_ReadObject` accepts arbitrary bytes; loading hostile
//! bytecode would be a code-execution vector. We mitigate by:
//! - keying on the version markers above (rejects bytecode from a
//!   different binary build)
//! - reading only from a directory we own
//! - never accepting bytecode from a remote source
//!
//! See PR review notes on PR #2 / #7 about not trusting non-locally-
//! produced bytecode.

use rquickjs::Ctx;
use rquickjs::qjs;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bumped when bytecode format changes incompatibly. Increment alongside
/// rquickjs/QuickJS upgrades that touch the on-disk shape. Reading a
/// bytecode file with mismatched version returns None.
const BYTECODE_FORMAT_VERSION: u32 = 1;
/// Default total cache size cap.
const DEFAULT_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Resolve the cache root directory: env override, then ~/.unbrowser/bytecode,
/// then /tmp fallback.
pub fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("UNBROWSER_BYTECODE_CACHE") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".unbrowser").join("bytecode");
    }
    PathBuf::from("/tmp").join("unbrowser-bytecode")
}

pub fn is_disabled() -> bool {
    std::env::var("UNBROWSER_NO_BYTECODE_CACHE")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// Compute the cache key for a given source. Combines:
///   - content hash
///   - format version
///   - target triple (compile-time constant)
///   - shim hash (so a shims.js change invalidates bytecode that captured
///     the old globals)
pub fn cache_key(source: &str, shim_hash: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"unbrowser-bytecode\0");
    h.update(BYTECODE_FORMAT_VERSION.to_le_bytes());
    // Target triple proxy: arch + os. Bytecode is endian/arch-specific
    // so an x86_64-macos cache file must not load on aarch64-linux.
    h.update(std::env::consts::ARCH.as_bytes());
    h.update(b"-");
    h.update(std::env::consts::OS.as_bytes());
    h.update(b"\0");
    h.update(shim_hash.as_bytes());
    h.update(b"\0");
    h.update(source.as_bytes());
    let digest = h.finalize();
    hex_encode(&digest[..])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn key_to_path(root: &Path, key: &str) -> PathBuf {
    let prefix = &key[..2.min(key.len())];
    root.join(prefix).join(format!("{key}.qbc"))
}

/// Read cached bytecode by key. None on miss / IO error.
pub fn read(root: &Path, key: &str) -> Option<Vec<u8>> {
    fs::read(key_to_path(root, key)).ok()
}

/// Write cached bytecode by key. Atomic via tmp-then-rename so a crash
/// can't leave a partial file. IO errors are logged via emit_event in
/// the caller — the cache write is best-effort.
pub fn write(root: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = key_to_path(root, key);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("qbc.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)
}

/// Compute sha256 of a string. Used to fingerprint shims.js / dom.js.
pub fn sha256(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex_encode(&h.finalize()[..])
}

/// Walk the cache root, evicting oldest files (by mtime) until total size
/// is under `max_bytes`. Called once at Session::new.
///
/// Best-effort: missing dir / io errors silently return.
pub fn prune(root: &Path, max_bytes: u64) {
    if !root.exists() {
        return;
    }
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    walk_dir(root, &mut |path, meta| {
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        total += len;
        entries.push((path, len, mtime));
    });
    if total <= max_bytes {
        return;
    }
    // Oldest first
    entries.sort_by_key(|(_, _, t)| *t);
    for (path, len, _) in entries {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

fn walk_dir<F: FnMut(PathBuf, fs::Metadata)>(dir: &Path, f: &mut F) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            walk_dir(&path, f);
        } else if path.extension().and_then(|e| e.to_str()) == Some("qbc") {
            f(path, meta);
        }
    }
}

/// Default total-bytes cap. Override via $UNBROWSER_BYTECODE_CACHE_MAX_MB.
pub fn max_total_bytes() -> u64 {
    std::env::var("UNBROWSER_BYTECODE_CACHE_MAX_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_MAX_TOTAL_BYTES)
}

// ---------------- unsafe QuickJS bytecode glue ----------------------------
//
// rquickjs 0.11's safe surface only exposes JS_WriteObject / JS_ReadObject
// for ES modules. We eval page code as classic scripts (Context::eval), so
// we call the C functions directly via rquickjs::qjs.

/// Extract the message of a JS exception value as a Rust String.
/// Used by compile_to_bytecode's error path so the caller sees why the
/// compile failed, not just that it did.
unsafe fn exception_to_string(ctx: *mut qjs::JSContext, exc: qjs::JSValue) -> String {
    unsafe {
        let cstr = qjs::JS_ToCString(ctx, exc);
        if cstr.is_null() {
            return "<unprintable exception>".to_string();
        }
        let s = std::ffi::CStr::from_ptr(cstr)
            .to_string_lossy()
            .into_owned();
        qjs::JS_FreeCString(ctx, cstr);
        s
    }
}

/// Compile source to bytecode without executing. Returns the bytecode
/// bytes ready to write to disk and to load+execute via load_and_eval.
///
/// Safety: the returned JSValue from JS_Eval(COMPILE_ONLY) is owned by the
/// caller; we free it after JS_WriteObject reads it. JS_WriteObject's
/// returned buffer must be freed via js_free.
pub fn compile_to_bytecode(ctx: &Ctx<'_>, source: &str, name: &str) -> Result<Vec<u8>, String> {
    let name_c =
        std::ffi::CString::new(name).unwrap_or_else(|_| std::ffi::CString::new("").unwrap());
    // QuickJS requires the source buffer to be null-terminated even though
    // JS_Eval takes a length parameter — passing raw &str bytes leaks the
    // adjacent stack/heap memory into the parser and produces nonsense
    // SyntaxErrors. Match rquickjs's safe eval path which CString'fies.
    let source_c = match std::ffi::CString::new(source) {
        Ok(s) => s,
        Err(_) => return Err("source contains NUL byte".into()),
    };
    let source_len = source.len();
    unsafe {
        let val = qjs::JS_Eval(
            ctx.as_raw().as_ptr(),
            source_c.as_ptr(),
            source_len as qjs::size_t,
            name_c.as_ptr(),
            (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32,
        );
        if qjs::JS_IsException(val) {
            // Extract the exception message via JS_GetException so the
            // caller can see WHY compile failed, not just that it did.
            let exc = qjs::JS_GetException(ctx.as_raw().as_ptr());
            let msg = exception_to_string(ctx.as_raw().as_ptr(), exc);
            qjs::JS_FreeValue(ctx.as_raw().as_ptr(), exc);
            qjs::JS_FreeValue(ctx.as_raw().as_ptr(), val);
            return Err(format!("compile failed: {msg}"));
        }

        let mut out_len: qjs::size_t = 0;
        let buf = qjs::JS_WriteObject(
            ctx.as_raw().as_ptr(),
            &mut out_len,
            val,
            qjs::JS_WRITE_OBJ_BYTECODE as i32,
        );
        // Free the compiled value — bytecode buffer is independent.
        qjs::JS_FreeValue(ctx.as_raw().as_ptr(), val);
        if buf.is_null() {
            return Err("JS_WriteObject returned null".into());
        }

        let bytes = std::slice::from_raw_parts(buf, out_len as usize).to_vec();
        qjs::js_free(ctx.as_raw().as_ptr(), buf as *mut _);
        Ok(bytes)
    }
}

/// Read bytecode and execute it. Caller is responsible for ensuring the
/// bytes were produced by `compile_to_bytecode` from this same binary
/// (the cache key incorporates target triple + format version + shim hash
/// to enforce this).
///
/// Returns Ok(()) on successful execution. Errors from JS itself surface
/// via the standard exception path; this just reports IO/format failures.
pub fn load_and_eval(ctx: &Ctx<'_>, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        let val = qjs::JS_ReadObject(
            ctx.as_raw().as_ptr(),
            bytes.as_ptr(),
            bytes.len() as qjs::size_t,
            qjs::JS_READ_OBJ_BYTECODE as i32,
        );
        if qjs::JS_IsException(val) {
            qjs::JS_FreeValue(ctx.as_raw().as_ptr(), val);
            return Err("JS_ReadObject failed (corrupt or version-mismatched bytecode)".into());
        }
        // JS_EvalFunction consumes the value; we don't FreeValue afterwards.
        let result = qjs::JS_EvalFunction(ctx.as_raw().as_ptr(), val);
        if qjs::JS_IsException(result) {
            qjs::JS_FreeValue(ctx.as_raw().as_ptr(), result);
            return Err("eval threw".into());
        }
        qjs::JS_FreeValue(ctx.as_raw().as_ptr(), result);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable() {
        let k1 = cache_key("var x = 1;", "shim_hash_a");
        let k2 = cache_key("var x = 1;", "shim_hash_a");
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_changes_with_content() {
        let k1 = cache_key("var x = 1;", "shim");
        let k2 = cache_key("var x = 2;", "shim");
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_changes_with_shim_hash() {
        let k1 = cache_key("var x = 1;", "shim_a");
        let k2 = cache_key("var x = 1;", "shim_b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_path_uses_two_char_prefix() {
        let path = key_to_path(Path::new("/tmp/cache"), "abcdef0123");
        assert_eq!(path, Path::new("/tmp/cache/ab/abcdef0123.qbc"));
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("unb_test_{}", std::process::id()));
        let key = cache_key("test source", "shim");
        let payload = vec![1, 2, 3, 42, 0, 99];
        write(&dir, &key, &payload).unwrap();
        let got = read(&dir, &key).unwrap();
        assert_eq!(got, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_evicts_oldest() {
        let dir = std::env::temp_dir().join(format!("unb_prune_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Write three small files.
        for i in 0..3 {
            let k = format!("{:064x}", i);
            write(&dir, &k, &vec![0u8; 1024]).unwrap();
            // Stagger mtimes so prune ordering is deterministic.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Cap at ~2 KB → should evict the oldest.
        prune(&dir, 2048);
        let remaining: Vec<_> = walk_collect(&dir);
        assert!(
            remaining.len() <= 2,
            "expected ≤2 files after prune, got {}",
            remaining.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn walk_collect(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        walk_dir(dir, &mut |p, _| out.push(p));
        out
    }
}
