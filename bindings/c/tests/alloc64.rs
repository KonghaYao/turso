#![cfg(not(target_os = "windows"))]

use std::ffi::{c_int, c_void, CStr, CString};

#[test]
fn allocator_rejects_oversized_requests_without_truncating() {
    let path = std::env::var("TURSO_ALLOC64_LIBRARY")
        .expect("TURSO_ALLOC64_LIBRARY must name the freshly built cdylib");
    let path = CString::new(path).expect("library path without NUL");
    unsafe {
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
        let malloc64 = symbol!("sqlite3_malloc64", unsafe extern "C" fn(u64) -> *mut c_void);
        let malloc = symbol!("sqlite3_malloc", unsafe extern "C" fn(c_int) -> *mut c_void);
        let free = symbol!("sqlite3_free", unsafe extern "C" fn(*mut c_void));

        for size in [1, 16, 128] {
            for allocation in [malloc64(size), malloc(size as c_int)] {
                assert!(!allocation.is_null(), "small allocation of {size} bytes");
                let bytes = std::slice::from_raw_parts_mut(allocation.cast::<u8>(), size as usize);
                bytes.fill(0xab);
                let matches = bytes.iter().all(|byte| *byte == 0xab);
                free(allocation);
                assert!(matches, "small allocation must support writes and reads");
            }
        }

        for size in [0, u64::MAX, 0x1_0000_0010, 2_147_483_392] {
            let allocation = malloc64(size);
            let rejected = allocation.is_null();
            free(allocation);
            assert!(rejected, "sqlite3_malloc64({size}) must return NULL");
        }
        for size in [0, -1, c_int::MIN, 2_147_483_392] {
            let allocation = malloc(size);
            let rejected = allocation.is_null();
            free(allocation);
            assert!(rejected, "sqlite3_malloc({size}) must return NULL");
        }
        free(std::ptr::null_mut());
    }
}

struct Library(*mut c_void);

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}
