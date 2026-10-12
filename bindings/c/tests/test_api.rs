#![cfg(all(feature = "sqlite_test_api", not(target_os = "windows")))]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;

#[test]
fn pending_byte_controls_reserved_pages_on_disk() {
    let library_path = std::env::var("TURSO_TEST_API_LIBRARY")
        .expect("TURSO_TEST_API_LIBRARY must name the freshly built test-enabled cdylib");
    let api = unsafe { Api::load(&library_path) };
    let original = unsafe { (api.pending_byte_get)() };
    let restore = PendingByteRestore {
        api: &api,
        original,
    };
    let directory = tempfile::tempdir().expect("create database directory");

    for pending_byte in [65_536, 1_073_741_824] {
        unsafe { (api.pending_byte_set)(pending_byte) };
        assert_eq!(unsafe { (api.pending_byte_get)() }, pending_byte);
        let path = directory.path().join(format!("pending-{pending_byte}.db"));
        {
            let database = Database::open(&api, &path);
            database.exec("PRAGMA page_size=4096; CREATE TABLE payloads(payload BLOB);");
            database.exec("BEGIN;");
            let insert = format!("INSERT INTO payloads VALUES(X'{}');", "ab".repeat(4096));
            for _ in 0..64 {
                database.exec(&insert);
            }
            database.exec("COMMIT; PRAGMA wal_checkpoint(TRUNCATE);");
        }

        let bytes = std::fs::read(&path).expect("read checkpointed database");
        assert_eq!(&bytes[..16], b"SQLite format 3\0");
        assert_eq!(u16::from_be_bytes([bytes[16], bytes[17]]), 4096);
        let page_count = u32::from_be_bytes(bytes[28..32].try_into().expect("page count"));
        assert_eq!(bytes.len(), page_count as usize * 4096);
        assert!(page_count > 17, "database must cross the reserved page");
        let page_17 = &bytes[65_536..69_632];
        assert_eq!(
            page_17.iter().all(|byte| *byte == 0),
            pending_byte == 65_536,
            "page 17 must be unused only when the pending byte is at 64 KiB"
        );

        let database = Database::open(&api, &path);
        assert_eq!(
            database.query("SELECT count(*), sum(length(payload)) FROM payloads;"),
            vec![vec!["64".to_owned(), "262144".to_owned()]]
        );
        assert_eq!(
            database.query("PRAGMA integrity_check;"),
            vec![vec!["ok".to_owned()]]
        );
    }

    drop(restore);
    assert_eq!(unsafe { (api.pending_byte_get)() }, original);
}

struct PendingByteRestore<'api> {
    api: &'api Api,
    original: u32,
}

impl Drop for PendingByteRestore<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.pending_byte_set)(self.original) };
    }
}

struct Library(*mut c_void);

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}

type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

struct Api {
    _library: Library,
    pending_byte_get: unsafe extern "C" fn() -> u32,
    pending_byte_set: unsafe extern "C" fn(u32),
    open: unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int,
    close: unsafe extern "C" fn(*mut c_void) -> c_int,
    exec: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        ExecCallback,
        *mut c_void,
        *mut *mut c_char,
    ) -> c_int,
    errmsg: unsafe extern "C" fn(*mut c_void) -> *const c_char,
}

impl Api {
    unsafe fn load(path: &str) -> Self {
        let path = CString::new(path).expect("library path without NUL");
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(
            !handle.is_null(),
            "dlopen failed: {:?}",
            CStr::from_ptr(libc::dlerror())
        );
        let library = Library(handle);
        macro_rules! symbol {
            ($name:literal, $signature:ty) => {{
                let name = CString::new($name).expect("symbol without NUL");
                let address = libc::dlsym(library.0, name.as_ptr());
                assert!(!address.is_null(), "missing public C symbol {}", $name);
                std::mem::transmute::<*mut c_void, $signature>(address)
            }};
        }
        Self {
            pending_byte_get: symbol!("turso_test_pending_byte_get", unsafe extern "C" fn() -> u32),
            pending_byte_set: symbol!("turso_test_pending_byte_set", unsafe extern "C" fn(u32)),
            open: symbol!(
                "sqlite3_open",
                unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int
            ),
            close: symbol!("sqlite3_close", unsafe extern "C" fn(*mut c_void) -> c_int),
            exec: symbol!(
                "sqlite3_exec",
                unsafe extern "C" fn(
                    *mut c_void,
                    *const c_char,
                    ExecCallback,
                    *mut c_void,
                    *mut *mut c_char,
                ) -> c_int
            ),
            errmsg: symbol!(
                "sqlite3_errmsg",
                unsafe extern "C" fn(*mut c_void) -> *const c_char
            ),
            _library: library,
        }
    }
}

struct Database<'api> {
    api: &'api Api,
    handle: *mut c_void,
}

impl<'api> Database<'api> {
    fn open(api: &'api Api, path: &std::path::Path) -> Self {
        let path = CString::new(path.to_str().expect("UTF-8 database path"))
            .expect("database path without NUL");
        let mut database = Self {
            api,
            handle: ptr::null_mut(),
        };
        let result = unsafe { (api.open)(path.as_ptr(), &mut database.handle) };
        assert_eq!(result, 0, "sqlite3_open failed");
        database
    }

    fn exec(&self, sql: &str) {
        self.query(sql);
    }

    fn query(&self, sql: &str) -> Vec<Vec<String>> {
        let sql = CString::new(sql).expect("SQL without NUL");
        let mut rows: Vec<Vec<String>> = Vec::new();
        let result = unsafe {
            (self.api.exec)(
                self.handle,
                sql.as_ptr(),
                Some(collect_rows),
                ptr::from_mut(&mut rows).cast(),
                ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "sqlite3_exec failed: {:?}", unsafe {
            CStr::from_ptr((self.api.errmsg)(self.handle))
        });
        rows
    }
}

impl Drop for Database<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            let result = unsafe { (self.api.close)(self.handle) };
            assert_eq!(result, 0, "sqlite3_close failed");
        }
    }
}

unsafe extern "C" fn collect_rows(
    context: *mut c_void,
    column_count: c_int,
    values: *mut *mut c_char,
    _names: *mut *mut c_char,
) -> c_int {
    let rows = &mut *context.cast::<Vec<Vec<String>>>();
    let values = std::slice::from_raw_parts(values, column_count as usize);
    rows.push(
        values
            .iter()
            .map(|value| {
                if value.is_null() {
                    "NULL".to_owned()
                } else {
                    CStr::from_ptr(*value).to_string_lossy().into_owned()
                }
            })
            .collect(),
    );
    0
}
